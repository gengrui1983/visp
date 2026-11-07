# CodeMode 详解：概念、实现与效果

> 对象：OpenCode 2.0.9 的 `@opencode/codemode` 包及其在 `packages/core` 中的接入
> 源码根：`coding_agent/opencode-2.0.9`
> 定位：概念科普 + 实现说明（含示例代码）+ 效果分析（含事例）
> 关联：本系列 [`02-tool-catalog-codemode.md`](./02-tool-catalog-codemode.md) 讨论其中的 token 效率部分

---

## 0. 速览

- **是什么**：CodeMode 是一种「用**一段程序**代替**多次工具调用**」的 agent 交互范式。模型不再一次调一个工具、等结果回对话，而是写一段受限 JavaScript，在**一次 `execute` 调用内**编排多个工具，最后返回一个 JSON 结果。
- **怎么实现**：`@opencode/codemode` 是一个**自研的 JavaScript 子集解释器**（不是沙箱真实 JS）。宿主用 `Tool.make` 描述工具、`CodeMode.make` 建运行时；模型写程序通过 `tools.<ns>.<tool>(input)` 调用，全局只有被允许的内建与宿主显式注入的扩展（如 `fetch`）。
- **什么效果**：① 工具暴露从「N 份 JSON Schema」降到「1 个 `execute` + 一段签名目录」；② 一次调用完成多步，**回合数大幅减少**；③ 中间结果留在程序内被裁剪，**只有最终值进入对话**；④ `Promise.all` 天然并发；⑤ 无网络/文件系统等默认能力，宿主可设资源上限。
- **代价**：程序源码本身占 token；单步简单任务用 CodeMode 反而绕。

参考出处：`packages/codemode/README.md`、`packages/codemode/AGENTS.md`、`packages/codemode/package.json`。

---

## 1. 什么是 CodeMode

### 1.1 核心思想

传统 tool-calling 把每个工具作为独立 function call 暴露给模型，交互是**多轮往返**的：

```
用户 → 模型: 调工具 A
模型 ← 环境: A 的结果（整段进入对话历史）
模型 → 用户: 再调工具 B
... 每个中间结果都留在上下文里 ...
```

CodeMode 把「工具」看成一套 **API**，让模型写**客户端代码**：

```
用户 → 模型: 写一段程序，内部自己调用 A、B、C
模型 ← 环境: 程序返回的最终结果
```

- 模型只看到一个 `execute` 工具，入参是 `{ code: string }`。
- 程序在受限运行时里执行，可以循环、条件、并发、`try/catch`，把中间结果**留在程序变量里**，只把最终值返回给模型。
- 与外界交互只通过宿主提供的 `tools` 树和少数扩展全局。

这个概念最早由 **Cloudflare** 提出（其 Code Mode 博客用 isolate 实现），本项目做了自己的解释器实现（`README.md:1-12`）。

### 1.2 一个直观对照

任务：**在仓库里找出所有 `TODO`，按文件汇总，只返回「文件 + 出现次数」**。

传统方式（多轮，中间结果全进对话）：

```
① grep("TODO")                      → 返回大量匹配行
② 模型手动读结果、发现需要更多上下文 → read(文件1)
③ 模型再 read(文件2) ...
④ 模型手工统计 → 最终答复
```

CodeMode 方式（一次 `execute`，中间数据不进对话）：

```js
// 模型生成的程序（概念示例）
const hits = await tools.grep({ pattern: "TODO", include: "*.ts" })
const byFile = new Map()
for (const line of hits.match.split("\n")) {
  if (!line) continue
  const file = line.split(":")[0]
  byFile.set(file, (byFile.get(file) ?? 0) + 1)
}
return [...byFile.entries()]
  .map(([file, count]) => ({ file, count }))
  .sort((a, b) => b.count - a.count)
```

模型只看到最终的「文件 + 次数」数组；中间的原始匹配行从未进入对话历史。

### 1.3 与传统工具调用对比

