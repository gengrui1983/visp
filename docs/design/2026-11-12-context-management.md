# 上下文管理重构设计（2026-11-12）

## 1. 背景

实际使用中观察到三个问题：

1. **长任务遗忘 + 重复工具调用**：几十轮工具调用后，`DefaultContextTrimmer`
   的做法是“HEAD + 保留最近 TAIL + 直接丢弃 MIDDLE”。被丢弃的中间轮次里可能
   包含已确认的事实、做过的查询和失败尝试，agent 因此重新执行同样的工具，
   最后被 doom-loop 检测终止，始终给不出结论。
2. **token 估算系统性偏低**：旧实现是 `bytes / 4`。CJK 文本（UTF-8 每字 3 字节）
   实际约 1–1.5 token/字，却按 0.75 token/字估算；JSON/代码标点也不适合
   4 字符/token。实际观测到 32k 本地模型上估算 12k、服务端计数 22k，请求
   直接 400，会话失败退出。此外工具定义（schema 常有数千 token）完全没计入
   预算。
3. **同一套逻辑要服务 32k 本地模型和 1M 云端模型**：不能按模型名硬编码策略，
   必须只依赖 `max_context_tokens` / `max_tokens` 和 provider 上报的真实 usage
   自适应。

## 2. 目标与非目标

目标：

- 估算更准（保守 + 用真实 usage 校准）。
- 接近上限时**压缩**而非直接丢弃，保留目标、已确认事实、已做工作、死路和
  下一步。
- 服务端返回上下文超长时自救重试，不终止会话。
- 关键信息持久化，压缩后仍可查询。
- 行为可配置，有合理默认值，不依赖模型名称。
- 关键行为有测试。

非目标：

- 不引入外部向量数据库或 embedding 服务。
- 不做 LLM 二次摘要调用（会增加成本、延迟和失败面）。
- 不替代 provider 自身的分词器；估算允许误差，由校准与安全边际兜底。

## 3. 总体流程

```
history
  │
  ├─ token 估算（字符类别 + 消息/工具/图片开销 + 会话校准）
  ├─ 预算 = (窗口 − 输出预留 − system/工具开销) × (1 − 安全边际) ÷ 校准
  │
  ├─ 未达 trigger(默认 80%)
  │    ├─ 有上次压缩检查点 ──► 复用压缩结果 + 追加新消息（保持前缀稳定）
  │    └─ 否则 ─────────────► 原样返回
  │
  └─ 达到 trigger
       ├─ 工具输出首尾截断（保留日志尾部错误）
       ├─ 按「用户消息 / assistant 工具步骤」切分轮次
       ├─ HEAD 轮次（任务锚点） + TAIL 轮次（最近工作流）
       ├─ MIDDLE 提取结构化记忆（事实/决定/死路/方向变化/工作日志）
       │    ├─ 持久化到 MemoryStore
       │    └─ 生成压缩摘要消息
       ├─ 目标为 target(默认 45%)，记录检查点
       └─ 仍超预算：减少 TAIL/HEAD、硬截断、最终保留 goal + summary + 最新尾部
```

## 4. Token 计量

### 4.1 基础估算

`visp_core::message::estimate_tokens` 按字符类别估算：

| 类别 | 估算 |
|------|------|
| ASCII 字母/数字 | 4 字符 / token |
| ASCII 标点 | 2 字符 / token（JSON、代码、日志） |
| 非 ASCII（CJK/emoji） | 1 字符 / token |
| 换行 | 1 token |
| 每条消息 | +3 协议开销 |
| 每个 tool call | +4 结构开销 + id/name/arguments |
| 每张图片 | +1024 保守预留 |

消息估算不再迷信持久化的 `estimated_tokens`，构建 prompt 时按内容重算。

### 4.2 工具定义

`estimate_tool_definitions_tokens` 把工具名、描述和 JSON Schema 计入
`system_overhead`。这是旧实现遗漏的一处大头。

### 4.3 会话级校准

provider 每次返回 `UsageInfo.input_tokens` 后：

```
sample = actual_input / estimated_input
calibration = max(previous, sample)   // 只向上校准，下限 1.0，上限 6.0
```

- 不存在模型名判断；不同模型/网关差异完全由数据驱动。
- 校准状态按 **session_id** 隔离，共享 trimmer 不会互相污染。
- 校准值仅用于预算决策，agent loop 计算 sample 时使用未校准的基础估算，
  避免反馈回路。

**为什么不做向下校准。** `UsageInfo.input_tokens` 不是跨 provider 统一的
“prompt 总 token”：

