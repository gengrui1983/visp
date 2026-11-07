# 04. 压缩与上下文窗口管理（总览项 D）

> 分析对象：`../opencode-1.18.31`、`../opencode-2.0.9`（均为无 `.git` 快照）
> 对应总览：[`00-overview.md`](./00-overview.md) 的 D 项
> 方法：直接源码比对（子 agent 委派因 API key 不可用）
> 结论日期：2026-09

## 0. 一句话结论

两者都是「接近窗口上限 → 用同一模型生成分段摘要 → 旧历史折叠为 checkpoint、保留尾部原文」，
v2 的升级在四点：**真实 usage 锚点 + 只对锚后增量做本地估算**、
**checkpoint 走真实消息结构（复用系统前缀，可缓存）而非整段重序列化文本**、
**结构化摘要模板 + 输出校验 + 二次重试**、**provider 原生压缩（加密 checkpoint）与全请求级丢旧图片**。

> **对 00 总览的一处勘误**：00 文档 D.1 称 v1 溢出判断用 `Token.estimate(JSON.stringify(msgs))`。
> 经核对，v1 的**触发**同样使用 provider 真实 usage（`session/overflow.ts:22-34` 读的是
> 上一条 assistant 的 `tokens`）；粗估 `Token.estimate(JSON.stringify(...))` 用在
> **尾部预算选择**（`compaction.ts:215-221`）与 core 侧溢出恢复的预检里。
> v2 的改进是「锚点 + 增量」的估算方式与逐 part 的媒体估算，而非「真实 usage vs 粗估」的对立。

---

## 1. v1（1.18.31）

### 1.1 触发（`session/overflow.ts`）

- `usable()`（`:10-20`）：`limit.input - reserved`，否则 `context - maxOutput`；
  `reserved = cfg.compaction.reserved ?? min(20_000, maxOutput)`（`COMPACTION_BUFFER`）；
- `isOverflow()`（`:22-34`）：`cfg.compaction.auto === false` 或 `context === 0` 时关闭；
  否则 `tokens.total || input+output+cache.read+cache.write >= usable()` —— 真实 usage。

### 1.2 独立的 prune（旧工具输出清理）

`session/compaction.ts:273-317`，与摘要压缩相互独立：

- 从后往前跳过最近 2 个 user turn；`skill` 工具受保护（`PRUNE_PROTECTED_TOOLS`，`:31`）；
- 累计 `Token.estimate(output)`，超过 `PRUNE_PROTECT = 40_000` 后更旧的部分进入待清理；
- 清理量 `> PRUNE_MINIMUM = 20_000` 才执行：把旧 tool part 标记 `time.compacted`，
  之后的序列化里显示 `[Old tool result content cleared]`（`:76-78`）；
- 序列化截断 `TOOL_OUTPUT_MAX_CHARS = 2_000`（`:30,51-52`）。

### 1.3 摘要压缩（`processCompaction`，`:319-557`）

- **尾部保留**：`preserveRecentBudget = clamp(2_000, 15_000, usable×0.25)` 或配置
  （`:115-120`）；按 turn 划分，惰性估算（`estimate()` = `Token.estimate(JSON.stringify(model msgs))`，
  `:215-221`），装不下再用 `splitTurn` 在 turn 内二分起点（`:140-163`）；
  尾起点记入 compaction part 的 `tail_start_id`。
- **旧 checkpoint 隐藏**：`completedCompactions` 找出之前压缩的 user+assistant 消息对，
  从本次输入中剔除（`:97-113, 364-368`）；上一份摘要以 `<prior-summary>` 标签传入。
- **head 全量重序列化**：`serialize()`（`:54-85`）把 head 拍平成
  `[User]: ...` / `[Assistant]: ...` / `[Assistant tool call]: ...` / `[Tool result]: ...` 的**纯文本**，
  全部塞进单个 user 消息的 `<conversation>` 标签。