| 维度 | 传统工具调用 | CodeMode |
|---|---|---|
| 暴露单位 | 每个工具一份 JSON Schema | 1 个 `execute` + 一段签名目录 |
| 交互回合 | 每次工具调用一轮 | 一次调用内可完成多步 |
| 组合能力 | 靠模型逐条阅读结果推理 | 程序内循环 / 条件 / 变量 |
| 中间结果 | 全部进入对话上下文 | 留在程序内，只返回最终值 |
| 并发 | 需多轮或并行 tool calls | `Promise.all` 直接并发 |
| 错误处理 | 模型推理重试 | `try/catch` |
| 执行能力 | 仅限工具的粒度 | 工具 + 受限 JS 子集 |
| 安全边界 | 工具各自鉴权 | 无默认能力 + 工具内鉴权 + 资源上限 |

### 1.4 不是银弹

- **单步简单任务**（就调一个工具拿结果）用 CodeMode 反而多一层：模型要先写程序。
- **高度探索性任务**（下一步取决于上一步的语义判断）传统多轮可能更自然。
- 程序源码本身消耗一次 tool call 的 token；失败时要读诊断信息。

---

## 2. 实现方法（含示例代码）

### 2.1 分层架构

```
Host（opencode core / 你的应用）
  · 用 Tool.make / 插件注册工具
  · 决定每个工具 direct（原生工具列表）还是 codemode
  · 组装 inventory → 建 execute 工具 → 下发目录指令
  · 执行时把子工具调用路由回真实工具（含权限/持久化）
        ▲ Tool.make / Namespace.make      │ execute(code)
        │                                 ▼
@opencode/codemode（纯运行时，与宿主会话/渠道无关）
  · 受限 JS 解释器（Acorn 解析 + 白名单）
  · tools 树（trie）、search 发现、签名渲染（JSON Schema→TS）
  · 执行限制（超时/工具调用数/输出上限）、数据边界拷贝
```

职责分界（`packages/codemode/AGENTS.md:1-8`）：

- CodeMode **只管受限执行**：它**不知道**会话、渠道、对话模型，也不做授权决策。
- **授权、持久化、外部权限**都属于宿主；宿主不提供的工具，程序里就调不到。

### 2.2 宿主侧 API 示例

用 `Tool.make` 描述一个工具、`Namespace.make` 分组、`CodeMode.make` 建运行时、`runtime.execute` 跑程序：

```ts
import { CodeMode, Namespace, Tool } from "@opencode/codemode"
import { Effect, Schema } from "effect"

// 一个工具：模型可见的 description/签名来自这里；execute 里做真正的授权与副作用
const lookupOrder = Tool.make({
  description: "Look up an order by ID",
  input: Schema.Struct({ id: Schema.String }),
  output: Schema.Struct({ id: Schema.String, status: Schema.String }),
  execute: ({ id }) => Effect.succeed({ id, status: "open" }),
})

// 用 Namespace 给一组工具加描述（描述会参与 search 匹配）
const runtime = CodeMode.make({
  tools: {
    orders: Namespace.make({
      description: "Purchases, fulfillment, and shipment tracking",
      tools: { lookup: lookupOrder },
    }),
  },
  limits: { timeoutMs: 30_000 }, // 可选；默认不限
})

// 模型写的那段程序，由宿主代为执行
const result = await Effect.runPromise(
  runtime.execute(`
    const order = await tools.orders.lookup({ id: "order_42" })
    return { id: order.id, needsAttention: order.status !== "complete" }
  `),
)
// result: { ok: true, value: {...}, toolCalls: [...] } 或 { ok: false, error: {...} }
```

要点：

- `input` / `output` 既接受 **Effect Schema**（会真正校验与解码），也接受**纯 JSON Schema**（只用于生成签名）；不给 `output` 时签名是 `Promise<void>`（`packages/codemode/README.md:55-61`）。
- **描述与 schema 都是模型可见契约**；授权要写在 `execute` 里（`README.md:61`）。
- 一次性执行也可用 `CodeMode.execute({ ...options, code })`（`README.md:80-82`）。

### 2.3 模型侧：`execute` 工具与程序

