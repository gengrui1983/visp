# visp 工具调用 token 效率优化 — 设计文档

> 日期：2026-10-05
> 参照：`docs/learn/opencode-token-efficiency/`（00 总览、01 system prompt、02 工具目录与 CodeMode、03 指令增量、codemode-explained）
> 状态：待审核
> 范围：工具暴露形态、工具指南注入、工具输出回传、工具调用观测。**不含**：压缩机制（总览 D）、缓存断点策略重构（总览 E，仅做兼容性约束）、历史读取（总览 F）

---

## 0. 摘要

opencode 2.0 的"工具调用省 token"由四项机制叠加：**B 工具目录收拢（CodeMode）**、
**A 中工具指南条件化瘦身**、**C 目录增量下发**、**G 工具输出治理**。visp 全盘照搬会过度工程，
本设计按收益/成本分为三期：

| 阶段 | 内容 | 对应机制 | 预期收益 | 风险 |
|---|---|---|---|---|
| P0 | 指南瘦身去重、skill 描述封顶、输出治理统一、per-tool 观测埋点 | A(部分)+G | 每轮省约 1–2K tokens；输出可恢复；获得决策数据 | 低 |
| P1 | 会话指令基线 + keyed 工具目录增量下发；OpenAI 缓存稳定化 | C | 长会话与工具集变化场景防缓存失效；为规则迁移打地基 | 中 |
| P2（可选） | CodeMode：长尾工具收拢进单个 `execute` + 受限 JS 运行时 | B | 回合数减少；中间结果不进历史（opencode 实测最大头） | 高 |

P0/P1 先落地并用 P0-d 的观测数据验证收益；P2 是否启动由决策门槛数据决定。

---

## 1. 背景与目标

### 1.1 现状问题盘点（基于代码勘察）

| # | 问题 | 位置 | 影响 |
|---|---|---|---|
| P1 | 工具指南每轮重算重发，且 codegraph 3 个工具的**完整描述**在指南与 tools schema 中各出现一次（双份） | `visp-core/src/agent.rs:722-801`（`render_tool_guide`）、`visp-daemon/src/agent_loop.rs:275-280`、`:304` | 每轮多付约 1.5K–2.5K chars 的重复描述；system 字节构成不必要地大 |
| P2 | `skill` 工具描述**动态拼接全部 skill 列表，长度不封顶**：skill 数量线性膨胀，且会话中途 skill 集变化 → tools 字节变化 → provider 前缀缓存失效 | `visp-tools/src/skill.rs:19-33,48` | 工具多时 schema 膨胀数千 token；缓存断点被打破 |
| P3 | AgentTool 描述 = agent 全文 description，随子 agent 数量线性增长 | `visp-core/src/agent.rs:275-367` | 同上（轻） |
| P4 | 输出治理两层截断不一致且不可恢复：工具层 100KB 直接弃尾，上下文层 8000 字符再截；无"落盘 + 引用路径"、无空输出归一、无截断元数据 | `visp-tools/src/truncate.rs:2`（100KB）、`visp-context/src/lib.rs:7,138-145`（8000 chars） | 长输出场景模型拿不到截断部分（只能盲改 offset 重读）；截断行为对模型不可见（无元数据）；噪声空串 |
| P5 | 无 per-tool token/调用计量，工具层消耗只能靠整体 usage 间接推断 | `visp-daemon/src/observability/metrics_layer.rs` | 无法评估优化收益、无法为 P2 提供决策数据 |
| P6 | OpenAI 路径无缓存主动控制（Anthropic 已有 `cache_control`）；指南与 tools 字节稳定性无约束 | `visp-llm/src/openai.rs`、`anthropic.rs:84-105` | OpenAI 自动前缀缓存命不命中全看运气 |

### 1.2 目标

1. 每轮请求的 system + tools 字节：**确定性、可缓存、最小化**（同一会话内无变化时逐字节相同）
2. 工具相关信息**单点披露**：描述/签名只出现一次，指南只承载策略规则
3. 工具输出**统一治理**：预算封顶、截断可恢复、噪声归一
4. 建立**工具级 token 观测**，用数据决定 P2 是否投入
5. 长会话 / 工具集变化场景下缓存尽可能命中（delta 化）

### 1.3 非目标

- 不迁移规则文件（rules）到增量体系（留给 P1 地基的后续复用）
- 不改压缩（compaction）触发与预算策略
- 不改子 agent 工具过滤原则（`filter_tools_for_sub_agent` 语义保留）

---

## 2. 参照机制 → visp 映射

