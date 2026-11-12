# 2026-11-12 `visp run` 改为 ACP 客户端

## 背景

旧实现把「无人值守执行一个任务」放在启动器 `visp` 里：`visp run` 自己拉起
daemon、直连 gRPC、创建会话、事件泵、审批与退出码处理全在 `crates/visp`。
这与仓库分工冲突：

- 这类能力已有 `visp-acp`（ACP agent，编辑器通过 ACP 驱动 visp）；
- `visp` 只应负责启动进程，不应包含会话/审批等业务逻辑。

本设计把非交互模式移到 `visp-tui`，并让它成为 **ACP 客户端**。

## 架构

```
用户/脚本
   │  visp run <args...>
   ▼
visp（启动器，纯进程启动，无业务逻辑）
   │  exec: visp-tui run <args...>（参数原样转发，透传退出码）
   ▼
visp-tui run（ACP 客户端）
   │  spawn 子进程 + ACP over stdio（JSON-RPC）
   ▼
visp-acp（ACP agent）
   │  gRPC（现有实现；默认自拉起 visp-daemon）
   ▼
visp-daemon
```

职责边界：

- `crates/visp`：识别第一个参数 `run`，`spawn visp-tui run <剩余参数...>`，
  继承 stdin/stdout/stderr，等待并透传退出码。Ctrl-C 期间不因 SIGINT 提前
  退出，而是等子进程收尾（见「Ctrl-C 与信号」）。不解析任务、不连 daemon、
  不处理审批、不改写参数。
- `crates/visp-tui`：`run` 子命令即 ACP 客户端。交互式 TUI 路径不变，仍直连
  daemon。
- `crates/visp-acp`：现有 ACP agent；另外新增可选的启动参数
  `--model/--temperature/--thinking-budget`，在 `session/new` 创建会话时作为
  初始 `LlmConfig` 传给 daemon（原因见「已知局限」）。

## ACP 消息流程

`visp-tui run` 使用 `agent-client-protocol` 的 client 端：

1. 启动 `visp-acp` 子进程（`AcpAgent`，stdio 传输）。测试可用
   `VISP_ACP_BIN` 指向假 agent。
2. `initialize`：协议版本 v1，客户端能力使用默认值（不声明
   `elicitation.form`，因此 visp-acp 对 LLM 提问走「降级 + 立即回填」路径，
   不会阻塞）。
3. `session/new`：`cwd` 为 `-p/--project` 规范化后的绝对路径；返回
   `sessionId`。
4. `session/prompt`：发送一个文本 `ContentBlock`。
5. 会话期间：
   - `session/update` 通知：
     - `agent_message_chunk` → stdout（原样，不额外加换行）；
     - `tool_call` → stderr `[tool #N] <title>`，并记录 toolCallId→title；
     - `tool_call_update` 且 `status = failed` → stderr
       `[tool error] <title>: <content>`；
     - 其他更新静默。
   - `session/request_permission` 请求：
     - `-y/--yes`：选择 `allow_once`，计数 granted；
     - 否则：选择 `reject_once`，stderr 提示
       `denied (use --yes to approve): ...`，计数 denied；
     - 找不到对应选项时回 `Cancelled`，保证 agent 不阻塞。
6. Ctrl-C：先发 `session/cancel` 通知，再等待 prompt 响应最多 10 秒；
   无论是否收到响应都按中断退出（超时不再无限等待）。
7. 收到 `session/prompt` 响应：按 `stopReason` 映射退出码；连接在响应前
   断开则映射为连接丢失。

## 退出码映射

| 退出码 | 含义 | 触发条件 |
|---:|---|---|
| 0 | 完成 | `stopReason = end_turn` |
| 1 | 运行错误 | agent 拒绝/达到限制、初始化/建会话失败、spawn 失败、非法项目路径等 |
| 2 | 命令行用法错误 | clap 参数解析失败；prompt 为空（stdin 为空或全空白） |
| 3 | 与 agent 的连接意外断开 | ACP 入站流在响应前 EOF（`incoming_transport_closed`） |
| 130 | 被中断 | Ctrl-C（先 cancel，后退出） |

每个退出码含义唯一；`Cancelled` 只有在客户端自己发过 cancel 时才算 130，
否则视为 agent 侧错误（1）。

## Ctrl-C 与信号

终端的 Ctrl-C 把 SIGINT 发给**前台进程组里的所有进程**，启动器和
`visp-tui` 都会收到。因此：