宿主把运行时包装成一个模型可见的工具。它长这样（opencode core 的定义）：

- 名字固定 `execute`，`execute` 是保留名（`packages/core/src/tool.ts:313-314`）。
- 入参只有一个字段：`code: string`（`packages/codemode/src/codemode.ts:65`）。
- 出参：`{ output, toolCalls, files, error? }`（`packages/core/src/codemode/tool.ts:32-37`）。

模型生成程序的示例（调用多个 MCP 工具、并发、裁剪结果）：

```js
// 用 search 发现工具，或直接用目录里列出的精确路径
const issues = await tools.github.search_issues({ q: "is:open label:bug", per_page: 20 })

// 并发取详情；中间结果留在程序里，不逐条回对话
const details = await Promise.all(
  issues.items.slice(0, 5).map((issue) => tools.github.get_issue({ number: issue.number })),
)

// 只返回精炼后的结果
return details.map((d) => ({
  number: d.number,
  title: d.title,
  assignee: d.assignee?.login ?? null,
  comments: d.comments,
}))
```

程序语言规则（`README.md:14-24`）：

- **只 await 你在意的调用**：程序结束时仍在运行的调用会被中断。
- **REPL 式返回**：不写 `return` 时，最后一个顶层表达式的值就是结果；`undefined` 变 `null`。
- **只支持子集**：不支持的语法会返回带位置的 `UnsupportedSyntax` 诊断。
- 需要更多工具时用内建 `search(...)`（见 §2.4）。

### 2.4 运行时内部实现

**工具树与调用路径**（`packages/codemode/src/tool-runtime.ts`）：

- 工具按「点号路径」建 trie（`toolTrie`，`:126-146`）；调用时按段解析（`resolve`，`:327-339`）。
- 路径含非标识符段时用括号写法：`tools.context7["resolve-library-id"](...)`（`toolExpression`，`:108-113`）。
- 内建 `search` 还可被程序里同名声明遮蔽（`README.md:167-175`）。

**签名生成**（`tool-schema.ts`）：

- 把工具输入/输出的 JSON Schema 渲染成 TypeScript 风格签名，如
  `tools.orders.lookup(input: { id: string }): Promise<{ id: string; status: string }>`。
- **懒渲染**：`describeTool` 的 `signature` 是 getter（`tool-runtime.ts:169-184`），
  只有被目录列出或被 search 命中才真正渲染；渲染深度上限 `MAX_RENDER_DEPTH = 8`（`tool-schema.ts:25`）。

**search**（`tool-runtime.ts:89-107, 212-274`）：

- 入参 `{ query?, namespace?, limit?, offset? }`，默认 `limit = 10`。
- 排序打分：路径精确匹配权重最高，其次路径包含、描述包含、参数名匹配（`:238-262`）。
- 支持精确路径、按 namespace、空查询浏览、分页；**search 本身也算一次工具调用**。

**Hooks**（`tool-runtime.ts:55-81`）：

- 宿主可在每次工具/扩展调用前后挂 `tool.before` / `tool.after` / `extension.before` / `extension.after`；
  opencode core 用它来发进度事件给 TUI（`packages/core/src/codemode/tool.ts:143-176`）。

**数据边界**（`src/data.ts`、`interpreter-support.md`）：

- 程序返回值与工具入参按 **`JSON.stringify` 语义**穿过边界；例外：Promise 会被 await、
  `Set` 以数组穿越、`URLSearchParams` 以查询串穿越、`Error` 序列化为 `{ name, message, ...own }`、
  `Uint8Array` 被拒绝（提示先编码为文本）、`__proto__` 键被丢弃（防原型污染）；
  嵌套深度上限 `MAX_VALUE_DEPTH = 32`。

**执行限制**（`src/codemode.ts:21-35`、`interpreter/limits.ts`）：

| 限制 | 默认 | 作用 |
|---|---|---|
| `timeoutMs` | 不限 | 总执行时长（在解释器步与步之间触发） |
| `maxToolCalls` | 不限 | 被许可的工具调用数 |
| `maxOutputBytes` | 不限 | 保留的结果与日志大小 |