- Anthropic 的 `input_tokens` 只表示未命中缓存的输入；完整 prompt 还要加上
  `cache_creation_input_tokens` 与 `cache_read_input_tokens`。
- OpenAI 的 `prompt_tokens` 已包含 `cached_tokens`，语义又不同。

因此 `sample < 1` 可能只说明“这次请求大量命中缓存”，而不是“估算偏高”。
若据此把 calibration 降到 1.0 以下，预算会按同一个错误比例被放大，正好在
缓存命中的轮次把请求推过窗口上限。当前实现选择：

- `sample < 1` 时保持 calibration 不变（不低于 1.0），把估算偏高的代价限制在
  安全边际内；这是“浪费一点窗口”换“不 400”的保守取舍。
- 基础估算对 CJK 按 1 字/token 是保守值；部分 tokenizer 可能更省，某些非
  ASCII 文字可能更费。校准只修正“低估”这个危险方向。
- 若未来要在不牺牲安全性的前提下回收高估的窗口，正确做法是先在
  `ChatEvent::UsageInfo` 中提供归一化的 prompt 总 token（含 cache read/write），
  再允许双向校准；当前不做，因为需要改 provider 协议与所有 adapter。

### 4.4 预算公式

```
output_reserve = max(max_tokens, min_output_tokens)      // 且 ≤ 窗口 75%
available = max_context_tokens − output_reserve − system/tool overhead
budget = available × (100 − safety_margin_pct) / 100 / calibration
trigger = budget × compact_trigger_pct / 100
target  = budget × compact_target_pct / 100
```

trigger > target 形成滞后区间。trim 每次拿到的是**完整历史**，所以仅靠
trigger/target 数值本身并不能避免每轮重算：历史在压缩后仍然超过 trigger。
实现用一个会话级**压缩检查点**（见 5.5）记住上次的输出；后续轮次把新消息
追加到检查点输出上，只有“压缩结果 + 新增消息”的逻辑大小再次达到 trigger
时才重新压缩。这样压缩后的 prompt 前缀保持稳定，provider 的前缀缓存可以
继续命中。

## 5. 结构化压缩

### 5.1 轮次切分与观察截断

旧实现只按用户消息切分轮次。真实的 agent 任务通常只有开头一条用户消息，
后面几十轮都是 `assistant tool_call → tool result`；这样整段历史会被当成
一个轮次，HEAD/TAIL 无法留下最近工作流，最终退化成 goal + summary。

现在的切分规则：

- 用户消息开启新轮次，紧随其后的第一条 assistant 消息属于该轮次；
- 同一轮用户任务内，后续每条 assistant 消息（即每个工具调用/结论步骤）
  开启新的子轮次，其 tool result 归属该子轮次。

例如 `User, A(call), Tool, A(call), Tool, A(text)` 被切为
`[User, A, Tool] [A, Tool] [A]`。`head_turns` / `tail_turns` 因此对
“单用户 + 多轮工具调用”同样有效。

工具输出超过 `tool_output_max_chars` 时保留 **前 60% + 后 40%**，中间标记
`[N chars truncated]`。日志/编译错误的可操作信息通常在末尾，旧实现的
“只保留前缀”会丢掉根因。

### 5.2 摘要内容

压缩摘要以 `Message::system` 插入被丢弃区间的位置，包含：

1. `## Goal`：首条用户消息（任务锚点）。
2. `## Next step`：最后一条非空 assistant 文本。
3. `## Decisions` / `## Direction changes` / `## Dead ends`：
   从被压缩轮次自动抽取。顺序刻意放在 facts 与工作日志之前，因为这些信息
   丢失代价最高。
4. `## Confirmed facts`：确认的事实。
5. `## Work already done (do not repeat unless files changed)`：去重后的工具
   调用日志，格式 `name(args) (xN) -> ok/error: ...`。重复调用只出现一次并
   标注次数，直接抑制“重复查询”。
6. 已持久化的记忆条目（含 agent 主动记录的内容）。

摘要预算不是简单地“按顺序拼完再截断”。Goal / Next step 之后，剩余的
`max_summary_tokens` 按**存在的分组**平分：决定类、facts、工作日志各拿到
一份预算，分组内部再按行数/预算裁剪。否则一个 20 行的工具日志会把排在
后面的 confirmed facts 挤出摘要（这是审查中发现的真实问题）。

### 5.3 记忆抽取启发式

无额外 LLM 调用，按关键词与结构分类（中英双语）：

