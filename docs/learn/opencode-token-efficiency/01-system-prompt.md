# 01. System prompt 组装与瘦身（总览项 A）

> 分析对象：`../opencode-1.18.31`、`../opencode-2.0.9`（均为无 `.git` 快照）
> 对应总览：[`00-overview.md`](./00-overview.md) 的 A 项
> 方法：直接源码比对（子 agent 委派因 API key 不可用）
> 结论日期：2026-09

## 0. 一句话结论

v1 每个 step 把「按模型选定的**整段** prompt 文件（8–15 KB）+ 环境 + AGENTS.md 全文 + skills + MCP」
拼进 system 数组**重算重发**；v2 拆成
**756 B 静态基座（按工具条件插 4 行）+ provider 插件小补丁（anthropic 仅 300 B）+ 模型身份 3 行 + 环境等迁移到「冻结的指令基线」**，
每轮请求 system 字节稳定可缓存。

---

## 1. v1（1.18.31）：整段 prompt 文件 + 每 step 重算

### 1.1 选择整段文件

`packages/opencode/src/session/system.ts:28-51` 的 `provider(model)` 按 `model.api.id` 子串匹配，
返回**一整个** prompt 文件（位于 `session/prompt/`）：

| 模型匹配 | 文件 | 实测大小 |
|---|---|---|
| `claude` | `anthropic.txt` | 8,212 B |
| `gpt`（含 codex/gpt-6 分支） | `gpt.txt` / `codex.txt` / `gpt-astra.txt` | 9,284 / 7,390 / 4,078 B |
| `gpt-4`/`o1`/`o3` | `beast.txt` | 11,080 B |
| `gemini-` | `gemini.txt` | 15,372 B |
| Copilot GPT-5 | `copilot-gpt-5.txt` | 14,241 B |
| `kimi` | `kimi.txt` | 8,695 B |
| `muse` | `meta.txt` | 9,159 B |
| `trinity` | `trinity.txt` | 7,748 B |
| 其他 | `default.txt` | 8,528 B |

（`session/prompt/*.txt` 共 15 个文件、114,107 B，含 plan 模式相关文本。）

### 1.2 每个 LLM step 重新拼装

`packages/opencode/src/session/prompt.ts` 的 step 循环内（`prompt.ts:1257-1271`）：

```
const [skills, env, instructions, mcpInstructions, modelMsgs] = yield* Effect.all([
  sys.skills(agent),
  sys.environment(model),
  instruction.system().pipe(Effect.orDie),   // ← AGENTS.md / CLAUDE.md / config.instructions 全文
  sys.mcp(agent, session.permission),
  MessageV2.toModelMessagesEffect(msgs, model),
])
const system = [...env, ...instructions, ...(mcpInstructions ? [mcpInstructions] : []),
                ...(skills ? [skills] : [])]
```

- `environment()`（`system.ts:69-105`）每次用 `new Date().toDateString()` 现算，
  含 "You are powered by the model named ..." 一句 + `<env>` 块。
- `instruction.system()` 在**每个 step** 都执行：AGENTS.md / CLAUDE.md / CONTEXT.md /
  `config.instructions` / 远程 URL **全文**进 system 数组。
- `skills()`（`system.ts:107-119`）输出 verbose 版技能列表。

### 1.3 请求头拼接

`packages/opencode/src/session/llm/request.ts:56-78`：

- 第 1 条 system = `agent.prompt ?? SystemPrompt.provider(model)`（整段 8 KB 级文件）；
- 第 2 条 system = 上面 `input.system`（env + instructions + mcp + skills）join 后的整块 + `user.system`；
- 即典型请求带 **2 条 system 消息**，第 1 条就是那 8–15 KB。

---

## 2. v2（2.0.9）：小基座 + 条件行 + 插件补丁 + 冻结基线

### 2.1 静态基座：756 B

`packages/core/src/session/runner/prompt/system.txt` **全文**（15 行，756 B）：