- `visp-tui` 收到 SIGINT 后按上一节的流程取消并收尾，最终以 130 退出；
- 启动器在 `spawn` 之前把 SIGINT 的处置设为**真实的空处理函数**，收到
  Ctrl-C 后不退出，继续 `wait`，等子进程真正结束后再把它的退出码（130）
  透传出去。

这样 shell 提示符一定在 `visp-tui` 收尾完成后才返回；取消期间正在执行的
工具不会留在后台。启动器只做信号处置 + 等待，不解析信号、不取消任何东西，
仍是纯进程启动。

实现细节与取舍：

- **不用 `SIG_IGN`。** 被忽略的处置会跨 `exec` 继承，子进程 `visp-tui`
  就再也收不到 Ctrl-C 了。自定义处理函数在 `exec` 时复位为默认处置，子进程
  不受影响（`crates/visp/tests/run_interrupt.rs` 的假 `visp-tui` 依赖
  `trap` 能生效来验证这一点）。
- **不转发信号。** 启动器与子进程同属一个进程组，Ctrl-C 已经直接送达子
  进程；再转发一次只会重复投递。启动器本身不做任何业务动作。
- **子进程被信号杀死时**（未安装处理函数），启动器的 `exit_code` 仍然把
  「信号 N」映射为 128+N，与之前一致。

## 测试

`crates/visp/tests/run_interrupt.rs` 用真实启动器复现终端语义：
`process_group(0)` 让 `visp run -y ...` 成为独立进程组，假 `visp-tui`
捕获 SIGINT 后延迟 400ms 才写标记并退出 130；测试向 `-pgid` 发 SIGINT，
断言启动器退出码为 130、标记已存在、且启动器的等待时间不短于子进程的收尾
时间。修复前启动器 4ms 内被 SIGINT 杀死（`status.code() == None`），测试
失败。

## 与原实现的差异

| 项目 | 旧 `visp run` | 新 `visp-tui run` |
|---|---|---|
| 协议 | 直接 gRPC | ACP over stdio（由 visp-tui 作为客户端） |
| 启动者 | `visp` 自己拉起 daemon | `visp-tui` 启动 `visp-acp`；`visp-acp` 拉起/连接 daemon |
| 参数透传 | `visp` 解析后重建 | `visp` 原样转发；`visp-tui` 解析 |
| 审批 | 内部 `UserQuery` 索引 | ACP `session/request_permission` 选项 |
| LLM 提问 | 固定「无用户」文本回填 | 不声明 elicitation 能力，visp-acp 走降级并自动回填 |
| 汇总 | 含 token usage | 不含 token usage（ACP v1 未在提示响应中暴露；见局限） |
| 子 agent 文本 | 按 sessionId 过滤 | visp-acp 已过滤，客户端只收到主 agent 文本 |
| 退出码 3 | daemon 流断开 | ACP 连接断开 |

## 已知局限

1. **模型/temperature/thinking budget 不是 ACP 协议能力。** ACP v1 没有
   模型选择请求，`session/new` 只有 `cwd`/MCP/`_meta`。因此这三个参数以
   `visp-acp` 的启动参数传递（visp 私有扩展），由 `visp-acp` 在建会话时写入
   `LlmConfig`。若直接使用其他 ACP agent，这些参数不会被识别。
2. **没有 token 用量汇总。** 旧实现从 daemon 的 `UsageInfo` 读取最后一次
   调用的 token；ACP v1 的 `PromptResponse` 不包含 usage，故只汇总工具调用
   与审批次数。
3. **审批粒度固定。** `--yes` 只选 `allow_once`，不会自动选择
   `allow_always`；无匹配选项时回 `Cancelled`（而不是猜测一个选项）。
4. **Ctrl-C 的取消是可选的优雅等待。** 发出 `session/cancel` 后最多等待
   10 秒；若 agent 不响应，仍以 130 退出，可能留下 agent 子进程由
   `AcpAgent` 的 drop/连接关闭清理。启动器会一直等到这一步结束（包括
   最多 10 秒的等待），因此 Ctrl-C 后 shell 可能延迟这么久才出现提示符。
   另外启动器只处理 SIGINT：若只对启动器 PID 发送 SIGINT（而不是对进程组，
   终端 Ctrl-C 属于后者），该信号在子进程运行期间会被忽略、不转发；SIGTERM
   等其它信号保持默认行为。
5. **只支持单轮 prompt。** 与旧实现一致：一次 `run` 创建新会话、发送一个
   prompt、然后退出；不支持 `-s/--session` 续跑。
6. **`visp run` 必须是第一个参数。** 启动器只识别 `visp run ...` 形式；
   `visp -p x run ...` 这类把全局参数放在子命令之前的写法不再透传。