- 用户消息 → Fact（新约束/要求）。
- assistant 文本按句子扫描：`failed/cannot/走不通/失败` → DeadEnd；
  `instead/改为/转向` → Pivot；`decided/决定/采用` → Decision；
  `confirmed/found/确认/根因` → Fact。
- 工具调用/结果配对：成功 → Work；失败 → DeadEnd。
- 每类最多注入 12 条，单条 ≤240 字符。
- 抽取总数超过 80 条时，优先保留 Fact/Decision/DeadEnd/Pivot 等非 Work
  条目，只用剩余名额保留最近的 Work 条目；工作日志本身会覆盖工具调用。

启发式不完美，但零成本、确定性、可测试；agent 也可用 `memory` 工具补充
更精确的记录。

### 5.4 极端收缩

多级回退，保证不超 target：

1. 减少 TAIL 轮数到 1。
2. 减少 HEAD 轮数到 1。
3. 对保留消息做硬截断。
4. 轮次太少、无法形成 MIDDLE 时（例如只有一条用户消息 + 一条超长 assistant
   文本），保留 goal 锚点，对锚点之后的全部内容抽取记忆生成摘要，再从尾部
   追加能放下的最新消息。
5. 最终只保留 goal + summary + 最新 user 消息，并继续按 target 截断。预算为
   0 是退化输入（窗口完全被输出预留和 system 开销占满），此时优先保留 goal
   锚点而不是返回空 prompt。

每一步都执行 `repair_tool_pairs`：移除没有对应 tool result 的 assistant
tool_calls，以及孤立的 tool result，避免 OpenAI/Anthropic 的 400。

### 5.5 滞后区间与会话检查点

`SessionState` 保存最近一次压缩的检查点：

- `input_len` / `input_estimate`：生成该结果的完整历史前缀长度与基础估算，
  用于确认历史仍是“只追加”的（前缀被改写或历史变短则丢弃检查点）。
- `output`：压缩后的消息（`Arc<Vec<Message>>`，clone 成本低）。
- `output_estimate`：压缩结果的基础估算。

下一轮 `trim` 的判断是：

```
logical = checkpoint.output_estimate + estimate(history[checkpoint.input_len..])
if logical <= trigger: 返回 checkpoint.output + 新消息
else:                  从完整历史重新压缩并替换检查点
```

代价与边界：

- 检查点属于会话级状态，随 `SessionStates` 的 256 个会话上限淘汰；每个会话
  最多缓存一份压缩结果。
- 若 agent loop 传入的历史不是对上次前缀的追加（会话分叉、历史被编辑），
  前缀校验失败后会立即丢弃检查点并重新压缩。
- 服务端报超长（`force_compact`）时无条件丢弃检查点，按 pressure 重新压缩。
- 好处是压缩后的 prompt 前缀在多轮之间保持字节稳定，避免每轮重写摘要导致
  前缀缓存全部失效。

## 6. 持久任务记忆

### 6.1 数据模型

`visp_core::memory`：

- `MemoryKind`：`goal` / `fact` / `decision` / `dead_end` / `pivot` / `work` / `next`
- `MemoryStore`：`append`（幂等）、`search`（大小写不敏感子串）、`list`
  （按时间正序）、`count`
- `InMemoryMemoryStore`：测试、`storage.driver = "memory"` 或持久化降级

`visp-db::SqliteMemoryStore` 使用 SQLite 表：

```sql
CREATE TABLE context_memory (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  content TEXT NOT NULL,
  source TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE(session_id, kind, content)
);
```

### 6.2 为什么选 SQLite

| 方案 | 优点 | 缺点 | 结论 |
|------|------|------|------|
| **SQLite（同会话库）** | 已有依赖；事务/外键级联删除；可 SQL 检索；随会话备份 | 需要 schema migration；子串检索非语义 | **采用** |
| JSONL 文件 | 实现最简单；易追加 | 并发追加/轮转/损坏恢复要自己处理；查询需全量扫描；与会话生命周期不一致 | 放弃 |
| 纯内存 | 零 IO | 重启丢失，不满足“持久” | 仅作降级 |
| 外部 KV/Redis | 快 | 新增服务依赖，部署复杂 | 放弃 |
| 向量数据库 | 语义检索 | 需 embedding 模型/服务；成本与复杂度高；记忆条目小且结构化，关键词足够 | 放弃 |
| LLM 摘要后存文本 | 摘要质量高 | 额外调用、延迟、失败重试；关键事实可能被改写 | 放弃（启发式 + agent 工具） |

记忆与会话/消息共用同一个数据库文件但使用独立连接（WAL），通过
`ON DELETE CASCADE` 随会话删除。

