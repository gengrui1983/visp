# 02. 工具目录收拢与 CodeMode（总览项 B）

> 分析对象：`../opencode-1.18.31`、`../opencode-2.0.9`（均为无 `.git` 快照）
> 对应总览：[`00-overview.md`](./00-overview.md) 的 B 项
> 方法：直接源码比对（子 agent 委派因 API key 不可用）
> 结论日期：2026-09

## 0. 一句话结论

v2 把「N 个工具各自携带一份 JSON Schema」改成
**「12 个核心工具直连 + 1 个 `execute` 工具 + 一段受预算约束的文本目录（含 `search` 兜底）」**。

省 token 来自三处叠加：

1. **Schema 形态变了**：`execute` 的入参 schema 只有一个 `code: string` 字段，替代掉所有 MCP / 插件工具各自的完整 JSON Schema。
2. **目录从「schema」降级为「一行签名」**：工具以 TS 风格单行签名列出，且总预算封顶（`INLINE_BUDGET = 2000` tokens），列不下的靠 `search` 按需取。
3. **目录以增量指令下发**：完整目录只在会话基线里出现一次（稳定、可缓存），之后仅在工具集变化时追加小段 delta。

---

## 1. 什么是 CodeMode（概念）

> 更完整的概念、实现（含示例代码）与效果（含事例）见独立文档
> [`codemode-explained.md`](./codemode-explained.md)。本节仅保留理解 B 项所需的最小概念。

### 1.1 定义

CodeMode（代码模式）是一种「用**一段脚本**代替**多次工具调用**」的 agent 交互范式：

- 传统做法：把每个工具作为独立的 function call 暴露给模型；模型一次只调一个工具，
  等结果回到对话、再决定下一步（多轮往返）。
- CodeMode 做法：只给模型一个 `execute` 工具 + 一个**受限的 JavaScript 运行时**；
  模型写一段程序，在**一次调用内**调用多个工具、组合/过滤/并发处理结果，最后返回一个 JSON 值。

概念最早由 Cloudflare 提出（"Code Mode"），本项目在其思路基础上做了自己的实现。
来源：`packages/codemode/README.md:1-12`；包描述见 `packages/codemode/package.json`
（"Effect-native confined code execution over schema-described tools"）。

### 1.2 本项目的实现边界

`@opencode/codemode` 是一个**自研的「JavaScript 子集」解释器**，不是对任意 JS 做沙箱：

- 程序**不能**直接访问网络、文件系统、进程或应用 API（`README.md:7-9`）；
- 与外界交互只能通过宿主提供的两类能力：
  1. **工具** `tools.<namespace>.<tool>(input)`（点号构成嵌套路径，非标识符段用
     `tools["<name>"]` 或 `tools.context7["resolve-library-id"]` 这类括号写法，`README.md:76-78`）；
  2. **扩展全局**，例如 HTTP 用的 `fetch`（core 侧注入，`core/src/codemode/web.ts:40`）。
- 宿主可限制执行资源：`timeoutMs` / `maxToolCalls` / `maxOutputBytes`
  （`packages/codemode/src/codemode.ts:21-35`；注意**默认全部不限**，`README.md:185`）。
- 数据只在「普通 JSON」边界上穿过程序与宿主（`Data.toBoundary`，`src/data.ts`），
  非 JSON 值有明确的跨边界规则。

### 1.3 与普通 JavaScript 的差异

（`README.md:14-24`）

- 只有被实现的内建 API 可用；`fetch`/定时器/`process`/文件系统/import 等在语言层不可用
  （注意：这里的 `fetch` 指的是**宿主显式注入的扩展全局**，不是 JS 原生 `fetch`）。
- **未完成的工作会被打断**：程序结束时仍在运行的调用被中断；未 `await` 的 Promise 的
  rejection 以 warning 返回。
- **REPL 式返回**：没有显式 `return` 时，最后一个顶层表达式即结果；`undefined` 变 `null`。
- 不支持的语法返回带位置的 `UnsupportedSyntax` 诊断。

### 1.4 调用与返回形态