解释器还有内建上限：字符串 `2^24` 字符、数组 `10,000,000` 元素、同时挂起 `10,000` 个 Promise，
超出抛 `RangeError`。工具调用并发**不受限**。

**解释器与全局**（`interpreter/globals.ts`、`interpreter/limits.ts`、`interpreter-support.md`）：

- 用 Acorn 按最新 JS 语法解析，再用白名单**收紧**；TypeScript 专有语法直接拒绝。
- 顶层支持 `await` / `return`（程序在隐式 async 函数作用域内）。
- 可用内建：`Object` / `Array` / `Math` / `JSON` / `console` / `Promise` / `Date` / `RegExp` /
  `Map` / `Set` / `URL` / `URLSearchParams` / `Uint8Array` / `TextEncoder` / `TextDecoder` /
  `crypto` / `atob` / `btoa` 等。
- **不可用**：`Function` 构造器、定时器、`process`、文件系统、`import`/模块、
  以及宿主未显式注入的网络能力。

**扩展（Extension）**：宿主显式注入的全局函数（`src/extension.ts`），值只做拷贝、从不共享；
**扩展调用不算工具调用**。opencode core 用扩展注入 `fetch`（`packages/core/src/codemode/web.ts:40`，30s 超时）。

### 2.5 宿主如何接入（opencode 2.0.9 实例）

**分流**：工具在注册时用 `options.codemode` 决定去向（`packages/core/src/tool.ts:234-235`）：

```ts
// 进 CodeMode：默认 codemode=true
draft.add({
  name: "session_rename",
  description: "Rename a session, or omit sessionID to rename the current session...",
  input: RenameInput,
  output: RenameOutput,
  options: { namespace: "opencode", codemode: true },
  execute: (input, context) => { /* ... */ },
})

// 直连（保留在 provider 原生工具列表）：显式 codemode:false
// 见 packages/core/src/tool/plugin/read.ts:42 等 12 个核心工具
```

**组装**：`snapshot()` 构造 inventory，`CodeModeTool.create(...)` 生成 `execute` 工具；
其 `execute` 收到程序后，对每次子工具调用回调 `executeTool`（带插件 hooks 与权限）
（`packages/core/src/tool.ts:225-286`、`core/src/codemode/tool.ts:71-140`）。

**目录**：`CodeModeTool.catalog(inventory)` 生成目录，经 Instructions 体系下发
（`core/src/session/context.ts:145-153`）；渲染结果按 `(data, names)` 缓存复用
（`core/src/tool.ts:246-254`）。目录的预算与增量详见 [`02`](./02-tool-catalog-codemode.md)。

> 注意：core 建运行时**未设置** `limits`（`core/src/codemode/tool.ts:239`），
> 因此在 opencode 中 `timeoutMs` / `maxToolCalls` / `maxOutputBytes` 都是默认不限。

---
## 3. 产生的效果（含事例）

### 3.1 效果一：工具 schema 从「N 份」变「1 份」

**事例**：接入一个带 30+ 工具的 MCP server。

- 传统：模型工具列表里出现 30+ 个工具，每个携带**完整 JSON Schema**（含嵌套属性、`$defs`、描述）。
- CodeMode：列表里只有 `execute`，其 schema 是 `{ code: string }` 一个字段；
  工具以**一行签名**出现在文本目录里，目录还受预算封顶（`INLINE_BUDGET = 2000` tokens，
  `core/src/codemode/catalog.ts:50`），列不下的靠 `search` 取。

**量化参照**（量级示意，非精确实测）：

| | 传统 | CodeMode |
|---|---|---|
| 工具定义条数 | 30+ | 1（`execute`） |
| 每工具 schema | 数百 token 级 | 1 个 string 字段 |
| 目录文本 | 无（信息在 schema 里） | 封顶约 2000 tokens |
| 超出部分 | 全量暴露 | `search` 按需发现 |

更详细的数据与常量见 [`02`](./02-tool-catalog-codemode.md)。