### 6.3 读写路径

- **自动写**：压缩 MIDDLE 时，`extract_notes` 生成条目并 `append`；相同
  `(session_id, kind, content)` 幂等，重复压缩不会膨胀。
- **自动读**：每次压缩生成摘要时 `list` 最近 `max_memory_entries` 条并合并。
- **主动写/读**：新增 `memory` 工具（`action=record|search|list`），agent
  可记录精确决定或压缩后检索历史。
- **持久化失败**：记录 warning 并继续压缩，不阻塞对话（降级而不是失败）。

## 7. 服务端上下文超长自救

`LlmError::is_context_length_exceeded` 仅对 4xx（400/413/422）+ 关键词判断，
覆盖 OpenAI/vLLM、Anthropic、网关和中文错误信息，例如：

- `context length` / `context_length_exceeded`
- `maximum context length`
- `prompt is too long`
- `too many tokens` / `reduce the length`
- `request entity too large`
- `上下文长度` / `输入过长`

agent loop 在同一次 iteration 内：

1. 第一次调用返回该错误。
2. `on_context_overflow(session)`：`pressure *= 0.7`（下限 0.25）、
   `overflow_level += 1`、`force_compact = true`。
3. 下一次 `trim` 无视 trigger 强制压缩（`force_compact` 只生效一次，
   避免 provider 不上报 usage 时每轮都压缩）。
4. 重新 `setup_iteration`（重建 prompt + 工具定义），重试同一次调用。
5. 最多 `agent.context.overflow_retries` 次（默认 2）；耗尽后才走原有错误路径。
6. 成功拿到 usage 后清空 pressure/overflow 状态，校准系数继续生效。

同时发送 `StatusUpdate` 事件，让 CLI 显示“已压缩历史并重试”，而不是静默。

已知边界：如果超长错误发生在**流式响应中间**（而非请求阶段），当前仍按
stream error 结束。多数 provider 的超长校验发生在请求头/体阶段，此边界
在下方“已知局限”中说明。

## 8. 配置项

`daemon.toml`：

```toml
[agent.context]
enabled = true                 # false = 退回 NoopTrimmer
memory_enabled = true          # 是否启用持久记忆 + memory 工具
head_turns = 2                 # 保护的开头轮数（任务锚点）
tail_turns = 6                 # 保护的末尾轮数（最近工作流）
compact_trigger_pct = 80       # 达到预算百分比触发压缩
compact_target_pct = 45        # 压缩目标，形成滞后区间
safety_margin_pct = 12         # 额外安全边际
tool_output_max_chars = 8000   # 单条工具输出保留字符数
min_output_tokens = 2000       # 输出 token 最小预留
max_summary_tokens = 1200      # 压缩摘要最大 token
max_memory_entries = 80        # 注入摘要/检索的最大记忆条数
overflow_retries = 2           # 服务端超长自救重试次数
```

全部有默认值；旧配置无需修改即可获得新行为。策略不读取模型名称。

## 9. 代码结构

| 文件 | 职责 |
|------|------|
| `crates/visp-core/src/message.rs` | 字符类别 token 估算、消息/工具/图片开销 |
| `crates/visp-core/src/error.rs` | `is_context_length_exceeded` |
| `crates/visp-core/src/context.rs` | 扩展 `ContextTrimmer`：session 维度、校准、溢出压力（全部默认方法，兼容旧实现） |
| `crates/visp-core/src/memory.rs` | 记忆类型、存储 trait、内存实现 |
| `crates/visp-core/src/prompt.rs` | `build_for_session` + 工具开销 |
| `crates/visp-core/src/agent_loop.rs` | usage 校准、400 自救重试、状态事件 |
| `crates/visp-context/src/lib.rs` | 自适应预算、压缩主流程、会话状态 |
| `crates/visp-context/src/compaction.rs` | 观察截断、记忆抽取、工作日志、摘要生成、tool pair 修复 |
| `crates/visp-db/src/schema.rs` | v7 migration：`context_memory` 表与索引 |
| `crates/visp-db/src/memory_repo.rs` | `SqliteMemoryStore` |
| `crates/visp-tools/src/memory.rs` | `memory` 工具 |
| `crates/visp-daemon/src/main.rs` | 创建存储/trimmer、注册工具、配置映射 |

## 10. 测试方法

测试先于实现编写，并确认在改动前失败（编译失败或断言失败），再实现：

- `visp-core::message_tests`
  - CJK 不得按 bytes/4 低估。
  - 标点密集 JSON 估算更保守。
  - 图片有 token 预留。
