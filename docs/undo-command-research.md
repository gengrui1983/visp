# `/undo` 命令实现调查：opencode-2.0.9 vs codex-main

> 调查日期：2026-11-01
> 代码版本：`../opencode-2.0.9`、`../codex-main`（工作目录快照）

## 结论速览

| 项目 | 是否有 `/undo` | 核心机制 | 文件系统回滚 | 会话记录回滚 |
|------|----------------|----------|--------------|--------------|
| opencode-2.0.9 | ✅ 有（TUI slash 命令 `session.undo`） | **Snapshot（影子 git 仓库）+ Revert 三阶段（stage/commit/clear）** | ✅ 通过快照 restore | ✅ 持久层按 seq 删除 |
| codex-main | ❌ 无 `/undo` slash 命令 | **ThreadRollback 协议操作（replay-based 重放截断）** | ❌ 明确不回滚磁盘 | ⚠️ 仅内存/重放视图，rollout 原始记录保留 |

---

## 一、opencode-2.0.9 的 `/undo`

opencode 的 undo 是**完整闭环**：既回滚对话消息，也回滚文件改动，且支持 redo。

### 1. TUI 入口（packages/tui/src/routes/session/index.tsx:899-929）

```ts
{
  title: "Undo previous message",
  id: "session.undo",
  group: "Session",
  slash: { name: "undo" },
  run: () => {
    // 1. 找到 revert 边界之前最后一条非空 user 消息
    const message = messagesBeforeRevert().findLast(
      (m): m is SessionMessageUser => m.type === "user" && !!m.text.trim(),
    )
    if (!message) { toast.show({ message: "Nothing to undo", ... }); return }
    // 2. 若该消息还在 pending（未投递），先取消投递
    if (pendingDeliveries().has(message.id)) {
      if (!(await mutatePending("cancel", message.id))) return
    } else {
      // 3. 已在会话中：中断运行 → 等待结束 → 调用 revert.stage
      await client.api.session.interrupt({ sessionID })
      await client.api.session.wait({ sessionID })
      await client.api.session.revert.stage({ sessionID, messageID: message.id })
    }
    // 4. 把被撤销的那条 prompt 内容重新填回输入框（可编辑后重发）
    target?.set({ ...projectedPromptInput(message), pasted: [] })
  },
}
```

配套的 `/redo`（同文件 932-945 行）：调用 `session.revert.clear` 恢复到撤销前状态；`enabled` 取决于 `session()?.revert?.messageID` 是否存在。

TUI 侧的显示逻辑（index.tsx:163-177）：

```ts
const messagesBeforeRevert = () => {
  const messageID = session()?.revert?.messageID
  if (!messageID) return messages()
  const index = messages().findIndex((message) => message.id === messageID)
  return index === -1 ? messages() : messages().slice(0, index)
}
```

即 undo 处于"staged（暂存）"状态时，UI 只渲染边界之前的消息——**消息此时并未真正删除，只是被视图层裁剪**，因此 redo 可以无损恢复。

### 2. 服务端 API（packages/server/src/handlers/session.ts:453-496）

三个 HTTP API 端点，直接映射到 core 的 Session Service：

| API | 功能 | 错误处理 |
|-----|------|----------|
| `session.revert.stage` | 暂存一次撤销（messageID 为边界） | NotFound / MessageNotFound / Busy / Snapshot.Error |
| `session.revert.clear` | 清除暂存（= redo），恢复文件 | NotFound / Busy / Snapshot.Error |
| `session.revert.commit` | 提交撤销，真正截断消息 | NotFound / Busy |

所有 revert 操作都会先检查 `execution.isActive(sessionID)`，会话运行中一律返回 `BusyError`（packages/core/src/session/session.ts:313-339）。

### 3. core 实现（packages/core/src/session/revert.ts）

#### stage —— 暂存撤销（revert.ts:23-59）

```
1. original = session.revert?.snapshot ?? snapshot.capture()
   // 若已在撤销流程中，复用最初的快照；否则先给当前状态拍快照
2. plan(database, { sessionID, messageID })
   // 找到边界消息的 seq，收集其后所有 assistant 消息
   // 每条 assistant 消息自带 message.snapshot.start（该消息开始时的快照树 ID）
   // 和 message.snapshot.files（该消息改动的文件列表）
   // → 得到 Map<文件路径, 目标快照树>（先到先得，以最早的快照为准）
3. restore = 边界前已暂存文件 + 上述计划文件
   → snapshot.restore(restore)   // 把每个文件回滚到其最早快照时的内容
4. files = snapshot.diff(from: original, to: 当前快照, paths)
   // 计算回滚后与初始快照的逐文件 diff，作为"撤销了什么"的记录
5. 持久化 revert = { messageID, snapshot: original, files }
   → bus.publish(SessionEvent.RevertEvent.Staged)
```