- 发现工具：内建**同步** `search(...)`（精确路径 / 按 namespace / 空查询浏览 / 分页），
  返回可调用的 path 与完整签名，并计入 `maxToolCalls`（`README.md:167-175`）。
- 返回：`CodeMode.Result`
  （`codemode/src/codemode.ts:93-118`）
  ——成功为 `{ ok: true, value, warnings?, logs?, truncated?, toolCalls }`，
  失败为 `{ ok: false, error, logs?, truncated?, toolCalls }`；
  诊断种类包括 `ParseError` / `UnknownTool` / `InvalidToolInput` / `ToolCallLimitExceeded` /
  `TimeoutExceeded` / `ToolFailure` 等（完整表见 `README.md:148-165`）。
- 运行时把每次工具调用通过 hooks 暴露给宿主（`tool.before`/`tool.after`），
  core 借此在 TUI 里展示进度（`core/src/codemode/tool.ts:143-176`）。

### 1.5 与「省 token」的关系

CodeMode 本身是**交互范式的改变**，token 收益来自它带来的两个副作用：

1. **工具暴露形态改变**：不再需要给模型每个工具的完整 JSON Schema，
   只需 `execute` 一个极小的 schema + 一段工具**签名目录**（本分档 B 的主题）。
2. **回合数减少**：一次 `execute` 内可完成多次工具调用与结果组合，
   中间结果不必逐条穿过对话历史。

代价与边界：程序源码本身会作为一次 tool call 进入上下文；诊断/失败信息也占 token；
目录仍需要有策略地下发（预算、增量、search 兜底）——这正是 2.0.9 要解决的问题。

---

## 2. v1（1.18.31）基线：每个工具一份 schema，逐个暴露

### 2.1 内建工具

`packages/opencode/src/tool/registry.ts` 用 `Tool.init(...)` 逐个初始化内建工具，并汇总成 `builtin` 数组
（`registry.ts:209-252`）：

```
invalid, question(条件), shell, read, glob, grep, edit, write,
task, fetch, todo, search, skill, patch,
execute?(实验), lsp?(实验), plan?(实验)
```

每个工具对象包含 `description` + `parameters`（运行时 schema）+ `jsonSchema`。`ToolRegistry.tools()`
（`registry.ts:291-340`）把过滤后的工具逐条转成 `{ id, description, parameters, jsonSchema, ... }` 发给 provider。
即：**模型看到的是与工具数量成正比的 schema 集合**。

### 2.2 插件工具

`fromPlugin`（`registry.ts:125-181`）把插件的 Zod args 转成 JSON Schema
（`zodJsonSchema` / `legacyJsonSchema`，`registry.ts:363-381`），同样**每个插件工具一份独立 schema**。

### 2.3 MCP 工具

默认（未开实验开关）在 `packages/opencode/src/session/tools.ts:390-...` 逐个
`McpCatalog.convertTool` 注入，每个 MCP 工具携带自己的 `inputSchema`。

### 2.4 v1 的实验性 CodeMode：只收拢 MCP

- 开关：`runtime-flags.ts:48` `experimentalCodeMode = enabledByExperimental("OPENCODE_EXPERIMENTAL_CODE_MODE")`，
  而 `enabledByExperimental` 默认回落到 `OPENCODE_EXPERIMENTAL`（`runtime-flags.ts:11-14`），**默认关闭**。
- 开启后，`registry.ts:118-119` 动态加载 `tool/code-mode.ts`，但 `code-mode.ts` 的目录只来自
  `mcp.tools()`（`code-mode.ts:210-212`）——**只把 MCP 工具树塞进 `execute`**；内建与插件工具仍逐个暴露。
- 目录文本生成：`code-mode.ts:58-65 describeCatalog()` → 追加到 `execute` 工具的 `description`
  （`registry.ts:305-308`、`325-331`）。
- `execute` 入参 schema 本身很小：`code-mode.ts:16-20` 只有 `code: string`。

**v1 的 token 形态**：内建工具描述文本合计约 **15 KB**（`packages/opencode/src/tool/*.txt`，实测 15,073 bytes），
外加每个工具的 JSON Schema；MCP 工具再逐个叠加 schema。开了实验 CodeMode 也只抵消 MCP 那部分。