```
You are an AI agent running in OpenCode, a coding agent harness. Help the user accomplish their goals using the tools you have available.

# Harness
- Responses are rendered as GitHub-flavored Markdown.
- `<system-reminder>` blocks are harness instructions, not user-authored content. Read and follow them.
- Prefer parallelizing independent tool calls.
${OPENCODE_TOOL_GUIDANCE}

# Communication
- Use clear file paths when referring to files.
- Keep responses clear and concise, and avoid unnecessary technical jargon.

# Working in codebases
- Keep changes consistent with the structure, naming, style, and patterns of the surrounding code.
- Treat unfamiliar files or changes as potential user work and investigate before deleting or overwriting them.
```

### 2.2 按工具条件插入，而不是整段策略长文

`packages/core/src/session/system-prompt.ts:9-28`：渲染时把 `${OPENCODE_TOOL_GUIDANCE}`
替换为**只与实际存在工具有关**的 4 行：

- 有 `shell` → 2 行（优先专用工具；禁止用分隔符串命令）；
- 有 `write` → 1 行（write 建新文件/整替，目标修改用 edit）；
- 有 `edit` → 1 行（edit 的 oldString/newString 精确替换规则）；
- 都没有 → 空串。

入口 `make(tools)`（`system-prompt.ts:5-7`）接收的是**本请求实际会下发的工具名列表**
（`model-request.ts:86`：`input.tools.definitions.map((tool) => tool.name)`）。

### 2.3 provider 差异 → 插件小补丁

`packages/core/src/plugin/optimize.ts`：

- `AnthropicPlugin`（`:22-30`）：命中 `claude` 时**追加**（`mode: "append"`）
  `system-prompt/anthropic.txt` —— 实测 **300 B**，只有一段「代码注释密度」指引；
- `OpenAIPlugin`（`:16-20`）：命中 `gpt` 时**覆盖**为 `gpt.txt`（2,572 B）/ `gpt-astra.txt`（3,825 B）；
- kimi / meta / trinity 仍保留较大补丁（8,701 / 7,853 / 7,684 B）——
  **瘦身主要发生在 anthropic 与 gpt 两个主流家族**；
- 都尊重自定义 agent：`agent.system` 存在时补丁直接跳过（`optimize.ts:81`）。

补丁 hook 挂在 session 的 `context` / `compaction` / `generate` 三个阶段（`optimize.ts:88-90`）。

### 2.4 「你是谁/环境/日期」迁往指令基线

v1 里 system 头部的 model 一句话与 `<env>` 块，v2 拆成两处：

- **模型身份** → `plugin/identity.ts:9-16`，3 行 `# Your Model`（Name / Provider ID / Model ID），
  `splice(1, 0, ...)` 插在基座之后（`identity.ts:27`）；
- **环境与日期** → `instructions/builtins.ts:25-55` 成为两个 Instructions source
  （key `core/environment` / `core/date`），进入会话**冻结基线**（见 §3.3）；
  日期跨天时渲染的是增量 `Today's date is now: ...`（`builtins.ts:53`），不再动 system。

### 2.5 请求组装入口

`packages/core/src/session/model-request.ts:73-93` `baseTranscript`：

- `system[0]` = `agent.system ?? SessionSystemPrompt.make(工具名列表)`；
- `system[1]` = `input.initial` —— 指令 epoch 基线（见 §3.3）；
- 随后 `prepare`（`:201-254`）触发 `session.context` 插件 hook（identity / optimize / opencode 等在此修改
  `event.system`），最终 `LLM.request` 携带 `promptCacheKey = sessionID`（`:247`）。

---

## 3. 代码走读（关键片段）

### 3.1 条件拼装（v2 `session/system-prompt.ts`）

```
render(prompt, tools):
  instructions = []
  tools.includes("shell") → push 2 行
  tools.includes("write") → push 1 行
  tools.includes("edit")  → push 1 行
  return prompt.replace("${OPENCODE_TOOL_GUIDANCE}", instructions.join("\n"))
```

### 3.2 provider 补丁（v2 `plugin/optimize.ts` 节选）

```
AnthropicPlugin = make("opencode.prompt.anthropic", (model) =>
    model.id 含 "claude" ? PROMPT_ANTHROPIC : undefined, "append")

hook:
  agent.system 存在 → return            // 自定义 prompt 不动
  rendered = SessionSystemPrompt.render(template, Object.keys(event.tools))
  mode === "append" → event.system[0].text += "\n\n" + rendered
  否则覆盖 event.system[0]
```