`plan` 的关键（revert.ts:86-116）：只处理 `seq > boundary.seq` 的 assistant 消息，且跳过没有 `snapshot.start` 的消息。**每条 assistant 消息在开始生成时就记录了当时的文件快照树**（`message.snapshot.start`），这是文件级回滚的依据。

#### clear —— 清除暂存 = redo（revert.ts:61-76）

把 `session.revert.files` 中的每个文件 restore 回 `session.revert.snapshot`（即 undo 前的原始快照），然后发布 `RevertEvent.Cleared`。

#### commit —— 提交撤销（revert.ts:78-84 + projector.ts:731-770）

commit 只发布一个 `RevertEvent.Committed` 事件，真正的截断由 projector（事件溯源投影层）完成：

```ts
// packages/core/src/session/projector.ts
yield* bus.project(SessionEvent.RevertEvent.Committed, (event) => Effect.gen(function* () {
  // 1. 找到边界消息的 seq
  const boundary = ... where id = event.data.to
  // 2. 删除 seq >= boundary 的所有消息（含边界消息本身）
  yield* db.delete(SessionMessageTable)
    .where(session_id = X AND seq >= boundary.seq)
  // 3. 删除 seq >= boundary 的所有 inbox 排队项
  yield* db.delete(SessionInboxTable)
    .where(session_id = X AND enqueued_seq >= boundary.seq)
  // 4. 清除 session.revert 字段
  yield* db.update(SessionTable).set({ revert: null })
  // 5. 重置指令状态
  yield* InstructionState.reset(db, sessionID)
}))
```

同理，`Staged` 事件把 revert 元数据写入 `SessionTable.revert` 列，`Cleared` 事件把它置 null（projector.ts:709-729）。

### 4. commit 的触发时机（packages/core/src/session/session.ts:159, 245）

撤销是"暂存"状态，**用户发出下一条新 prompt 时自动 commit**：

```ts
// session.ts:159 (prompt 流程)
// Commit a staged revert only after preparation succeeds, before admitting new work.
if (session.revert) yield* SessionRevert.commit(bus, session)
```

即：undo → 界面裁剪 + 文件已回滚 → 用户编辑重发 → commit 真正删除旧消息 → 新对话继续。compact 流程同样会先 commit（session.ts:245）。

### 5. Snapshot 底层：影子 git 仓库（packages/core/src/snapshot.ts）

快照系统是文件回滚的基石：

- **存储位置**：项目 worktree 内容被 `git write-tree` 写入一个**独立于用户 .git 的影子仓库**，位于 `<global.data>/snapshot/<project.id>/<hash(worktree)>`（snapshot.ts:83-96）。不污染用户的 git 历史。
- **capture**：对项目目录做 `git add -A`（到临时 index）+ `write-tree`，得到内容寻址的 TreeID；限制 untracked 文件最大 2MB；失败时 best-effort 返回 undefined 并仅告警（snapshot.ts:112-133）。
- **restore**：按 `Map<相对路径, TreeID>` 逐文件 `git tree.restore`；路径不在目标树中则删除该文件；路径逃逸项目目录直接报错（snapshot.ts:196-215）。
- **diff**：`git diff --name-status/--numstat/--unified=N` 三次批量调用，拼装结构化 per-file diff；被用户 .gitignore 忽略的路径会被过滤掉（snapshot.ts:154-181）。
- **开关**：`location.vcs?.type === "git"` 且未被工具（editor）禁用时才启用；非 git 项目快照能力关闭。

### 6. 数据模型

`SessionSchema.Info["revert"]`（session.ts:212）持久化在 session 表：

```ts
revert: {
  messageID: string    // 撤销边界（将删除此消息及其后所有消息）
  snapshot?: string    // undo 前的初始快照 TreeID（redo 用）
  files?: { file, status, additions, deletions, patch? }[]  // 撤销涉及的文件 diff
}
```

---

## 二、codex-main：没有 `/undo`

### 1. 确认不存在

- TUI 的 `SlashCommand` 枚举（codex-rs/tui/src/slash_command.rs:12+）中**没有 Undo/Rewind 变体**；grep `Undo|Rewind` 在 tui 源码中零命中。
- 全仓 grep "undo" 只命中注释、无关字符串（如 "Cannot be undone. Subagent threads will also be deleted."，slash_dispatch.rs:216）。

### 2. 最接近的机制：`ThreadRollback` 协议操作

协议定义（codex-rs/protocol/src/protocol.rs:721-723）：

