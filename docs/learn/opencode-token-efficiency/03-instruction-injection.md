# 03. 指令文件注入：初始 vs 增量（总览项 C）

> 分析对象：`../opencode-1.18.31`、`../opencode-2.0.9`（均为无 `.git` 快照）
> 对应总览：[`00-overview.md`](./00-overview.md) 的 C 项
> 方法：直接源码比对（子 agent 委派因 API key 不可用）
> 结论日期：2026-09

## 0. 一句话结论

v1 **每个 LLM step** 重新发现、重读、全文重发 AGENTS.md / CLAUDE.md / `config.instructions`（含远程 URL 抓取）；
v2 把指令源做成 **keyed + 哈希 + 持久化的 Instructions 体系**：全文只在会话「epoch 基线」里出现一次（稳定可缓存），
之后文件变化时**只追加小段 delta**（优先 unified diff），且压缩时基线随 epoch 前移。

---

## 1. v1（1.18.31）：每 step 全文重读重发

### 1.1 发现与读取

`packages/opencode/src/session/instruction.ts`：

- 文件清单（`:60-68`）：全局 `~/.config/opencode/AGENTS.md`、`~/.claude/CLAUDE.md`（可用 flag 关），
  项目级 `AGENTS.md` / `CLAUDE.md` / `CONTEXT.md`（deprecated）**findUp 取第一层命中**（`:122-133`），
  外加 `config.instructions` 的 glob 展开（`:135-150`）；
- `system()`（`:155-169`）：把上述文件**全部重读一遍**（`concurrency: 8`），
  远程 URL（`http(s)://` 开头的 `config.instructions`）**每个 step 重新抓取**（5s 超时，`:95-103`），
  返回 `Instructions from: <path>\n<全文>` 数组。

### 1.2 每个 step 调一次

`packages/opencode/src/session/prompt.ts:1257-1269`（step 循环体内）：

```
const [skills, env, instructions, ...] = yield* Effect.all([
  ..., instruction.system().pipe(Effect.orDie), ...
])
const system = [...env, ...instructions, ...]
```

即：**每个 LLM step**（一次用户消息后的每次模型往返）都把指令全文塞进 system 数组。
对大规则文件的项目，每个 step 重复发送同样几千 token。

### 1.3 唯一的去重：`claims`

`instruction.ts:70-77, 179-221`：`claims: Map<MessageID, Set<string>>` 只服务于
「read 工具读取文件时顺带发现的上层目录指令文件」——按**assistant 消息 ID** 记录已附加过的路径，
同一条消息内不重复；跨消息、跨 step 的全局指令集**没有任何去重或基线**。

---

## 2. v2（2.0.9）：keyed 指令源 + 哈希基线 + 增量渲染

### 2.1 Instructions 抽象（`packages/core/src/instructions/index.ts`）

每个指令源是一个 `Source`（`:33-39`）：

- `key`：稳定标识（如 `core/instructions`、`core/environment`、`core/codemode`、`core/mcp-guidance`）；
- `read`：产出**canonical JSON**（键排序的确定性序列化，`:243-250`）；
- 渲染三件套：`initial(current)` / `changed(previous, current)` / `removed(previous)`。

 Admission（`:142-159`）：对每个源算 **sha256 哈希**（`hash()`，`:194-196`），
 与会话存储的 `current_values` 比较——哈希没变 → 不产生任何输出；
 变了 → 进入 delta。

### 2.2 持久化状态（`session/instruction-state.ts`）

- 每会话一行状态：`initial_values` / `current_values`（哈希 → blob 表引用），见 `initialize/apply`（`:89-137`）；
- `observe/commit`（`:21-87`）：读所有源 → 与存储值 diff → 变化时渲染 delta 文本，
  经 `SessionEvent.InstructionsUpdated` 发布（文本冻结进 durable 事件，`:44-60`）；
- 投影器把该事件写成 transcript 中一条 **system 类型消息**（`session/projector.ts:666-671`）——
  delta 按**时间顺序**进入历史一次，之后就是普通历史；
- `advanceEpoch`（`:139-154`）：压缩完成时把 `initial_values ← current_values` 并前移 epoch 起点
  （新基线从压缩点生效）；
- fork 会话用父会话 `current` 播种基线（`:176-182`）。

### 2.3 AGENTS.md 源：变更渲染成 diff

`packages/core/src/instruction-discovery.ts`（key `core/instructions`）：

- 发现由内部插件向 `files: Map<path, content>` 贡献（全局 → 项目近→远，`:33-36` 注释）；
- `initial`：`Instructions from: <path>\n<全文>`，多个文件 `\n\n` 连接（`:123-125`）；
- `renderUpdate`（`:127-146`）：
  - 移除 → `The instructions from <path> no longer apply.`；
  - 新增 → `New instructions apply from:\n<全文>`；
  - 变更 → 用 `diff` 库生成 **unified diff（context 3）**，包在 ```diff 代码块里；
    **仅当 diff 比全文替换更长时**才退回 `The instructions changed:\n<全文>`（`:141-145`）；
- 源消失 → `Previously loaded instructions no longer apply.`（`:120`）。

### 2.4 其他源同样增量

- **CodeMode 工具目录**：`codemode/instructions.ts:34-126`——目录完整时只发新增/变更/移除的 listing；
  目录不完整（超预算）时退化为「namespace 计数变化」的小段提示
  （`New tool namespaces are available: ...` / `... now has N tools`），并提示重新 `search`；
- **MCP server 指引**：`mcp/instructions.ts:32-55`——新增/移除渲染小 delta，内容变化才整段重发；
- **环境/日期**：`instructions/builtins.ts:40-55`——日期跨天只发 `Today's date is now: ...`。