- **摘要请求**（`prompt.ts` 管线 + core `buildPrompt`，core `session/compaction.ts:160-177`）：
  - 走 compaction 专用 agent（630 B prompt，`agent/agent.ts:219-224`）；
  - core 的溢出恢复路径（`compactAfterOverflow`）更彻底：`messages: [Message.user(summaryPrompt)]`、
    `tools: []` 的**裸调用**（core `:205-211`），`SUMMARY_OUTPUT_TOKENS = 4_096` 封顶；
  - 提示结构：`<conversation>`（本次 head 全文）+ `<prior-summary>` + 「合并两份、对话优先」的
    `SUMMARY_UPDATE_INSTRUCTIONS` + 模板。
- **溢出重放**：provider 报溢出时，把触发压缩的原始 user 消息重放，媒体换成
  `[Attached <mime>: <file>]` 占位（`:484-487`）；
- **自动继续**：压缩成功后插入一条 synthetic user 消息
  （`metadata.compaction_continue`，`:519-548`）。

### 1.4 v1 摘要模板

core `session/compaction.ts:19-46`：`Objective / Important Details / Work State(Completed/Active/Blocked) /
Next Move / Relevant Files` + 简短 Rules；输出**无格式校验**（只看 `summary.trim()` 非空）。

---

## 2. v2（2.0.9）

### 2.1 设置与触发（`packages/core/src/session/compaction.ts`）

- 设置：`auto / buffer = DEFAULT_BUFFER 20_000 / tokens = DEFAULT_KEEP_TOKENS 15_000`（`:40-41,402-404`）；
- `required()`（`:742-763`）：
  - 自动模式且最后一条不是刚完成的 checkpoint；
  - **锚点检查**（`:750-753`）：必须存在比最近 checkpoint 更新的真实 usage——
    原生压缩刚替换完窗口、还没有新的主响应时**不再触发**；
  - 上限 `promptCeiling = min(limit.input - buffer, context - max(output, buffer))`，
    `output = min(limit.output, OUTPUT_TOKEN_MAX = 32_000)`（`:754-761`）；
  - `estimateTokens(input) >= promptCeiling` 才压缩。

### 2.2 真实 usage 锚点 + 增量估算（`estimateTokens`，`:174-208`）

- 找最后一条带 input usage 的 assistant 消息作**锚**（`:168-176`）；
- 锚之前的历史不估算——其成本已由锚的 provider usage 覆盖：
  `锚.input + cache.read + cache.write + output + reasoning`（`:184-192`）；
- 只对**锚之后**的消息做本地估算（`:178-183`），且保留锚自身的本地 tool 结果
  （「not covered by its provider usage」，`:177` 注释）；
- 逐 part 估算（`estimatePart`，`:215-230`）：图片 `IMAGE_TOKEN_ESTIMATE = 1_500`、
  PDF `PDF_TOKEN_ESTIMATE = 2_000`（`:44-45,210-213`）、加密 checkpoint 只估其文本；
- 无锚可用时退化为全量估算：system 各 part + 工具定义（`:193-207`）。

### 2.3 本地摘要压缩（`execute`，`:595-735`）

- **切分**：`splitHistory(messages, keepTokens = 15_000)`（`:322-362`）——
  从尾部按 `Token.estimate` 累积保留最新内容，**对齐到 user 边界**
  （assistant 的 tool 调用与结果不拆散，`:348-350`）；
  全都装得下时只保留最近一轮（有旧 checkpoint 才有可压缩的前缀，`:352-361`）；
- **tail 序列化**：`serializeRecentMessage`（`:279-320`）比 v1 覆盖更多类型
  （skills 激活、location 切换、synthetic、后台 shell 排除），工具输出仍截
  `TOOL_OUTPUT_MAX_CHARS = 2_000`；checkpoint 与指令更新不进尾部文本（`:280-281`）；
- **head 走真实消息结构**：`compactionRequest`（`:469-494`）用 `baseTranscript`
  ——system 是**与主请求相同的基座 + 指令基线**（可命中缓存），
  head 以**原始角色/工具调用结构**发送，旧 checkpoint 消息就在 transcript 里；
- **增量 checkpoint 指令**：`buildPrompt(update=true)`（`:364-385`）——
  `Update the existing checkpoint in the conversation above into one consolidated summary`、
  `Newer history always takes precedence ...`、对旧模板 checkpoint（含 `## Additional Context` 遗留标题，
  `:87-88,624-625`）额外要求按新模板重写；摘要 prompt 在 hook 之后以 user 消息追加（`:628-631`）；
  指令 delta 以 `Message.system` 附在请求末尾（`:488-491`）；