```rust
/// Request Codex to drop the last N user turns from in-memory context.
///
/// This does not attempt to revert local filesystem changes. Clients are
/// responsible for undoing any edits on disk.
ThreadRollback { num_turns: u32 },
```

特点：

- **只回滚对话上下文，不回滚文件**——文档注释明确声明磁盘编辑由客户端自行负责。
- 暴露为 app-server 的 JSON-RPC 方法 `thread/rollback`（app-server-protocol/src/protocol/common.rs:690），且 v2 协议中已标记 **DEPRECATED**（app-server-protocol/src/protocol/v2/thread.rs:1219）。
- **未接入 TUI**：没有任何 slash 命令映射到它，只有 IDE/Desktop 等外部客户端可调用。

#### 处理流程（codex-rs/core/src/session/handlers.rs:239+ `thread_rollback`）

```
1. 校验 num_turns >= 1，否则发 Error 事件（ThreadRollbackFailed）
2. 校验当前没有活跃 turn（active_turn 为空），否则报错
3. 取 live_thread（持久化线程），flush() 确保落盘
4. load_history() 读取完整 rollout 历史
5. 构造 ThreadRolledBack { num_turns } 标记事件，append 到历史末尾
6. 用完整 items 调用 apply_rollout_reconstruction() 重放重建内存历史
```

#### 重放截断的实现

- **内存历史**（session/rollout_reconstruction.rs:370-372）：重放时遇到 `ThreadRolledBack(rollback)` 标记就执行 `history.drop_last_n_user_turns(rollback.num_turns)`——按 user turn 粒度从内存历史尾部丢弃。
- **rollout 索引**（thread_rollout_truncation.rs:36-63）：`user_message_positions_in_rollout` 在扫描 rollout 时同样应用 `ThreadRolledBack` 标记（`user_positions.truncate(len - num_turns)`），保证后续索引（resume/fork 定位 turn 边界）使用的是"回滚后"的有效历史，而非原始流。

关键设计：**rollout（持久化记录）保持不变**，回滚只是往 rollout 里追加一个标记事件 + 在重放/索引时应用该标记。这是一种事件溯源式的"软删除"。

### 3. TUI 内用户可用的相近命令

| 命令 | 位置 | 行为 |
|------|------|------|
| `/fork` | slash_dispatch.rs:246-249 | 发 `AppEvent::ForkCurrentSession`，从当前对话分叉出新对话（原对话不动） |
| `/resume` | slash_dispatch.rs:243-245 | 打开会话选择器，恢复历史会话 |
| `/new`、`/clear` | slash_dispatch.rs:180, 240 | 开新对话/清屏 |
| `/compact` | slash_dispatch.rs:264 | 压缩上下文（非回退） |

codex 的哲学是：对话层面用 fork/resume + ThreadRollback（协议层、软删除），文件层面**不做自动 undo**——apply_patch 的改动不可由系统一键撤销，依赖用户 git 或客户端自行处理。

---

## 三、设计对比与可借鉴点

| 维度 | opencode | codex |
|------|----------|-------|
| 回滚粒度 | 单条 user 消息（边界 + 其后全部） | N 个 user turn |
| 文件回滚 | ✅ 影子 git 快照，逐文件 restore 到"最早触及它的 assistant 消息开始时"的状态 | ❌ 明确不做 |
| 持久层处理 | staged 期间软裁剪（视图层），commit 时硬删除（DB 按 seq 删） | rollout 永不删，追加标记事件，重放时截断内存 |
| redo | ✅ clear = 恢复原状（文件 + 视图） | ❌ 无 |
| 并发保护 | 会话活跃时 BusyError；stage 前先 interrupt + wait | 有活跃 turn 时报错 |
| 状态机 | stage → (用户重发自动) commit / clear(=redo) | 一次性操作，无状态机 |
| 暴露方式 | TUI slash + HTTP API + SDK | 仅 JSON-RPC（且已 deprecated），TUI 无入口 |

**opencode 实现的三点核心设计**：

1. **每条 assistant 消息自带 `snapshot.start` + `snapshot.files`**：生成前拍快照、生成中记录改动文件，使"精确回滚到该消息之前"成为可能，无需全局快照链。
2. **两阶段撤销（stage/commit）**：undo 后消息先"隐藏"不删除，给用户 redo 的余地；用户确认（发出新 prompt）才物理删除。文件则立即回滚（因为用户马上要基于回滚后的工作区编辑）。
3. **undo 后把原 prompt 回填输入框**：撤销的意图通常就是"修改后重试"，直接 `projectedPromptInput(message)` 回填，减少一次输入。

**codex 的取舍**：不回滚文件是刻意的（注释写明），因为自动恢复文件在用户已手动改动工作区时风险大；对话回滚用"追加标记 + 重放截断"保持 rollout 可审计。