| opencode 机制 | 要点 | 本设计去向 |
|---|---|---|
| A：756B 基座 + 按工具条件插 4 行 | 工具策略整段长文 → 条件短行 | P0-a：`render_tool_guide` 瘦身与条件化 |
| B：12 核心直连 + 1 `execute` + 预算目录 + `search` 兜底 | schema 从 N 份变 1 份 | P2（需 JS 运行时）；P0-b 先做"单工具描述封顶"的降级版 |
| C：keyed source + sha256 + epoch 基线 + delta system 消息 | 稳定可缓存，变化只发 delta | P1：以工具目录为首个 source 建 Instructions 体系 |
| G：统一截断常量（50KiB/2000 行）+ 落盘 + 引用句 + 空输出归一 | 截断可恢复 | P0-c |

---

## 3. 总体架构：三层治理

```
┌─────────────────────────────────────────────────────┐
│ 暴露层（每轮随请求发送，要求字节冻结）                 │
│   tools schema（native 直连，预算化描述）              │
│   工具指南（策略短行，条件渲染，去重）                  │
│   会话指令基线（P1：目录/环境等 keyed source 冻结文本） │
├─────────────────────────────────────────────────────┤
│ 执行层（工具调用 → 结果）                              │
│   并行执行（既有，保留）                               │
│   输出治理：预算 → 落盘 → 引用/元数据/归一（P0-c）      │
│   P2: execute 运行时编排子工具调用（走既有权限链）       │
├─────────────────────────────────────────────────────┤
│ 观测层                                                │
│   per-call 计量：tokens(估)/bytes/时长/成败 → metrics  │
│   聚合视图 → P2 决策门槛                               │
└─────────────────────────────────────────────────────┘
```

---

## 4. 各期设计

### 4.1 P0-a 工具指南瘦身与条件化

**职责**：`render_tool_guide` 重构为确定性渲染器。

- 输入 = 本请求实际下发的工具名集合 + 分类信息；输出 = 稳定文本。
- **去重原则**：工具的参数/用法信息只在 schema 中出现一次；指南不再复述任何工具的完整描述（codegraph 三段描述从指南移除）。
- 指南只保留两类内容：
  1. 分类清单：分类名 + 工具名列表（一行文字清单，不展开描述）
  2. 条件化策略短行（对齐 opencode 的 4 行模式，仅在对应工具存在时插入）：
     - 有 `bash` → 2 行（优先专用文件工具；禁止分隔符串联多条命令）
     - 有 `write_file` → 1 行（新文件/整替用 write，局部修改用 edit）
     - 有 `edit_file` → 1 行（old/new 精确匹配 + 唯一性约束）
     - 常规执行原则 2–3 行（并行独立调用、失败重试提示等，合并现有 IMPORTANT 大段的**必要最小集**）
- **确定性约束**：指南中不得出现时间戳、计数器等易变量；同一输入字节级相同。
- 配套 golden-file 快照测试：固定工具集 → 断言指南文本逐字节稳定。

预期：指南从当前 ~2–3K chars 降至 ~600–1000 chars，且消除 codegraph 双份重复。

### 4.2 P0-b 单工具描述预算化（skill / AgentTool）

**职责**：给"动态拼装描述"的工具加预算封顶，防止 schema 无界膨胀。

- **skill 工具**：描述改为「工具用法一句话 + skill 目录清单」，清单只列 **skill 名 + 描述首行**（截 120 字符，对齐 opencode `DESCRIPTION_LIMIT`），总预算封顶（目标 ≤2000 tokens，采用估算系数 4 chars/token）。
- 超预算时的**淘汰策略**：先按最短清单条目优先保留（同 opencode `summarize` 思路：每类别先保一行，再轮转补位），被淘汰的不出现在清单中，提示改为「可用 skill 共 N 个，使用 `skill` 工具的 query 参数按需发现」。
- **skill 工具新增查询用法**：支持按名称/关键词列出一个 skill 的完整说明（发现兜底，替代被预算淘汰的部分）。
- **AgentTool**：描述只保留 agent description 首行 + 用途一句话，超出预算截断（截断尾部加省略元数据）。
- 微调原则：其余静态工具（bash/read/...）描述保持现状，不在本期做文案改写（避免影响既有模型行为习惯），仅在后续评审时统一。

### 4.3 P0-c 工具输出治理（对齐 opencode G 项）

**职责**：把两层互不知情的截断（工具层 100KB / 上下文层 8000 chars）统一为一套治理流程。

**统一常量（提议，可配置化 into daemon.toml）**：