- **模板与规则**（`:46-85`）：Objective / Requirements / Decisions /
  Work State(Completed/Active/Blocked) / Next Move / Relevant Files(≤15) / Important Context；
  Rules 强制简洁、不复述、保留精确路径/命令/错误串、只携带未解决的用户请求、
  标注变更是否已 commit/push/merge；
- **输出校验 + 重试**：`hasSummarySection` 要求出现模板标题（`:392-393`），
  不合格**自动补一条提醒再试一次**（`:638-648`）；finish reason `length` / `content-filter`
  都判失败（`:674-681`）。

### 2.4 provider 原生压缩（`executeProvider`，`:501-594`）

- `model.compaction?.type === "native"` 时走此路径（`:739-740`）；
- 插件可注册 `native` 策略（`:111-115,411-413`）；要求稳定 endpoint 与目录内路由兼容
  （`:516-528`），否则明确拒绝；
- 把**完整当前请求**交给 provider，换回**加密 checkpoint**（replacement window）：
  以 `providerContext` 编码进 `Compaction.Ended` 事件（`:564-573`），
  成为 transcript 里的 checkpoint 消息；旧历史完全不进本地摘要；
- `retained`：给 checkpoint-only 机制提供「keepTokens 预算内的完整真实 user 消息」
  （`retainUsers`，`:232-256`——剔除 skills、合成消息，图片先行 boundImages）；
- 原生路径自身溢出且属自动压缩 → `recoverLocally`：从 **local 边界**重放原始 transcript
  再走本地摘要（`:495-499,577-585`）；
- 后续请求由 F 项机制消费：`history.ts:83-95` 只从最近 checkpoint 的 seq 之后读；
  `latest` 边界跨原生 checkpoint 重放，`local` 边界全部重展开（`:21-27`）。

### 2.5 全请求级丢旧图片（`session/model-request.ts:41-44,134-181`）

- 对**每个** LLM 请求（不只压缩）统计历史图片字节数；
- 超过 `IMAGE_BYTES_TRIGGER = 25 MiB` 时，从旧到新把图片替换为
  `IMAGE_REMOVED` 文本（明确告知模型「图已移除，不得凭记忆声称看过」），直到降到
  `IMAGE_BYTES_TARGET = 15 MiB`。

### 2.6 与 C 项的衔接

压缩完成时 `InstructionState.advanceEpoch`（`instruction-state.ts:139-154`）把当前指令值
提升为新基线——压缩后请求的 system 基线反映最新指令，旧 delta 消息留在边界前。

---

## 3. 代码走读（关键片段）

**锚点估算**（v2 `:174-192` 摘述）：

```
index = findLastIndex(hasInputUsage)          // 最后一条有真实 usage 的 assistant
added = 本地估算 messages[index..]（去掉锚 assistant 本身，保留其 tool 结果）
return added + 锚.input + cache.read + cache.write + output + reasoning
```

**增量 checkpoint 指令**（v2 `:372-384` 摘述）：

```
Update the existing checkpoint in the conversation above into one consolidated summary.
Newer history always takes precedence over the existing checkpoint. ...
Reconcile Work State and Next Move: ...
Return only the updated Markdown sections. Do not reproduce the
<conversation-checkpoint> / <summary> / <recent-context> wrapper tags ...
```

**v1 的对应物**（core `:160-177`）：`<conversation>`（本次 head 重序列化全文）+
`<prior-summary>` + 「合并两份，对话优先，prior-summary 丢弃」——
**每次都要重读整个 head 的拍平文本**。

---

## 4. 压缩后的会话长什么样

### 4.1 v1

```
... 更早消息 ...
[user, parts: [compaction]]          ← 压缩触发标记（隐藏于后续输入）
[assistant, summary: true]           ← 摘要文本（下一次压缩时以 <prior-summary> 传入）
... 保留的尾部 turn（tail_start_id 起）...
```

下一次请求：旧 checkpoint 消息对被 `hidden` 剔除；上下文 = 新摘要调用产出的
`<prior-summary>` + `<conversation>`（head 重序列化）+ 尾部原文。