---

## 3. v2（2.0.9）的做法（重点）

### 3.1 分流：`codemode` 默认开启

核心在 `packages/core/src/tool.ts`：

- `tool.ts:234` `direct` = `options.codemode === false` 的工具；
- `tool.ts:235` `codeModeTools` = `options.codemode !== false` 的其余全部工具。
- 契约写在 `packages/core/src/tool/AGENTS.md:33`：
  > `codemode` defaults true; `codemode: false` keeps the tool on the provider's native tool list.

**直连（provider 原生工具列表）的 12 个核心工具**，各自在插件里显式声明 `options: { codemode: false }`：

| 工具 | 声明位置 |
|---|---|
| `read` | `tool/plugin/read.ts:42` |
| `write` | `tool/plugin/write.ts:59` |
| `edit` | `tool/plugin/edit.ts:123` |
| `glob` | `tool/plugin/glob.ts:58` |
| `grep` | `tool/plugin/grep.ts:77` |
| `shell` | `tool/plugin/shell.ts:191` |
| `patch` | `tool/plugin/patch.ts:81` |
| `question` | `tool/plugin/question.ts:58` |
| `skill` | `tool/plugin/skill.ts:42` |
| `subagent` | `tool/plugin/subagent.ts:104` |
| `webfetch` | `tool/plugin/webfetch.ts:113` |
| `websearch` | `tool/plugin/websearch.ts:43` |

加上 `execute` 本身，模型每轮看到的 provider 工具数约为 **13**。（对比 v1 的 14 个内建
外加插件与 MCP 全量。）

**收进 CodeMode 的**：

- **MCP 工具**：`tool/mcp.ts:45` `options: { namespace: namespace(tool.server), codemode: tool.codemode !== false }`；
- **opencode 自管理工具**：`tool/plugin/opencode.ts:95/115/140`，`namespace: "opencode"`、
  `codemode: true`（`session_rename` / `session_move`(pinned) / `models`）；
- **插件工具**：默认 `codemode` 为 true，除非插件显式关掉。

> 注意 v2 不再直连 `todo` / `lsp` / `plan` 等（v1 的 `todowrite`、实验性 `lsp`/`plan` 在 v2 的核心工具集中不存在），
> 这让直连基数进一步下降。

### 3.2 `execute` 工具本体极小

- 名称固定 `execute`（`codemode/tool.ts:76`）；`execute` 是保留名，
  `tool.ts:313-314` 禁止 `codemode:false` 的工具占用该名。
- `description` 是**不随目录变化的固定文本**（`codemode/tool.ts:62-69`，约 1.0 KB），
  并显式写明「目录通过 Instructions 下发」：
  > Invariant model-facing guidance; the changing tool catalog is delivered through Instructions.
- 入参 schema = `CodeMode.Input` = `Schema.Struct({ code: Schema.String })`
  （`packages/codemode/src/codemode.ts:65`）。
- 出参 `ExecuteOutput` = `{ output, toolCalls, files, error? }`
  （`codemode/tool.ts:32-37`）。

**对比 v1**：v1 的 `execute` 描述是一行 72 字节（`code-mode.ts:14`），但目录文本直接挂在描述里、
不受预算与增量机制管理。v2 把「易变的目录」从「工具描述」里剥离出去，交给 Instructions 体系（见 §3.4）。

### 3.3 目录文本：预算化 + `search` 兜底

目录渲染在 `packages/core/src/codemode/catalog.ts`：

- 常量（`catalog.ts:48-50`）：
  - `DESCRIPTION_LIMIT = 120`（每条 listing 的 description 截断到 120 字符，`catalog.ts:65-72`）；
  - `CHARACTERS_PER_TOKEN = 4`（token 估算）；
  - `INLINE_BUDGET = 2_000`（内联目录的 token 预算上限，`catalog.ts:50,54-55`）。
