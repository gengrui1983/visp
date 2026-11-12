# 独立验证（Independent Verifier）设计

日期：2026-11-12  
状态：已实现（工作区改动，未提交）

## 1. 问题

visp 完成较大的功能时，常见失败模式不是「代码写不出来」，而是：

1. 实现者把需求建立在一个与真实使用形态不符的假设上；
2. 实现者按同一假设编写测试，于是「测试全绿」只证明代码符合自己的设想；
3. 需求原文里写明的真实场景，没有被转化成任何测试。

这类问题的根源是**实现者自己验证自己**。只要验证者共享实现者的上下文（对话历史、
推理过程、自己写的测试），它就会沿着同一条思路确认「一切正常」。

本设计引入一个**固定的独立验证环节**：主 agent 完成一次修改了项目文件的任务后，
由一个没有参与实现、只看原始需求与改动的验证者，自行设计验收场景并实际执行，
最后给出带证据的通过/不通过结论；不通过则交回返工，轮数有上限。

## 2. 总体设计

### 2.1 触发条件

- 只对**根会话的用户回合**触发，子 agent 的完成不会单独触发验证。
- 回合内必须**成功修改过项目文件**：
  - `write_file` / `edit_file` / `multi_edit` / `apply_patch` / `notebook_edit` 的成功调用；
  - 子 agent（如 `fixer`）的写文件调用会沿 `parent_session_id` 上溯，归到根回合；
  - 只读回合（回答、搜索、分析）不触发。
- `[verifier].enabled = true`（默认）才触发。

### 2.2 隔离

验证者运行在一个**全新的 visp 子会话**中：

- `SessionManager::create_sub` 创建新 session，`history` 为空；
- 不传入实现者的 `AgentMessage`/推理历史；
- 输入只有三部分：
  1. 本轮用户的原始请求（`ClientMessage::UserInput.text`，返工期间保持不变）；
  2. 本轮成功修改的文件路径列表；
  3. `git status --porcelain` + `git diff` 的改动快照（截断到 24k 字符）。
- 验证者仍拥有正常工具能力（读文件、grep/glob、codegraph、bash 等），但
  `filter_tools_for_sub_agent(..., &[])` 会剔除 Agent 类工具，避免它再委托给别人。
- 验证者没有实现者的系统提示（它有自己的系统提示 + 项目级 system prompt）。

### 2.3 验证者工作协议

系统提示强制要求：

- 不信任实现者的测试或结论；
- 从原始需求自行推导验收场景；
- **实际执行**（读代码、跑命令、可写临时测试文件），不能只重跑实现者的测试，
  也不能只读 diff 给意见；
- 每条结论附证据：检查了什么、怎么检查、看到的结果；
- 结束前必须调用一次 `submit_verification`。

`submit_verification` 是专用工具（`visp-agent/src/verifier.rs`）：

```json
{
  "passed": false,
  "summary": "核心场景失败：仍是内存全量加载",
  "evidence": [
    {
      "check": "10 万行 CSV 按列名查询",
      "method": "构造大文件并实际运行查询",
      "result": "失败：OOM / 结果错误"
    }
  ]
}
```

工具严格校验 `summary` 非空、`evidence` 非空；首次提交生效，重复提交报错。
若模型始终不调用该工具，运行器读取最后一条 assistant 消息中的
`VERDICT: PASS|FAIL` 标记作为兜底。

验证报告区分「做完了」与「没做成」：

- `completed = true`：拿到了 `submit_verification` 或 `VERDICT` 结论；
- `completed = false`：超时、步数用尽、或始终没有结论。此时仍**保守判失败**
  （绝不当作通过），但结论会标记为 `inconclusive`，并且**不触发主 agent 返工**——
  「验证发现缺陷」才需要返工，「验证本身没做成」交给主 agent 修改没有依据。

### 2.4 回合状态机

`Orchestrator` 为每个根会话维护 `TurnVerificationState`：