### 4.2 v2

```
... 更早历史（checkpoint seq 之前不再被读取，history.ts:83-95）...
[compaction message, status: completed]     ← checkpoint；本地路径 text=结构化摘要，
                                              原生路径 providerContext=加密窗口
... 尾部保留的消息（对齐 user 边界，≤15K tokens）...
```

下一次请求：

- 只从 checkpoint seq 之后读历史；checkpoint 消息以
  `<conversation-checkpoint>` 包裹出现在 transcript 里；
- system 基线已随 `advanceEpoch` 前移（含最新 AGENTS.md 全文等）；
- 再触发压缩时，head（含旧 checkpoint）以真实消息结构发送，
  摘要指令只说「更新 conversation above 里的 checkpoint」。

### 4.3 请求形态对比（压缩调用本身）

| | v1 | v2 |
|---|---|---|
| system | compaction agent 630 B（core 恢复路径为裸调用） | 与主请求相同的基座 + 指令基线（缓存命中） |
| head 载体 | 单条 user 消息内的 `<conversation>` 拍平文本 | 原始 transcript（真实角色/工具调用/结构） |
| 旧摘要 | `<prior-summary>` 标签 + 隐藏旧消息对 | checkpoint 消息直接在 transcript 中，指令为「更新它」 |
| 输出校验 | 仅非空 | 模板标题校验 + 自动重试一次 |
| 媒体 | 仅溢出重放时占位 | 全请求 boundImages（25→15 MiB）+ 估算常量 |

---

## 5. 常量与阈值

| 常量 | v1 | v2 | 位置 |
|---|---|---|---|
| 触发 buffer | `COMPACTION_BUFFER` 20,000 | `DEFAULT_BUFFER` 20,000 | overflow.ts:8 / compaction.ts:40 |
| 保留尾部 | clamp(2,000..15,000, usable×0.25)；core 默认 8,000 | `DEFAULT_KEEP_TOKENS` 15,000 | compaction.ts(v1):32-33 / compaction.ts(v2):41 |
| 输出上限参与计算 | maxOutput（provider 值） | `OUTPUT_TOKEN_MAX` 32,000 封顶 | overflow.ts:16 / compaction.ts:42,757 |
| 工具输出截断 | 2,000 chars | 2,000 chars（surrogate 安全） | compaction.ts(v1):30 / compaction.ts(v2):43,258-270 |
| 图片/PDF 估算 | 无 | 1,500 / 2,000 tokens | compaction.ts(v2):44-45 |
| 丢图阈值 | 无（仅溢出重放占位） | 25 MiB → 15 MiB | model-request.ts:41-42 |
| prune | PRUNE_MINIMUM 20,000 / PROTECT 40,000 / 保护 skill | 无独立 prune（统一进压缩与输出治理 G） | compaction.ts(v1):28-31 |
| 摘要输出上限 | 4,096 tokens（core 路径） | 无显式上限；finish=length 判失败 | core:22 / v2:674-676 |

---

## 6. 可验证方法

1. **触发精度**：构造长会话逼近窗口，观察 v2 触发点是否贴合真实 usage
   （在锚点后追加大量内容才提前触发）；对比 v1 的尾部预算估算行为。
2. **压缩请求抓包**：v2 的压缩 LLM 调用应带完整 system（基座+基线）且 head 为真实消息结构；
   v1 是 compaction agent + 单条拍平 user 消息。
3. **增量 checkpoint**：连续两次压缩，第二次的提示应为
   `Update the existing checkpoint ...`，且旧 checkpoint 文本出现在 transcript 中；
   人为使用旧模板 checkpoint（含 `## Additional Context`）应触发 legacy 重写指令。
4. **模板校验**：让模型故意输出无标题文本，应看到一次
   `The previous response did not fill in the required summary template ...` 的重试。
5. **原生路径**：接支持 native compaction 的 provider/插件，确认
   `Compaction.Ended` 携带 `providerContext`、后续请求历史从 checkpoint 起、
   旧消息不再进入请求。
6. **丢图**：附加多张大图超过 25 MiB 后发起请求，旧图应被替换为
   `IMAGE_REMOVED` 文本。