| 常量 | 值（对齐 opencode） | 说明 |
|---|---|---|
| `MAX_BYTES` | 50 KiB | 工具原始输出的保留上限 |
| `MAX_LINES` | 2000 行 | 行数上限（先于字节） |
| `TOOL_OUTPUT_MAX_CHARS`（回传上限） | 8000 chars（维持现状） | 进入对话的部分 |

**流程**：

1. 工具产出后按 `MAX_LINES` → `MAX_BYTES` 顺序裁剪；
2. 未超限：原样回传（字节数追加为尾行元数据）；
3. 超限：**全量原文落盘**到会话目录（`tool-output/`，命名含 call id + 序号），上下文只回传：
   「已保留部分 + 一句截断元数据：`... N bytes truncated; full content saved to <path> ...` + 读取提示」；
4. 空输出归一为固定占位文本（`(no tool output)`）；nil/缺失结果补齐同样占位；
5. 落盘文件保留期：7 天自动清理（复用会话目录生命周期）；
6. 上下文层（`visp-context`）对已治理输出不再二次暴力截断，仅保留兜底（防未接入治理层的新工具）。

**收益点**：截断部分可恢复（模型可用既有 `read_file` 读取引用路径），长输出场景不再"盲区"；截断行为对模型显式可见。

### 4.4 P0-d per-tool 观测埋点

**职责**：为收益验证与 P2 决策门槛提供数据。

- 在工具执行包装层（`ToolRegistry::execute` 外沿）记录每次调用：
  `tool name / args 字节数 / 结果字节数 / 估算 tokens（既有 estimate 系数）/ 时长 / 是否截断落盘 / 成败`；
- 汇总进既有 observability metrics（复用 `UsageInfo` 的聚合路径扩展字段）；
- 导出按会话聚合视图：**每工具的"进上下文字节总量"与"调用次数/回合数"** —— 这是判断 P2 收益的关键指标（聚合型工具调用占比）。

### 4.5 P1 会话指令基线与工具目录增量下发

**职责**：让"会话级稳定文本"冻结为基线，变化走一次性 delta 消息。

**机制设计（对齐 opencode 03 篇，做最小裁剪）**：

- **keyed source 抽象**：每个 source 有稳定 `key`、确定性序列化、`initial/changed/removed` 三种渲染能力；
- **准入**：对 source 值做 sha256，与会话存储的快照比对——哈希未变 → 零输出；
- **基线**：会话首次构建时，把全部 source 的 initial 渲染合并为**指令基线**，作为 system 的一部分随每轮发送（字节冻结，利于缓存）；
- **变化**：工具集/skill 集/环境变化时，渲染 delta 为**一次性 system 类型消息**追加进 transcript（时间序），不再修改 system —— 与 opencode 相同：`system 不动 + 历史多一条小 delta`；
- **持久化**：基线与快照写入 visp-db（会话行 + blob），跨进程重启后可重放；
- **首个 source**：工具目录（skill 清单、AgentTool 清单、MCP 工具清单、环境 cwd/日期）。
  - 日期跨天 → delta 消息 `Today's date is now: ...`（对齐 opencode builtins 行为）；
- **epoch 前移**（简化版）：压缩完成后将 current 提升为新基线（复用现有压缩完成钩子）。

**OpenAI 缓存稳定化（配套）**：
- OpenAI 路径透传 prompt 复用标识（以会话 ID 为 key），保证自动前缀缓存按会话稳定命中；
- 确保 system parts 顺序稳定（基线在前、易变内容不进 system）。

**本期的边界决策**：
- 规则文件（rules）**暂不**迁入该体系（避免系统提示词行为大变），但 source 抽象以可扩展为先决条件；
- MCP 工具的动态上下线是 delta 触发的首批真实场景。

### 4.6 P2 CodeMode（二阶段，决策门槛制）

**职责**：把长尾工具收拢进单个 `execute` 工具 + 受限 JS 运行时，获得"回合数减少 + 中间结果不进历史"两大收益（opencode 实测最大头）。

**分流原则（对齐 opencode 02 §3.1）**：
- **直连**（native tools 列表）：`read_file / write_file / edit_file / grep / glob / bash / fetch_web / skill / AgentTool` + `execute` 本身；
- **收拢进 execute**（codemode 类）：MCP 工具（主体收益来源）、以及频次低的长尾分析工具（如 codegraph 六件套中的 trace/impact 等按观测数据决定）。
- `execute` 保留名，schema 仅 `code: string` 一个字段；目录文本不进 schema/description，走 P1 的 Instructions 增量下发（最大对齐点：P1 是 P2 的直接前置）。