### 3.3 冻结的指令基线（v2）

- `session/context.ts:145-153`：`Instructions.combine([builtins(env,date), CodeMode 目录,
  discovery(AGENTS.md 等), skills, references, mcp, entries])`；
- `session/instruction-state.ts:164-174` `initial()`：渲染 **epoch 基线**，作为 system 第 2 部分
  随每个请求发送，但**字节在 epoch 内冻结**；
- 源变化时 `commit()`（`:38-61`）渲染 delta，经 `SessionEvent.InstructionsUpdated` 投影为
  transcript 中的一次性 system 类型消息（`session/projector.ts:666-671`），
  **不改写 system**（这是与 C 项共享的机制）。

---

## 4. 最终拼出来的 prompt 是什么样子的

### 4.1 v1（Anthropic，第一个 LLM step）

```
system[0]（8,212 B）:
  You are OpenCode, the best coding agent on the planet.
  You are an interactive CLI tool that helps users with software engineering tasks. ...
  IMPORTANT: You must NEVER generate or guess URLs ...          ← URL 策略段
  You have access to the TodoWrite tools ... Use these tools
  VERY frequently ...                                           ← TodoWrite 长文
  <example>                                                     ← 示例对话（anthropic.txt:31-48）
  assistant: I'm going to use the TodoWrite tool to write the following items ...
  </example>
  ... 整段角色设定、工具策略、tone 指引 ...

system[1]（长度随项目而定）:
  You are powered by the model named claude-... . The exact model ID is anthropic/claude-...
  Here is some useful information about the environment you are running in:
  <env>
    Working directory: ...
    Workspace root folder: ...
    Is directory a git repo: yes
    Platform: darwin
    Today's date: Sat Sep 19 2026
  </env>

  <AGENTS.md 全文>
  <其他指令文件全文>

  <mcp_instructions>...</mcp_instructions>
  Skills provide specialized instructions and workflows ...（verbose 技能列表）
```

（行首引文摘自 `anthropic.txt:1-48` 实测；规模与构成为示意。）

### 4.2 v2（Anthropic，默认 agent）

system 数组（4 个 part）：

```
system[0] ≈ 1.5–1.7 KB：
  You are an AI agent running in OpenCode, a coding agent harness. ...
  # Harness
  - Responses are rendered as GitHub-flavored Markdown.
  - `<system-reminder>` blocks are harness instructions ...
  - Prefer parallelizing independent tool calls.
  - Prefer dedicated tools over shell commands; ...        ← shell 存在才出现
  - Do not chain shell commands with separators ...        ← shell 存在才出现
  - Use the write tool to create files ...                 ← write 存在才出现
  - Use the edit tool for targeted changes ...             ← edit 存在才出现
  # Communication
  - ...
  # Working in codebases
  - ...

  # Code comments                                          ← anthropic 补丁（300 B，append）
  By default, match the surrounding comment density: ...

system[1]：
  # Your Model                                             ← identity 插件
  - Name: Claude ...
  - Provider ID: anthropic
  - Model ID: claude-...

system[2]（指令 epoch 基线，单条 part，\n\n 连接）：
  Here is some useful information about the environment you are running in:
  <env>
    Current conversation session ID: ses_...
    Working directory: ...
    Workspace root folder: ...
    Is directory a git repo: yes
    Platform: darwin
    Prefer /var/folders/.../T/opencode over generic system temporary directories ...
  </env>

  Today's date: Sat Sep 19 2026

  The Code Mode tool catalog below is complete.            ← 有 MCP 时；≤2000 token 预算
  ## Available tools
  ...

  <AGENTS.md 全文>                                          ← discovery source
  Skills provide ...                                        ← skills source
  <mcp_instructions>...</mcp_instructions>

system[3]：
  When you create a worktree outside the current working directory ...  ← opencode 工具插件
```

**对比**：system 第 1 部分（模型相关固定头）从 **8.2 KB → ~1.6 KB**；
「工具策略长文」变成至多 4 行条件行；整体结构多 part 化，便于 provider 缓存断点（E 项）。

---