- 选择算法 `summarize()`（`catalog.ts:54-140`）：
  1. 先为**每个 namespace 至少保留一行**（namespace 头 + 计数，`namespaceLine` `catalog.ts:165-174`）；
  2. 然后「每轮每个 namespace 各选一条最短的 listing」，直到预算耗尽（`catalog.ts:115-127`）——
     短工具优先，尽可能多列；
  3. `pinned` 工具强制可见，不参与淘汰（`catalog.ts:79-92`；`codemode/tool.ts:178-193` 读取 `options.pinned`）。
- 单条 listing 一行：`  - <signature><space>// <description首行>`
  （`catalog.ts:65-72`；`namespaceLine` `catalog.ts:165-174`）。
- **未列出的工具靠 `search` 发现**（`packages/codemode/src/tool-runtime.ts`）：
  - `search` 入参 `{ query?, namespace?, limit?, offset? }`（`tool-runtime.ts:92-97`），
    默认 `defaultSearchLimit = 10`（`tool-runtime.ts:89`）；
  - 排序按 path/description/参数名匹配打分（`tool-runtime.ts:238-262`），返回 `path/description/signature`；
  - 提示语只在「目录不完整」时出现，并附 `search` 签名（`codemode/instructions.ts:9-19`）。
- **签名懒渲染**：`describeTool` 的 `signature` 是 getter（`tool-runtime.ts:169-184`），
  只有被列出或 search 命中才把 JSON Schema 渲染成 TS 风格签名（`tool-schema.ts`，渲染深度上限
  `MAX_RENDER_DEPTH = 8`，`tool-schema.ts:25`），避免全量开销。

目录文本形态示例（概念）：

```
- <serverName> (N tools) // namespace 描述
  - tools.<ns>.<tool>(input: { ... }): Promise<...> // 工具首行描述
```

### 3.4 目录以「增量指令」下发，不每轮重发

- `codemode/instructions.ts:128-143` 把目录包装成一个 Instructions source
  （key `core/codemode`），提供 `initial` / `changed` / `removed` 三种渲染。
- 注册进会话：`session/context.ts:145-153` 与 builtins / discovery / skills / references / mcp / entries
  `Instructions.combine(...)` 一起注入。
- `update()`（`instructions.ts:34-126`）在目录变化时**优先只发 delta**（新增 / 变更 / 移除的 listing，
  甚至在「目录不完整」时只发 namespace 计数变化），仅当 delta 比整段替换更长时才整段重发
  （`instructions.ts:93-95, 123-125`）。
- 基线 / 增量由 `session/instruction-state.ts` 统一管理（与总览 C 项同一机制）：
  `initial()`（`instruction-state.ts:164-174`）渲染 epoch 基线，`commit()`（`:38-61`）只在变化时落 delta。

**token 含义**：完整目录作为会话基线的稳定前缀出现（字节不变，利于缓存），
不随每个 step 重新生成/追加；工具集变化时追加的是小段 delta。

### 3.5 MCP 侧配套

- **server instructions 也按 codemode 分流**：`mcp/instructions.ts:18-30` 对 `codemode !== false` 的服务器
  追加「通过 `execute` 下的 `tools[<namespace>]` 使用」的指引，并随 `codemode` 变化走增量
  （`mcp/instructions.ts:32-55`）；同时按 agent 权限与能否 `execute` 过滤（`:83-107`）。
- **自带 CodeMode 的服务器不嵌套**：`plugin/mcp-codemode-exclusion.ts:7-24` 对
  `executor.sh` 或 `executor mcp` 本地命令设 `codemode: false`，直接暴露，避免「CodeMode 套 CodeMode」。
- **HTTP MCP 主动要求原始工具**：`mcp/client.ts:214-218`，当 `config.codemode !== false` 时
  自动追加 `?codemode=false`，让服务端返回原始工具列表而非它自己的 CodeMode 包装。

### 3.6 目录渲染缓存

`tool.ts:246-254` 以 `(data, names)` 为键缓存已渲染的 catalog：
注册表版本与可见工具名集合不变时，跨多个 step 复用同一份目录，不重复渲染。
注释明确写出「share the much larger rendered catalog across steps」。

---

## 4. 关键差异对照