```
UserInput
  └─ 新建回合状态（turn_id、原始请求、modified=false）
       ├─ ToolCallRequest/Result → 成功写文件 → modified=true，记录路径
       └─ 主 agent Done
            ├─ modified=false / 验证关闭 → end_root_turn（Done + Stop）
            └─ modified=true → done_withheld=true，启动第 1 轮验证
                 ├─ 通过 → end_root_turn（最终 Passed + Done + Stop）
                 ├─ 没做成（completed=false：超时/步数用尽/无结论）
                 │    └─ final Inconclusive（passed=false）+ Done + Stop，**不返工**
                 ├─ 发现缺陷且 rework_used < max_rounds
                 │    ├─ 发布 failed（非 final）+ rework
                 │    └─ 以「验证结论 + 证据」作为内部用户消息重启主 agent
                 └─ 发现缺陷且预算用尽 → final Exhausted（passed=false）+ Done + Stop
```

关键点：

- **root `Done` 被扣住**：主 agent 的事件转发任务不再直接转发 `AgentEvent::Done`；
  由 `Orchestrator::end_root_turn` 在验证收尾后统一发布。否则 ACP / `visp run`
  会在验证完成前就结束 turn，最终状态无法表达。
- 取消：`Cancel` 会标记 `cancelled` 并级联取消验证者子会话；迟到的验证结果按
  `turn_id` 丢弃，不再返工。
- 新用户消息会废弃上一回合仍在进行的验证（同样按 `turn_id` 丢弃迟到结果）。
- 验证没做成（无法解析模型、验证者超时、步数用尽、未提交结论）以
  `Inconclusive(is_final=true, passed=false)` 收尾：明确失败，但不返工。
- 返工回合自身执行错误仍以 `Failed(is_final=true)` 收尾。

### 2.5 对外事件模型

新增结构化事件（而不是拼接字符串）：

- core：`AgentEvent::Verification(VerificationUpdate)`
- proto：`ServerMessage.verification_update`（字段：`stage`、`round`、
  `max_rounds`、`summary`、`evidence[]`、`is_final`、`passed`、`session_id`、
  `agent_name`）
- `stage` 取值：`started | passed | failed | rework | exhausted | inconclusive`
  （`inconclusive` = 验证没做成，不是发现缺陷，绝不等于通过）

三个入口的展示/最终状态：

| 入口 | 进度/结论 | 最终状态 |
| --- | --- | --- |
| TUI | 主对话区新增 `LineType::Verification`：开始、失败、返工、通过/未通过/未完成 + 证据 | 最终结论行 + `Done`；未通过显示「已用尽 N 轮返工」，没做成显示「验证未完成」 |
| `visp run` | stderr 输出 `[verify]` 行（含证据）；验证者/子 agent 的命令审批由 `-y/--yes` 放行 | 退出码见下；每个值含义唯一 |
| ACP | 验证事件渲染为 `AgentMessageChunk` 文本（带证据） | `PromptResponse.stop_reason = end_turn`，`_meta["visp.verification.status"] = "passed" | "failed" | "incomplete"` |

`visp run` 的退出码：`0` 通过/未触发，`1` 运行时错误，`2` 命令行用法错误，
`3` daemon 流意外断开，`4` 完成但验证没做成，`5` 完成但验证发现缺陷，`130`
取消。**`2` 是 clap 参数解析失败时自身使用的退出码**（发生在 `run_once` 之前，
无法由它控制），因此固定保留给用法错误：`run_once` 的用法错误（如
`--session` 暂不支持、空 prompt）同样返回 `2`，而验证结论只用 `4`/`5`。
这样脚本拿到 `2` 时只会是「命令写错了」。

验证事件统一以**根 session_id** 发布，因此 TUI 主 tab/ACP 父 turn/headless 都能
直接看到；验证者自身的工具调用仍在 `verifier` 子 tab（TUI hidden tab）中可查。

## 3. 配置

```toml
[verifier]
enabled = true          # 默认 true
max_rounds = 2          # 首验失败后允许的返工轮数，默认 2
model = "Opencode/xxx"  # 验证者模型 key；缺省继承主会话模型
timeout_secs = 600      # 单轮验证超时
max_steps = 40          # 验证者最大 agentic 迭代次数
```