## 5. 第二轮请求是什么样子的

### 5.1 v2

- **system 完全不变**：4 个 part 逐字节与第一轮相同——
  基座是静态文件、补丁是确定性渲染、`# Your Model` 只依赖模型、
  基线冻结在 epoch（连日期都冻结，跨天走 `Today's date is now:` 增量消息）。
- **messages**：第一轮的完整历史（user → assistant → tool 调用与结果 → …）原样在前，
  第二轮 user 消息追加在尾；若两轮之间 AGENTS.md / MCP 工具集等发生变化，
  变化以**一条小的 delta system 消息**追加在历史里（一次性），system 仍不动。
- **缓存**：`promptCacheKey = sessionID`（`model-request.ts:247`）+ 默认 4 个缓存断点（E 项），
  第二轮起 system 与既有历史全部命中 cache read（约 10% 价格）。
- **tools**：同一批 13 个定义（12 直连 + `execute`），字节不变。

### 5.2 v1

- system **每个 step 重新计算**（`prompt.ts:1257-1271` 在 step 循环内）：
  8.2 KB 头 + `new Date()` 现算的 env + AGENTS.md 全文 + skills 重发一遍。
- 内容通常逐字节相同（同一天、文件没改），所以 provider 侧也可能命中缓存——
  **但要缓存的量多得多**（头部 8.2 KB + 全量指令），且缓存是条件式、分散的
  （`provider/transform.ts applyCaching`，见 E 项）；
  一旦跨天或指令文件变化，整个 system[1] 变化 → 缓存失效重写。
- tools：内建 + 插件 + MCP 全量逐个 schema（B 项）。

### 5.3 一轮请求的体感差异（量级示意）

| 部分 | v1 | v2 |
|---|---|---|
| system 固定头（模型相关） | ~8.2 KB（≈2K tokens） | ~1.6 KB（≈0.4K tokens） |
| 环境块 | system 内，每轮现算 | 基线内，冻结 |
| 工具策略 | 整段长文 | ≤4 行条件行 |
| 每轮增量成本 | 全量重发（靠缓存抵扣） | 全量重发但基数小、断点稳定 |

> 真实减少的发送量主要来自固定头（A）与工具目录（B）；
> 基线冻结 + 断点（E）主要降低计费。

---

## 6. 常量与量化（实测自快照）

| 项 | v1 | v2 | 位置 |
|---|---|---|---|
| anthropic 头 | 8,212 B | 基座 756 B + 补丁 300 B | `session/prompt/anthropic.txt` / `runner/prompt/system.txt` / `plugin/system-prompt/anthropic.txt` |
| gpt 头 | 9,284 B | 覆盖为 2,572 B | `plugin/system-prompt/gpt.txt` |
| gemini 头 | 15,372 B | 无专门补丁（走基座） | — |
| kimi/meta/trinity | 8,695 / 9,159 / 7,748 B | 8,701 / 7,853 / 7,684 B（未瘦身） | `plugin/system-prompt/` |
| 条件工具行 | 无 | shell×2 + write×1 + edit×1 | `session/system-prompt.ts:9-28` |
| 拼装时机 | 每 LLM step | 每请求 + hook | `prompt.ts:1257` / `model-request.ts:73-93` |

---

## 7. 可验证方法

1. **抓首轮请求体**：两版各跑一次（Anthropic 模型），比较 system 第 1 部分字节数
   （预计 8.2 KB → ~1.6 KB）。
2. **关掉工具看条件行**：用禁用 shell/write/edit 的 agent 跑 v2，
   确认 `${OPENCODE_TOOL_GUIDANCE}` 位置为空（`system-prompt.ts` 只在工具存在时插行）。
3. **自定义 agent**：设置 `agent.system` 后确认 v2 不再追加 provider 补丁（`optimize.ts:81`）。
4. **第二轮 diff**：连续两轮对话，对比两轮请求的 system 部分——
   v2 应逐字节相同；把系统日期跨天（或改 AGENTS.md）再跑，
   v2 应出现 `Today's date is now:` / 指令 delta 消息，而 system 不变。
5. **缓存观测**：看第二轮 usage 的 `cache.read` 占比（配合 05 篇的断点分析）。