**目录策略**：
- 预算封顶（INLINE_BUDGET≈2000 tokens）、每 namespace 保底一行、超限靠 `search` 兜底（代码模式内建发现函数，返回 path + 签名）。

**JS 运行时选型对比（关键决策）**：

| 方案 | 优点 | 缺点 | 结论 |
|---|---|---|---|
| A. rquickjs（QuickJS 绑定） | 成熟、快、二进制增量小、可冻结全局实现沙箱、有 async 支持 | unsafe FFI 边界需审计；需冻结原型链防逃逸 | **推荐** |
| B. boa（纯 Rust 引擎） | 纯 Rust、无 unsafe 依赖 | 性能慢 1–2 个数量级；兼容性长尾 | 仅当团队否决 C 依赖时 |
| C. 自研 JS 子集解释器（对齐 opencode `@opencode/codemode` 模式） | 语义完全可控、无运行时依赖 | 工程成本极高（opencode 有专职包） | 不建议 |
| D. 无引擎折衷：`batch_invoke` 工具（声明式调用列表） | 零新依赖、实现快 | 无组合/过滤能力，**中间结果仍进历史**（最大收益丢失）；仅省回合 | 备选 |

**强制约束**：
1. 子工具调用**必须复用既有权限/审批链路**（TUI 审批弹窗不因走 CodeMode 而旁路）——运行时只做受限执行，授权决策全部在宿主工具侧；
2. 资源上限必须默认启用：`timeout / maxToolCalls / maxOutputBytes`（opencode core 默认不限是它的选择，visp 默认收紧）；
3. 数据边界：子工具入参/返回按 JSON 语义拷贝穿越，非 JSON 值规则明确（对齐 opencode `Data.toBoundary` 思路）。

**决策门槛（由 P0-d 数据触发）**：满足其一才立项 P2：
- 聚合型场景（单轮内 ≥3 次同族工具调用的任务）在真实使用中占比 ≥ 15%；
- MCP 工具常态接入 ≥ 10 个且 schema 总量 > 4K tokens。
若不满足 → 维持 D 方案（batch_invoke）作为轻量替代，或干脆不做。

**Prompt 配套**：execute 的使用指引（何时写程序 vs 直接调用、`Promise.all` 并发、只返回精炼结果）作为条件短行纳入指南（工具存在才出现）。

---

## 5. 影响范围（crate 职责变更）

| crate | P0 变更 | P1 变更 | P2 变更 |
|---|---|---|---|
| visp-core | `render_tool_guide` 重构（agent.rs）；golden 测试 | keyed source 抽象 + 基线渲染接口（新模块） | execute 工具抽象 + 目录渲染 |
| visp-tools | skill 描述预算化 + query 用法；truncate 统一 + 落盘引用 | skill/Agent 清单迁为 source 数据供给 | MCP/长尾工具分流标记 |
| visp-context | 兜底截断让位于治理层 | 压缩完成后 epoch 前移钩子 | 无 |
| visp-llm | 无 | OpenAI 复用标识透传；Anthropic cache_control 断点位置核对 | provider 侧 execute schema 下发（复用现结构） |
| visp-agent | 无 | 构建 prompt 时合并基线 | 子工具调用路由回 ToolRegistry（权限复用） |
| visp-daemon | 观测埋点接入 metrics | delta 消息产生/投影进 transcript | execute 编排注册 + 资源上限配置 |
| visp-db | 无 | 基线/快照持久化（session 行 + blob 扩展） | 无 |
| visp-tui | 无 | delta system 消息的展示 | CodeMode 进度事件展示 |

---

## 6. 接口规范（数据结构示意）

**工具输出回传格式（含治理元数据）**——文本协议，新增约定行：

```
<正常或截断后的输出>
... [TRUNCATED] N bytes truncated; full content saved to <tool-output path> ...
```

**keyed source（P1）**：

- 字段：`key`（如 `tools/catalog`、`core/environment`）、`value`（确定性序列化）、`render_initial(value)`、`render_changed(prev, cur)`、`render_removed(prev)`
- 基线消息形态：多 source 的 initial 文本以固定分隔连接，顺序按 key 排序（确定性）
- delta 消息形态（进 transcript 一次性 system 消息）：
  - 变更：源内给出 diff 或全替换文本（优先 diff，diff 更长时退回全文）
  - 移除：`The ... source no longer applies.` 固定句式

**per-call 观测记录（P0-d）**：`tool / args_bytes / output_bytes / est_tokens / duration / truncated / ok / call_id`，聚合维度：session × tool。

---

## 7. 边界情况