- 配置位于 `visp-config::VerifierSection`，字段均为 `Option<T>`，支持项目级
  `.visp/daemon.toml` 只覆盖显式写的字段（`merge_verifier_sections`）。
- 默认值在运行时解析（`is_enabled` / `effective_*`），避免默认配置向
  `daemon.toml` 写入空段。
- **不依赖模型名称做任何判断**：是否验证只看 `enabled`，用哪个模型只看显式
  `model` key（缺省继承会话模型）；`verifier` 也不要求模型名包含任何关键字。

## 4. 取舍

1. **扣住 Done 而不是增加「最终完成」事件**：现有 TUI/ACP/headless 都以 `Done`
   作为 turn 结束信号。扣住 Done 能让所有入口零改动地获得正确的收尾时机；
   代价是主 agent 结束后到验证结束期间，客户端仍显示运行中（这正是期望的
   「验证在进行」）。
2. **验证者作为子会话而非进程**：复用 SessionManager、工具审批、事件总线、
   取消级联；隔离靠「新 session + 只注入原始请求与改动」实现，而不是新进程。
3. **结构化工具提交结论**：比解析自然语言可靠；同时保留 `VERDICT:` 文本兜底，
   兜底失败一律判失败（保守）。
4. **返工通过内部用户消息实现**：复用主 agent 完整实现能力，不需要新的
   「修复」agent；验证结论与证据作为消息内容交回，原始需求仍保留在会话历史中。
5. **触发判定用成功写工具调用**：简单、可测、不依赖文件系统轮询；代价是纯
   `bash` 改文件（如 `sed -i`、`cat >`）不会被识别（见局限 1；实测影响较小）。
6. **区分「没做成」与「做失败」**：`VerificationReport.completed` 显式记录验证者
   是否给出了结论。只有 `completed && !passed` 才返工；`!completed` 判
   `Inconclusive` 并直接结束。两者都 `passed=false`，不会把没做成伪装成通过。
7. **验证者可使用 bash/write**：需求要求「写测试或运行命令都可以」；因此验证者
   可能创建临时测试文件，这些改动不在验证范围内（见局限）。

## 5. 测试

新增/覆盖的关键行为：

- `crates/visp-agent/src/orchestrator_verifier_tests.rs`（8 个编排层测试）：
  - 只读回合不触发验证（验证者 provider 调用次数 0）；
  - 修改文件后自动验证；验证者上下文只有 system + 一条用户消息，包含原始请求
    与改动文件，不含实现者 assistant 历史；
  - 最终 `Done` 晚于最终验证结论；
  - 验证失败 → 返工 → 复验通过（主 agent 收到含验证问题的用户消息）；
  - `max_rounds=1` 时恰好验证两次、返工一次，最终 `Exhausted(passed=false)`；
  - 验证者始终不给结论（`max_steps=1` 的真实形态）→ 只验证一次、主 agent
    不返工、最终 `Inconclusive(passed=false)`；
  - `enabled=false` 时完全不触发；
  - 子 agent 写文件正确映射到根回合。
  - 测试 provider 用「一条用户消息 + 多轮工具调用（read/write/文本）」的真实形态脚本。
- `crates/visp-agent/src/verifier_tests.rs`（10 个）：
  - `submit_verification` 参数校验、首次提交生效、重复提交拒绝；
  - prompt 包含原始请求/改动路径/diff 与隔离纪律；
  - 写文件工具识别、路径参数提取；
  - 默认配置值。
- `crates/visp-config/src/config_verifier_tests.rs`（4 个）：默认值、TOML 解析、
  项目级字段合并、空段不序列化。
- `crates/visp-core/src/verification_tests.rs`（7 个）：各阶段 final/passed 语义，
  含 `Inconclusive`（final + passed=false）。
- `crates/visp-acp/src/translate.rs`（4 个验证相关测试）：验证进度可见、最终
  pass/fail 记录、`incomplete` 与缺陷结论区分、子会话验证事件不被抑制。