### 3.2 效果二：回合数减少

**事例**：查 5 个 issue 的详情并汇总。

- 传统：
  1. 调 `search_issues`；
  2. 收到列表；
  3. 调 `get_issue(1)` … 5. `get_issue(5)`；
  6. 汇总。
  → **约 7 次模型回合**，每个结果都进入上下文。
- CodeMode：模型写一段程序（§2.3 的示例），一次 `execute`
  → **1 次工具调用**，5 个详情在程序内并发获取并裁剪。

### 3.3 效果三：结果裁剪发生在程序内（token 大头）

**事例**：一个工具返回 100 条记录，但只需要 top 5。

```js
const all = await tools.metrics.list({ limit: 100 })
return all.items
  .filter((x) => x.status === "failing")
  .sort((a, b) => b.errors - a.errors)
  .slice(0, 5)
  .map((x) => ({ id: x.id, errors: x.errors }))
```

- 传统：100 条原始记录**全部**进入对话历史，之后每一轮都在重复计费。
- CodeMode：只有 5 条进入对话。**这是实际使用中最可观的省 token 来源**——
  不是 schema 缩了多少，而是中间大结果不再沉积到历史。

### 3.4 效果四：并发与延迟

**事例**：需要各自独立地查 10 个 URL。

```js
const urls = ["https://a", "https://b", /* ... */ "https://j"]
const pages = await Promise.all(urls.map((u) => fetch(u).then((r) => r.text())))
return pages.map((p) => p.length) // 只返回长度
```

工具调用并发**不受限**（`README.md:191`），因此 10 次请求并行发出，而不是串行多轮。

### 3.5 效果五：安全与可控

- **默认无能力**：程序不提供网络/文件系统/进程/`import`；能做什么完全取决于宿主给了哪些工具与扩展
  （`README.md:7-9`、`AGENTS.md:24`）。
- **授权在工具里**：例如 MCP 工具在执行前先做权限断言（`core/src/tool/mcp.ts` 的 `permission.assert`）。
- **可控上限**：Web 侧的 `fetch` 扩展带 30s 超时（`core/src/codemode/web.ts:5,18`）；
  宿主还可设 `timeoutMs` / `maxToolCalls` / `maxOutputBytes`。

**事例**：把 `fetch` 作为扩展显式注入时，宿主可以把它实现成「只允许特定 origin / 限制响应大小」的版本——
CodeMode 本身不做策略，策略在宿主。

### 3.6 效果六：工具很多时仍可发现

**事例**：一个 MCP server 有 200 个工具，目录放不下。

- 目录只列出每个 namespace 的计数 + 若干条最短的签名，并提示目录是 partial。
- 模型用 `search("create issue")` 或按 namespace 搜索，拿到精确路径与完整签名后再调用。

### 3.7 什么时候得不偿失

- 只调一个简单工具就能回答 → 直接调用更省。
- 需要边看结果边决定语义（探索、调试）→ 传统多轮更灵活。
- 工具不稳定、报错信息很长 → 程序失败的诊断也可能消耗 token。

---

## 4. 小结与延伸

- **概念**：用程序组合工具，一次调用完成多步；中间数据留在程序内。
- **实现**：宿主描述工具（`Tool.make`）→ 建运行时（`CodeMode.make`）→ 包装成 `execute` →
  跑受限 JS（白名单解释器）→ 子调用路由回宿主工具；数据在 JSON 边界拷贝，资源可设限。
- **效果**：schema 收拢、回合减少、结果裁剪、并发、默认无能力；
  代价是源码与诊断 token、以及简单任务的多余开销。

延伸阅读：

- 本系列 [`00-overview.md`](./00-overview.md)：1.18.31 vs 2.0.9 的六大机制差异。
- 本系列 [`02-tool-catalog-codemode.md`](./02-tool-catalog-codemode.md)：工具目录与预算、增量、search 的细节。
- 源码：`packages/codemode/README.md`、`packages/codemode/AGENTS.md`、`packages/codemode/interpreter-support.md`。