| 维度 | 1.18.31 | 2.0.9 |
|---|---|---|
| 工具暴露方式 | 全部逐个携带 JSON Schema | 12 核心直连 + 1 `execute` |
| CodeMode 默认 | 关闭（实验 env） | 开启（`codemode !== false`） |
| CodeMode 覆盖范围 | 仅 MCP 工具 | MCP + opencode 管理 + 插件（除显式 opt-out） |
| 目录承载位置 | `execute` 的 description 文本 | Instructions source（增量 / 预算化） |
| 目录预算 | 无上限，全量 | `INLINE_BUDGET = 2000` tokens，超出靠 `search` |
| 目录形态 | 逐工具条目 | namespace 分组 + 单行 TS 签名 + 描述首行截 120 字符 |
| 未列出工具发现 | 无 | `search(query/namespace/limit/offset)` |
| 变化时下发 | 每次重发全长 description | 优先 delta，必要时整段 |
| 自带 CodeMode 的 MCP | 无特殊处理 | `mcp-codemode-exclusion` + URL `?codemode=false` |

---

## 5. 常量与阈值（可回溯）

| 常量 | 值 | 位置 |
|---|---|---|
| `INLINE_BUDGET` | 2000 | `packages/core/src/codemode/catalog.ts:50` |
| `CHARACTERS_PER_TOKEN` | 4 | `catalog.ts:49` |
| `DESCRIPTION_LIMIT` | 120 | `catalog.ts:48` |
| `defaultSearchLimit` | 10 | `packages/codemode/src/tool-runtime.ts:89` |
| `MAX_RENDER_DEPTH` | 8 | `packages/codemode/src/tool-schema.ts:25` |
| `execute` 名称保留 | — | `packages/core/src/tool.ts:313-314` |
| v2 `execute` 固定描述 | ≈1.0 KB | `packages/core/src/codemode/tool.ts:62-69`（实测 1031 bytes） |
| v1 内建工具描述合计 | ≈15 KB | `packages/opencode/src/tool/*.txt`（实测 15,073 bytes） |

---

## 6. 为什么更省（token 账）

1. **schema 侧**：v1 每个 MCP/插件工具都带完整 JSON Schema（含嵌套属性、`$defs`），
   N 个工具即 N 份；v2 只有 `{ code: string }` 一份。
   接入 MCP / 浏览器等长尾工具后，差距从「数百」到「数千」token 不等。
2. **目录侧**：即便把目录文本算进去，v2 封顶约 2000 tokens，且未列出的靠 `search` 按需取；
   v1 没有预算，工具多则目录无限增长。
3. **时间侧**：目录作为稳定基线只出现一次并参与缓存，变化时才追加 delta；
   v1 的目录挂在工具 description 上，属于随请求装配的易变内容。
4. **分类提醒**：第 1、2 条是**真实减少发送量**；第 3 条主要是**降计费 / 提升缓存命中**的观感。

> 说明：本节只覆盖「工具目录与 CodeMode 收拢」这一项。System prompt（A）、指令增量（C）、
> 压缩（D）、缓存断点（E）、历史读取（F）分别见本系列对应篇章。

---

## 7. 可验证方法

1. **首轮请求的 tools 部分**：带一个 MCP server，分别抓两版首轮请求，比较
   `tools` 数组的 schema 体积（预计 v2 明显更小，且只有约 13 条）。
2. **`execute` 工具定义**：确认 v2 的 `inputSchema` 只有 `code` 一个字段；
   目录文本不在该 schema/description 里，而在 system 指令中。
3. **目录预算**：构造一个有几十个工具、描述很长的 MCP server，观察 v2 目录是否被截断并提示
   `The Code Mode tool catalog below is partial`，且 `search` 能找回未列出工具。
4. **增量**：会话中途新增/移除 MCP 工具，观察后续请求只追加小段 catalog delta
   （或仅 namespace 计数变化），而非整段目录。
5. **自带 CodeMode 的服务器**：接 `executor.sh`，确认其工具被直接暴露（不走 `execute`），
   验证 `mcp-codemode-exclusion` 与 `?codemode=false` 行为。