### 2.5 组装与发送

- `session/context.ts:145-153`：`Instructions.combine([...])` 得到源列表；
- `session/history.ts:118-135` `entriesForRunner`：事务内同时取
  **epoch 基线文本**（`InstructionState.initial`，从 blob 表重放 `initial_values`）与消息；
- `session/model-request.ts:83-91`：基线作为 **system 第 2 部分**随每个请求发送（字节冻结）。

---

## 3. 代码走读（关键片段）

**哈希准入**（`instructions/index.ts:147-158`）：

```
const next = hash(entry.value)            // sha256(canonical JSON)
if (previous?.[entry.key] === next) continue   // 没变 → 零输出
delta[entry.key] = next
blobs[next] = entry.value
```

**增量渲染入口**（`instructions/index.ts:171-192` `renderUpdate`）：
只对出现在 delta 里的 key 调 `source.changed(previous, current)`，各源的 delta 文本 `\n\n` 连接。

**AGENTS.md 变更 delta**（`instruction-discovery.ts:134-141`）：

```
The instructions from /path/AGENTS.md changed. Here's the diff:
```diff
@@ -12,7 +12,9 @@
（context 3 的 unified diff）
```
```

---

## 4. 请求长什么样

### 4.1 第一轮（v2）

- system[2]（epoch 基线）包含：`<env>` + 日期 + **AGENTS.md 全文**（`Instructions from: ...`）+
  CodeMode 目录 + skills + MCP 指引；
- transcript 无指令相关消息。

### 4.2 第二轮（无变化）

- system 与第一轮**逐字节相同**（基线冻结）；
- transcript 只有对话消息。指令相关 token = 基线一次（此后走缓存）。

### 4.3 第二轮（AGENTS.md 在两轮之间被编辑）

- system **仍然不变**（旧基线继续作为稳定前缀，缓存不被打破）；
- transcript 中、在新 user 消息之前，多出一条一次性 system 消息：

  ```
  The instructions from /path/AGENTS.md changed. Here's the diff:
  ```diff
  @@ -12,7 +12,9 @@
  + 新增的规则行
  ```
  ```

- 之后每轮请求都携带这条历史 delta（成为历史的一部分），system 里的旧全文保持不动，
  直到下一次压缩：`advanceEpoch` 把 current 提升为新基线，epoch 前移。

### 4.4 v1 对照

- **每一轮** system[1] 都重新包含 AGENTS.md 全文（`prompt.ts:1260`）；
- 文件没变时是「同样的大文本重复发送」（依赖 provider 侧缓存抵扣，且缓存为条件式）；
- 文件变了 → system 整块变化 → 前缀缓存失效重写；
- 远程 URL 指令每 step 重新 HTTP 抓取（最多 5s 阻塞）。

---

## 5. 常量与量化

| 项 | v1 | v2 | 位置 |
|---|---|---|---|
| 注入时机 | 每 LLM step | epoch 基线一次 + 变化时 delta | `prompt.ts:1260` / `model-request.ts:87` |
| 文件读取 | 每 step 全量重读 | 发现插件维护 + blob 持久化 | `instruction.ts:162` / `instruction-discovery.ts:38-56` |
| 远程 URL | 每 step 重新抓取（5s 超时） | —（v2 未见 per-step 抓取路径） | `instruction.ts:95-103` |
| 去重范围 | 仅单消息内子目录指令（claims） | 全部 keyed 源（sha256） | `instruction.ts:74` / `instructions/index.ts:194` |
| 变更下发 | system 整块变化 | unified diff / 全文 / 移除声明 | `instruction-discovery.ts:127-146` |
| 基线生命周期 | 无 | epoch：压缩时前移 | `instruction-state.ts:139-154` |
| delta 载体 | 无 | durable 事件 → system 类型消息 | `instruction-state.ts:47-60`、`projector.ts:666` |

**token 账**：设 AGENTS.md 5 KB（≈1.3K tokens）、会话 30 step——
v1 发送 30 × 1.3K ≈ 39K tokens（缓存抵扣后仍需反复写入/读取）；
v2 基线 1 次 + 假设中途 2 次小改（diff 各几十 token）≈ 1.4K tokens 进上下文。
差异随会话长度与规则文件大小线性放大（真实减少 + 缓存友好）。

---

## 6. 可验证方法

1. **不变场景**：长会话多轮对话，抓每轮请求——v2 的 system[2] 应逐字节相同；
   v1 每轮 system 都含 AGENTS.md 全文。
2. **编辑场景**：会话中途修改 AGENTS.md（改几行），下一轮请求应出现
   `The instructions from ... changed. Here's the diff:` 的 system 消息，且内容是 diff 而非全文；
   大幅重写文件时应退化为 `The instructions changed:\n<全文>`。
3. **删除场景**：删除被发现的指令文件，应出现
   `The instructions from <path> no longer apply.`。
4. **远程指令**：v1 给 `config.instructions` 配一个 http URL 并抓包——每个 step 都有 GET；
   v2 无此行为。
5. **压缩后**：触发一次 compaction，观察后续请求的基线变为「编辑后」的全文
   （epoch 前移），旧 delta 消息留在压缩边界之前的历史里。