1. **MCP 工具中途连接/断开**（P1 首场景）：目录 delta 化后不再改 schema 字节；MCP 工具未直接暴露前不可被调用（与现状一致）。
2. **skill 中途安装/删除**：清单 source 变化 → delta 消息；skill 工具本身 schema 字节不变。
3. **JSON 解析失败/参数截断**（现状 `agent_loop.rs:1387-1426` 的 TRUNCATED 错误）：保留现行为，输出治理元数据沿用同一定位方式。
4. **会话 fork/断线重连**：基线从 visp-db 重放；fork 会话以父会话 current 播种（对齐 opencode `:176-182`）。
5. **异常大的工具输出**（接近 100KB+）：治理层落盘后回传仅 ~8K chars + 元数据；内存峰值与现状（先收 100KB 再截）一致或更低。
6. **provider 差异**：Anthropic 断点锚定 system 尾部 + 最后一个 tool（现状保留，核对基线合并后锚点仍稳定）；OpenAI 靠会话级复用标识 + 前缀稳定性。
7. **P2 的 execute 失败**：返回结构化错误（ParseError/UnknownTool/ToolCallLimit/Timeout...）+ logs，便于模型自纠；失败诊断本身计入预算（maxOutputBytes）。
8. **并行 tool calls 保持**：直连工具的并行语义（spawn + join_all）不变；P2 中并发发生在程序内。

---

## 8. 关键设计决策记录

| 决策 | 选择 | 理由 |
|---|---|---|
| 分期而非全盘照搬 | P0/P1 先行，P2 门槛制 | P2 成本高且收益依赖使用形态；P0-d 提供决策数据 |
| CodeMode 运行时 | rquickjs 推荐、自研不采纳 | 工程成本/性能权衡（§4.6） |
| 指南内容策略化 | 工具信息单点披露（schema），指南只留策略短行 | 消除双份重复（现状 codegraph） |
| rules 不迁移 | 留扩展点 | 控制本期系统提示词行为变化面，降低回归风险 |
| 输出治理统一在工具层 | 100KB/8000 两层 → 预算组装 + 落盘引用 | 消除两层不知情截断；可恢复 |
| 目录走 Instructions 而非挂 description | 对齐 opencode v2 演进 | 易变内容不污染缓存锚点 |

---

## 9. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 指南/描述文案变化可能导致模型行为回归（工具选择习惯） | golden 测试 + 分段评审文案；P0 只删重复不改语义；观测对比优化前后 per-tool 使用分布 |
| delta 体系引入会话状态新故障面（基线丢失/错序） | visp-db 持久化设计为可重放；渲染器纯函数 + fallback 全量重发 |
| rquickjs unsafe FFI / 沙箱逃逸 | 全局对象深冻结；默认无宿主扩展；资源上限默认收紧；安全评审作为 P2 立项条件之一 |
| 落盘文件敏感信息泄漏 | 落盘目录继承会话存储权限；7 天清理；引用路径仅会话内有效 |
| OpenAI 复用标识兼容性 | 按会话派生；异常时退化为无标识（现状行为） |

---

## 10. 验证方法（对齐 opencode 文档 §验证方法）

1. **首轮 system + tools 体积**：抓 daemon 发往 provider 的请求体，对比优化前后字节数（预期 P0 后 system 相关部分从 ~5–6K chars 降至 ~2.5K chars 量级；标估算方法：4 chars/token）。
2. **指令不变场景**：同一会话连续 ≥2 轮，P1 后 system 基线部分应逐字节相同；OpenAI usage 的 cache 命中率提升、Anthropic cache_read 占比不降。
3. **变化场景**：会话中途安装 skill / 连接 MCP server → 应只出现一条小 delta 消息，schema 字节不变。
4. **截断场景**：构造 60KB 输出的工具 → 输出含落盘路径与元数据，`read_file` 引用路径可取回全文。
5. **长输出长会话**（>20 轮）：对比 P0/P1 前后总 input tokens 与 cache_write 重复率。
6. **per-tool 观测**：P0-d 上线后跑典型任务集（探索型 / CRUD 型 / 聚合型），产出聚合报告 → 支撑 P2 决策门槛判断。

---

## 11. 后续步骤

1. 用户审核本设计文档（重点：P2 取舍、常量取值、rquickjs 选型）；
2. 审核通过后进入工作计划（TDD 拆解、按阶段波次并行、golden 测试先行）；
3. P0 先行落地 → 用 §10.6 数据复核 P2 决策门槛 → 决定是否立项 P2 及技术 POC（rquickjs 沙箱冻结验证）。