- `visp-core::error_tests`
  - 常见中英文/各状态码超长文案被识别。
  - 401、tool_calls 格式错误、429、5xx 不误判。
- `visp-core::agent_loop_tests`
  - 400 超长 → 压缩 → 同轮重试成功，且不产生 `Error` 事件。
  - `overflow_retries = 0` 时仍走原错误路径。
- `visp-context::lib_tests`
  - **真实形态**：单条用户消息 + 40 轮 assistant/tool（含失败查询与中途确认
    的事实）压缩后，保留最近工具轮次、`:8765`/时间戳等事实、失败查询和
    work log；记忆库有对应条目；无孤儿 tool pair；结果在 target 内。
  - 滞后区间：追加一轮新消息时复用压缩前缀（结果以旧输出为前缀）；超过
    trigger 后才重新压缩。
  - 轮次切分：无用户消息的连续 assistant/tool 步骤也能被识别为独立轮次。
  - 校准按 session 生效且只向上；`sample < 1` 不会放大预算。
  - 溢出压力 + `force_compact` 立即触发压缩并丢弃检查点。
  - 摘要包含 goal/facts/decisions/work log/dead ends/next step。
  - 工具调用去重（xN）。
  - 压缩后无孤儿 tool call/result。
  - 结果满足 target 预算。
  - 极低预算仍保留任务锚点。
  - 记忆持久化后可检索；agent 预存记忆会进入下次摘要。
  - 观察截断保留尾部错误。
- `visp-core::memory` / `visp-db::memory_repo_tests`
  - 追加/检索/列表/计数、幂等、会话隔离、LIKE 通配符转义、级联删除。
- `visp-tools::memory_tests`
  - record/search/list、缺失 session、空 content、未知 action、未知 kind 回退。
- `visp-config::tests_context_section`
  - 默认值、完整配置、部分配置保留其他默认。

验证命令：

```bash
env -u VISP_CONFIG_DIR cargo test -p visp-core -p visp-context -p visp-db \
  -p visp-tools -p visp-config
env -u VISP_CONFIG_DIR cargo test -p visp-agent -p visp-daemon
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

> 注：本容器环境预置了 `VISP_CONFIG_DIR=/cfg`，会导致 `visp-config` 的
> 路径测试按 /cfg 解析。测试时用 `env -u VISP_CONFIG_DIR` 还原测试假设；
> 这与本次改动无关。

## 11. 已知局限

1. **估算不是分词器**：字符类别启发式仍有误差；首轮真实 usage 返回前只能靠
   安全边际。校准只在同一 session 内有效，不跨重启/跨会话持久化。
2. **usage 语义差异 / 不做向下校准**：Anthropic 的 `input_tokens` 不含
   cache read/write，OpenAI 的 `prompt_tokens` 又包含 cached tokens，二者不是
   同一个量。当前校准只向上，避免把“缓存命中”误判成“估算偏高”而放大预算。
   代价是估算偏高时（某些非 ASCII 文字、tokenizer 更省的模型）会浪费一部分
   窗口，浪费比例约等于高估比例，且不会被自动收回。要安全回收这部分窗口，
   需要先在 `UsageInfo` 中归一化 prompt 总 token 再允许向下校准。
3. **流式中途超长**：只处理请求阶段返回的 4xx；若错误在 stream 中途才出现，
   当前仍按流错误结束。多数 provider 不会这样返回。
4. **启发式摘要可能遗漏**：关键词抽取不保证覆盖所有重要语句；agent 可通过
   `memory` 工具主动记录/检索来弥补。故意不调用 LLM 摘要以控制成本与确定性。
5. **记忆检索是子串匹配**，不是语义检索；条目很多时 `LIKE` 会线性扫描。
   会话级记忆通常只有几十到几百条，暂不构成问题。
6. **图片 token 是固定预留**（1024/张），未按分辨率计算；对高分辨率多图请求
   可能仍偏保守。
7. **SQLite 独立连接**：与会话库共用文件但非同一连接，极端并发下依赖 WAL +
   busy_timeout；当前写入只发生在压缩时，频率低。

## 12. 后续可做

- 持久化每个 `(provider, model, tokenizer)` 的校准系数，跨会话冷启动更准。
- 可选接入 provider 的 tokenizer（如 tiktoken）作为高精度模式。
- 记忆检索增加 FTS5 或简单 BM25 排序。
- 流式错误也纳入 overflow 自救（需要重构 `collect_stream_events` 的返回类型）。