- `crates/visp-tui/src/app_tests.rs`（3 个）：验证行含进度/结论/证据、路由到
  主 tab、`inconclusive` 显示为「验证未完成」。
- `crates/visp-daemon/src/service.rs`（2 个）：`AgentEvent::Verification` →
  proto 结构化字段（`exhausted`/`inconclusive`、`is_final`、`passed=false`、证据）。
- `crates/visp/src/main.rs`（2 个）：`visp run -y` 与「从 stdin 读 prompt」的
  CLI 解析。
- `crates/visp/src/run_cmd.rs`（13 个）：沿用 PR #7 的 `--yes`/stdin/取消/流断开
  测试，并新增验证结果测试：发现缺陷→退出码 5、没做成→退出码 4、通过→0、
  子会话（验证者）审批在 `--yes` 下被放行、stderr 含证据；以及
  `exit_codes_are_unique_and_keep_2_for_cli_usage_errors`（断言 2 保留给
  clap 的用法错误、各 `RunOutcome` 互不重复）与
  `clap_parse_failure_uses_the_reserved_usage_exit_code` /
  `unsupported_session_flag_is_a_usage_error`。
- `crates/visp-proto`：oneof 变体集合测试同步更新。

**先失败后实现**：上一轮的实现前，先提交测试并运行
`cargo test -p visp-config --no-run` 与 `cargo test -p visp-agent --no-run`，
分别因缺少 `DaemonConfig.verifier` / `merge_verifier_sections`、缺少
`Orchestrator::with_verifier_config` / `root_ancestor` / `AgentEvent::Verification`
而编译失败；随后实现使测试转绿。

本轮针对审查意见 1，先加入
`test_verifier_without_conclusion_does_not_rework_main_agent` 并运行：修改前失败
（观察到 3 次 `Started`，期望 1 次），再实现 `completed` / `Inconclusive` 后转绿。

最终校验（本轮，单条命令串行执行，全部加 `-j 4`）：

- `cargo test -j 4 -p visp-core -p visp-agent -p visp-acp -p visp-daemon -p visp-tui -p visp`：
  1226 passed, 0 failed
- `cargo clippy -j 4 --all-targets -- -D warnings`：通过
- `cargo fmt --all -- --check`：通过

## 6. 已知局限

1. **触发判定**：只统计成功的内置写文件工具；仅通过 `bash`（`sed -i`、
   `cat > file` 等）修改的文件不会触发验证。验证者自己创建临时测试文件也不会
   再次触发（避免递归验证）。该局限经实测影响较小：5 次真实运行中内置写文件
   工具调用分别为 9/9/41/12/35 次，`bash` 改文件为 0–5 次，内置工具始终占多数。
2. **改动快照范围**：`git diff` 是工作区全量 diff，可能包含本回合之前就存在的
   未提交改动；非 git 项目只能依赖改动文件列表，验证者需自行读文件。
3. **验证者能力边界**：验证者与主 agent 共用工具注册表（剔除 Agent 类工具），
   因此它也可能修改项目文件；它创建/修改的文件不会被还原。
4. **headless 审批**：`visp run` 没有交互审批通道。传 `-y/--yes` 时所有工具审批
   （含验证者和子 agent）各放行一次；不传时一律拒绝并在 stderr 提示。需要逐条
   交互审批的场景应使用 TUI。
5. **配置热重载**：`[verifier]` 在 daemon 启动时读取，`/reload` 不重新应用。
6. **最终状态与 hook**：验证未通过/未完成时会话级 `Stop` 的 `status` 仍为
   `completed`（执行成功、验证失败）；最终状态通过结构化验证事件/退出码/
   ACP `_meta`（`passed` / `failed` / `incomplete`）表达，未新增 hook 事件
   （hook 事件集是冻结契约）。
7. **重连回放**：验证事件不写入会话 history，TUI 断线重连后不会回放历史验证行；
   最终结论可通过会话消息中的返工提示间接看到。
