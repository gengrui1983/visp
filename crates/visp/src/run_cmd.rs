//! `visp run`: one-shot, non-interactive mode.
//!
//! Starts the daemon like the TUI launcher does, creates one session, sends a
//! single prompt and streams the result, then shuts the daemon down. Meant for
//! scripts, CI and unattended troubleshooting.
//!
//! - stdout: the main agent's answer text only (pipe-friendly).
//! - stderr: progress (tool calls, tool errors, status, usage).
//! - Tool approvals cannot be asked interactively: `--yes` approves every
//!   request once; without it every request is denied (the agent is told and
//!   can continue without that tool).
//! - Questions with options from the model get a fixed "no user available"
//!   answer so the loop never blocks.
//!
//! Exit codes (each value has exactly one meaning):
//! - 0: the main agent finished and verification passed (or was not triggered)
//! - 1: the main agent (or the launcher) hit a runtime error
//! - 2: command-line usage error (clap's own exit code for parse failures,
//!   also used for unsupported flag combinations and a missing prompt). `run`
//!   results never reuse it, so scripts can tell a bad command from a verdict.
//! - 3: the daemon stream closed unexpectedly
//! - 4: the main agent finished but the verifier could not complete verification
//!   (timeout, step limit, no structured conclusion). This is **not** a pass and
//!   is distinct from 5: no code defect was found.
//! - 5: the main agent finished but independent verification rejected the result
//! - 130: interrupted

use std::future::Future;
use std::io::Write;

use tokio::sync::mpsc;
use visp_proto::visp::{
    Cancel, ClientMessage, ServerMessage, UserInput, UserResponse, client_message, server_message,
};

/// Answer given to model questions (options list), since nobody can reply.
pub const NO_USER_ANSWER: &str = "No interactive user is available (this is `visp run`). \
Decide yourself using your best judgement, state the assumption you made, and continue.";

/// How long to wait for the daemon to acknowledge a cancel after Ctrl-C.
const CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Exit codes of `visp run`. Each value has exactly one meaning. `EXIT_USAGE`
/// is also what clap returns when the command line cannot be parsed (that
/// happens before `run_once`), so no `RunOutcome` may use it.
pub const EXIT_OK: i32 = 0;
/// The main agent (or the launcher around it) hit a runtime error.
pub const EXIT_AGENT_ERROR: i32 = 1;
/// Command-line usage error: clap parse failure, unsupported flag
/// combination, or a missing prompt.
pub const EXIT_USAGE: i32 = 2;
/// The daemon's chat stream closed before the turn finished.
pub const EXIT_STREAM_CLOSED: i32 = 3;
/// The main agent finished but the verifier could not complete verification.
pub const EXIT_VERIFICATION_INCOMPLETE: i32 = 4;
/// The main agent finished but independent verification rejected the result.
pub const EXIT_VERIFICATION_FAILED: i32 = 5;
/// Interrupted by the user (Ctrl-C).
pub const EXIT_INTERRUPTED: i32 = 130;

/// Selected index for a tool approval (`UserQuery` with no options).
/// Same mapping as the TUI / visp-acp: 0 = allow once, 1 = deny.
const APPROVE_ONCE: i32 = 0;
const DENY: i32 = 1;

/// Result of one `visp run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// The main agent finished its turn.
    Completed,
    /// The agent finished, but the independent verifier found defects.
    VerificationFailed { summary: String },
    /// The agent finished, but verification did not complete (not a pass, and
    /// not a defect verdict either).
    VerificationIncomplete { summary: String },
    /// The main agent ended with an error.
    AgentError { code: String, message: String },
    /// The daemon's chat stream closed before the turn finished.
    StreamClosed,
    /// Interrupted by the user (Ctrl-C); a cancel was sent to the daemon.
    Interrupted,
}

impl RunOutcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            RunOutcome::Completed => EXIT_OK,
            RunOutcome::AgentError { .. } => EXIT_AGENT_ERROR,
            RunOutcome::VerificationFailed { .. } => EXIT_VERIFICATION_FAILED,
            RunOutcome::StreamClosed => EXIT_STREAM_CLOSED,
            RunOutcome::VerificationIncomplete { .. } => EXIT_VERIFICATION_INCOMPLETE,
            RunOutcome::Interrupted => EXIT_INTERRUPTED,
        }
    }
}

/// Final verification state observed before the root `Done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerificationState {
    Passed,
    Failed,
    Incomplete,
}

/// Totals printed at the end (stderr).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunStats {
    pub tool_calls: u32,
    pub approvals_granted: u32,
    pub approvals_denied: u32,
    /// Input/output tokens of the main agent's last LLM call, as reported by
    /// the daemon's `UsageInfo` (sent once per finished turn; it is the last
    /// call's usage, not a running total). Input ≈ context size at the end.
    pub last_input_tokens: u32,
    pub last_output_tokens: u32,
}

/// Build the `UserInput` message that starts the turn.
pub fn user_input(session_id: &str, prompt: &str) -> ClientMessage {
    ClientMessage {
        payload: Some(client_message::Payload::UserInput(UserInput {
            text: prompt.to_string(),
            session_id: session_id.to_string(),
            request_id: "visp-run-1".to_string(),
        })),
    }
}

fn user_response(query_id: &str, selected_index: i32, text: &str) -> ClientMessage {
    ClientMessage {
        payload: Some(client_message::Payload::UserResponse(UserResponse {
            query_id: query_id.to_string(),
            selected_index,
            text: text.to_string(),
        })),
    }
}

fn cancel(session_id: &str) -> ClientMessage {
    ClientMessage {
        payload: Some(client_message::Payload::Cancel(Cancel {
            session_id: session_id.to_string(),
        })),
    }
}

/// One line, at most `max` chars, for progress output.
fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// Pump daemon events for one turn of `session_id` until it ends.
///
/// Pure with respect to I/O targets so it can be tested with channels and
/// in-memory writers. `interrupt` resolves when the user asks to stop.
#[allow(clippy::too_many_arguments)]
pub async fn drive<O: Write, E: Write>(
    inbound: &mut mpsc::Receiver<ServerMessage>,
    outbound: &mpsc::Sender<ClientMessage>,
    session_id: &str,
    approve: bool,
    out: &mut O,
    err: &mut E,
    interrupt: impl Future<Output = ()>,
    stats: &mut RunStats,
) -> RunOutcome {
    tokio::pin!(interrupt);
    let mut interrupted = false;
    let mut cancel_deadline: Option<tokio::time::Instant> = None;
    let mut text_open = false;
    let mut verification: Option<(VerificationState, String)> = None;

    loop {
        let msg = tokio::select! {
            m = inbound.recv() => m,
            _ = &mut interrupt, if !interrupted => {
                interrupted = true;
                let _ = writeln!(err, "[visp run] interrupted, cancelling...");
                let _ = outbound.send(cancel(session_id)).await;
                cancel_deadline = Some(tokio::time::Instant::now() + CANCEL_GRACE);
                continue;
            }
            _ = async {
                match cancel_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                return RunOutcome::Interrupted;
            }
        };
        let Some(msg) = msg else {
            return if interrupted {
                RunOutcome::Interrupted
            } else {
                RunOutcome::StreamClosed
            };
        };
        let Some(payload) = msg.payload else {
            continue;
        };
        match payload {
            server_message::Payload::TextDelta(td) => {
                // Sub-agent text is not part of the answer (same rule as visp-acp).
                if td.session_id == session_id {
                    let _ = write!(out, "{}", td.delta);
                    let _ = out.flush();
                    text_open = !td.delta.ends_with('\n');
                }
            }
            server_message::Payload::ToolCall(tc) => {
                stats.tool_calls += 1;
                let who = if tc.agent_name.is_empty() {
                    String::new()
                } else {
                    format!("{}: ", tc.agent_name)
                };
                let _ = writeln!(
                    err,
                    "[tool #{}] {who}{}({})",
                    stats.tool_calls,
                    tc.tool_name,
                    one_line(&tc.arguments, 120)
                );
            }
            server_message::Payload::ToolResult(tr) => {
                if tr.is_error {
                    let _ = writeln!(
                        err,
                        "[tool error] {}: {}",
                        tr.tool_name,
                        one_line(&tr.content, 200)
                    );
                }
            }
            server_message::Payload::UserQuery(q) => {
                // Approvals and questions are answered for every session
                // (sub-agents block too if nobody answers).
                let reply = if q.options.is_empty() {
                    if approve {
                        stats.approvals_granted += 1;
                        user_response(&q.query_id, APPROVE_ONCE, "")
                    } else {
                        stats.approvals_denied += 1;
                        let _ = writeln!(
                            err,
                            "[visp run] denied (use --yes to approve): {}",
                            one_line(&q.message, 160)
                        );
                        user_response(&q.query_id, DENY, "")
                    }
                } else {
                    let _ = writeln!(
                        err,
                        "[visp run] question answered automatically: {}",
                        one_line(&q.message, 160)
                    );
                    user_response(&q.query_id, -1, NO_USER_ANSWER)
                };
                if outbound.send(reply).await.is_err() {
                    return RunOutcome::StreamClosed;
                }
            }
            server_message::Payload::VerificationUpdate(v) => {
                for line in format_verification_lines(&v) {
                    let _ = writeln!(err, "{line}");
                }
                if v.is_final {
                    let state = match v.stage.as_str() {
                        "passed" => VerificationState::Passed,
                        "inconclusive" => VerificationState::Incomplete,
                        _ => VerificationState::Failed,
                    };
                    verification = Some((state, v.summary.clone()));
                }
            }
            server_message::Payload::StatusUpdate(su) => {
                if su.session_id == session_id && !su.message.is_empty() {
                    let _ = writeln!(err, "[status] {}", one_line(&su.message, 200));
                }
            }
            server_message::Payload::UsageInfo(u) if u.session_id == session_id => {
                // Not cumulative: the daemon reports the last call of the turn.
                stats.last_input_tokens = u.input_tokens;
                stats.last_output_tokens = u.output_tokens;
            }
            server_message::Payload::Done(d) => {
                if d.session_id == session_id {
                    if text_open {
                        let _ = writeln!(out);
                    }
                    if interrupted {
                        return RunOutcome::Interrupted;
                    }
                    return match verification {
                        Some((VerificationState::Failed, summary)) => {
                            RunOutcome::VerificationFailed { summary }
                        }
                        Some((VerificationState::Incomplete, summary)) => {
                            RunOutcome::VerificationIncomplete { summary }
                        }
                        Some((VerificationState::Passed, _)) | None => RunOutcome::Completed,
                    };
                }
            }
            server_message::Payload::Error(e) if e.session_id == session_id => {
                if text_open {
                    let _ = writeln!(out);
                }
                if interrupted {
                    return RunOutcome::Interrupted;
                }
                return RunOutcome::AgentError {
                    code: e.code,
                    message: e.message,
                };
            }
            _ => {}
        }
    }
}

/// Render a structured verification event for stderr.
pub fn format_verification_lines(v: &visp_proto::visp::VerificationUpdate) -> Vec<String> {
    let head = match v.stage.as_str() {
        "started" => format!(
            "[verify] 🔍 第 {} 轮独立验证开始（返工上限 {}）",
            v.round, v.max_rounds
        ),
        "passed" => format!("[verify] ✅ 验证通过：{}", v.summary),
        "failed" => {
            if v.is_final {
                format!("[verify] ❌ 验证未通过：{}", v.summary)
            } else {
                format!("[verify] ❌ 验证未通过（将继续返工）：{}", v.summary)
            }
        }
        "rework" => format!("[verify] ↩ {}", v.summary),
        "exhausted" => format!(
            "[verify] ❌ 验证未通过（已用尽 {} 轮返工）：{}",
            v.max_rounds, v.summary
        ),
        "inconclusive" => format!(
            "[verify] ⚠️ 验证未完成（验证没做成，不是发现缺陷）：{}",
            v.summary
        ),
        other => format!("[verify] {}: {}", other, v.summary),
    };
    let mut lines = vec![head];
    for ev in &v.evidence {
        lines.push(format!(
            "[verify]   证据：检查={}；方法={}；结果={}",
            ev.check, ev.method, ev.result
        ));
    }
    lines
}

/// Read the prompt: positional words joined by spaces, or stdin when the
/// words are empty or a single "-".
pub fn resolve_prompt(words: &[String], stdin: impl std::io::Read) -> Result<String, String> {
    let from_stdin = words.is_empty() || (words.len() == 1 && words[0] == "-");
    let text = if from_stdin {
        let mut buf = String::new();
        let mut stdin = stdin;
        stdin
            .read_to_string(&mut buf)
            .map_err(|e| format!("failed to read prompt from stdin: {e}"))?;
        buf
    } else {
        words.join(" ")
    };
    if text.trim().is_empty() {
        return Err("empty prompt: pass it as arguments or on stdin".to_string());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use visp_proto::visp::{Done, Error, TextDelta, ToolCall, UserQuery};

    fn msg(p: server_message::Payload) -> ServerMessage {
        ServerMessage { payload: Some(p) }
    }

    fn text(sid: &str, agent: &str, t: &str) -> ServerMessage {
        msg(server_message::Payload::TextDelta(TextDelta {
            delta: t.into(),
            session_id: sid.into(),
            agent_name: agent.into(),
        }))
    }

    fn done(sid: &str) -> ServerMessage {
        msg(server_message::Payload::Done(Done {
            session_id: sid.into(),
        }))
    }

    fn approval(sid: &str, qid: &str) -> ServerMessage {
        msg(server_message::Payload::UserQuery(UserQuery {
            query_id: qid.into(),
            message: "Allow tool: bash(ls)?".into(),
            session_id: sid.into(),
            options: vec![],
            allow_other: false,
        }))
    }

    /// Feed `events`, run `drive`, return (outcome, stdout, stderr, sent messages).
    async fn run(
        events: Vec<ServerMessage>,
        approve: bool,
    ) -> (RunOutcome, String, String, Vec<ClientMessage>, RunStats) {
        let (in_tx, mut in_rx) = mpsc::channel(64);
        let (out_tx, mut out_rx) = mpsc::channel(64);
        for e in events {
            in_tx.send(e).await.unwrap();
        }
        drop(in_tx);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut stats = RunStats::default();
        let outcome = drive(
            &mut in_rx,
            &out_tx,
            "parent",
            approve,
            &mut out,
            &mut err,
            std::future::pending::<()>(),
            &mut stats,
        )
        .await;
        drop(out_tx);
        let mut sent = Vec::new();
        while let Some(m) = out_rx.recv().await {
            sent.push(m);
        }
        (
            outcome,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
            sent,
            stats,
        )
    }

    fn selected(m: &ClientMessage) -> (i32, String) {
        match &m.payload {
            Some(client_message::Payload::UserResponse(r)) => (r.selected_index, r.text.clone()),
            other => panic!("expected UserResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prints_only_parent_text_and_completes_on_parent_done() {
        let (outcome, out, _err, _sent, _) = run(
            vec![
                text("parent", "", "Hello "),
                text("child-1", "explorer", "SUB-AGENT NOISE"),
                done("child-1"), // a sub-agent finishing must not end the run
                text("parent", "", "world"),
                done("parent"),
                text("parent", "", "after done"),
            ],
            false,
        )
        .await;
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(out, "Hello world\n");
    }

    #[tokio::test]
    async fn approvals_follow_the_yes_flag() {
        let events = || {
            vec![
                approval("parent", "q1"),
                approval("child-1", "q2"),
                done("parent"),
            ]
        };

        let (_, _, _, sent, stats) = run(events(), true).await;
        assert_eq!(sent.len(), 2, "sub-agent approvals must be answered too");
        assert_eq!(selected(&sent[0]).0, APPROVE_ONCE);
        assert_eq!(selected(&sent[1]).0, APPROVE_ONCE);
        assert_eq!(stats.approvals_granted, 2);

        let (_, _, err, sent, stats) = run(events(), false).await;
        assert_eq!(selected(&sent[0]).0, DENY);
        assert_eq!(stats.approvals_denied, 2);
        assert!(err.contains("use --yes"));
    }

    #[tokio::test]
    async fn option_questions_get_the_no_user_answer() {
        let q = msg(server_message::Payload::UserQuery(UserQuery {
            query_id: "q9".into(),
            message: "Which file?".into(),
            session_id: "parent".into(),
            options: vec!["a.py".into(), "b.py".into()],
            allow_other: true,
        }));
        let (_, _, _, sent, _) = run(vec![q, done("parent")], true).await;
        assert_eq!(selected(&sent[0]), (-1, NO_USER_ANSWER.to_string()));
    }

    #[tokio::test]
    async fn parent_error_ends_with_agent_error_child_error_does_not() {
        let err_msg = |sid: &str| {
            msg(server_message::Payload::Error(Error {
                code: "LlmApi".into(),
                message: format!("boom in {sid}"),
                session_id: sid.into(),
                agent_name: String::new(),
            }))
        };
        let (outcome, _, _, _, _) = run(vec![err_msg("child-1"), err_msg("parent")], false).await;
        assert_eq!(
            outcome,
            RunOutcome::AgentError {
                code: "LlmApi".into(),
                message: "boom in parent".into()
            }
        );
        assert_eq!(outcome.exit_code(), 1);
    }

    #[tokio::test]
    async fn closed_stream_before_done_is_reported() {
        let (outcome, _, _, _, _) = run(vec![text("parent", "", "partial")], false).await;
        assert_eq!(outcome, RunOutcome::StreamClosed);
        assert_eq!(outcome.exit_code(), 3);
    }

    #[tokio::test]
    async fn tool_calls_are_counted_and_shown_on_stderr() {
        let tc = msg(server_message::Payload::ToolCall(ToolCall {
            call_id: "c1".into(),
            tool_name: "bash".into(),
            arguments: "{\"command\": \"ls\n -la\"}".into(),
            session_id: "child-1".into(),
            agent_name: "explorer".into(),
        }));
        let (_, out, err, _, stats) = run(vec![tc, done("parent")], false).await;
        assert_eq!(stats.tool_calls, 1);
        assert!(err.contains("[tool #1] explorer: bash({\"command\": \"ls -la\"})"));
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn interrupt_sends_cancel_and_returns_interrupted() {
        let (in_tx, mut in_rx) = mpsc::channel(8);
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut stats = RunStats::default();
        // The daemon answers the cancel with Done for the parent session.
        let daemon = tokio::spawn(async move {
            let m: ClientMessage = out_rx.recv().await.unwrap();
            assert!(matches!(
                m.payload,
                Some(client_message::Payload::Cancel(_))
            ));
            in_tx.send(done("parent")).await.unwrap();
        });
        let outcome = drive(
            &mut in_rx,
            &out_tx,
            "parent",
            false,
            &mut out,
            &mut err,
            async {},
            &mut stats,
        )
        .await;
        daemon.await.unwrap();
        assert_eq!(outcome, RunOutcome::Interrupted);
        assert_eq!(outcome.exit_code(), 130);
    }

    #[tokio::test]
    async fn usage_reports_the_main_agents_last_call_not_a_sum() {
        let usage = |sid: &str, i: u32, o: u32| {
            msg(server_message::Payload::UsageInfo(
                visp_proto::visp::UsageInfo {
                    input_tokens: i,
                    output_tokens: o,
                    session_id: sid.into(),
                    ..Default::default()
                },
            ))
        };
        let (_, _, _, _, stats) = run(
            vec![
                usage("parent", 100, 10),
                usage("child-1", 50, 5),
                usage("parent", 300, 30),
                done("parent"),
            ],
            false,
        )
        .await;
        assert_eq!(
            (stats.last_input_tokens, stats.last_output_tokens),
            (300, 30)
        );
    }

    #[test]
    fn prompt_from_args_or_stdin() {
        let words = vec!["fix".to_string(), "the bug".to_string()];
        assert_eq!(
            resolve_prompt(&words, std::io::empty()).unwrap(),
            "fix the bug"
        );
        assert_eq!(
            resolve_prompt(&[], "from stdin\n".as_bytes()).unwrap(),
            "from stdin\n"
        );
        assert_eq!(
            resolve_prompt(&["-".to_string()], "piped".as_bytes()).unwrap(),
            "piped"
        );
        assert!(resolve_prompt(&[], "   ".as_bytes()).is_err());
    }

    fn verification(stage: &str, is_final: bool, passed: bool, summary: &str) -> ServerMessage {
        msg(server_message::Payload::VerificationUpdate(
            visp_proto::visp::VerificationUpdate {
                stage: stage.into(),
                round: 1,
                max_rounds: 2,
                summary: summary.into(),
                evidence: vec![visp_proto::visp::VerificationEvidence {
                    check: "10 万行 CSV 按列名查询".into(),
                    method: "构造大文件并实际运行".into(),
                    result: "失败：仍全量加载".into(),
                }],
                is_final,
                passed,
                session_id: "parent".into(),
                agent_name: "verifier".into(),
            },
        ))
    }

    #[tokio::test]
    async fn final_failed_verification_maps_to_exit_code_2() {
        let (outcome, _out, err, _, _) = run(
            vec![
                verification("started", false, false, "进行中"),
                text("parent", "", "初版完成"),
                verification("exhausted", true, false, "核心场景仍不满足"),
                done("parent"),
            ],
            true,
        )
        .await;
        assert_eq!(
            outcome,
            RunOutcome::VerificationFailed {
                summary: "核心场景仍不满足".into()
            }
        );
        assert_eq!(
            outcome.exit_code(),
            EXIT_VERIFICATION_FAILED,
            "验证发现缺陷必须单独退出码"
        );
        assert!(err.contains("验证未通过"));
        assert!(err.contains("10 万行 CSV 按列名查询"), "stderr 必须带证据");
    }

    #[tokio::test]
    async fn incomplete_verification_is_not_a_pass_and_has_its_own_exit_code() {
        let (outcome, _out, err, _, _) = run(
            vec![
                verification(
                    "inconclusive",
                    true,
                    false,
                    "验证者未提交结构化结论（步数用尽）",
                ),
                done("parent"),
            ],
            true,
        )
        .await;
        assert_eq!(
            outcome,
            RunOutcome::VerificationIncomplete {
                summary: "验证者未提交结构化结论（步数用尽）".into()
            }
        );
        assert_eq!(
            outcome.exit_code(),
            4,
            "验证没做成既不是成功，也不应与发现缺陷共用退出码"
        );
        assert!(err.contains("验证未完成"));
        assert!(err.contains("不是发现缺陷"));
    }

    #[tokio::test]
    async fn passed_verification_keeps_exit_code_0() {
        let (outcome, out, _, _, _) = run(
            vec![
                text("parent", "", "答案"),
                verification("passed", true, true, "核心场景通过"),
                done("parent"),
            ],
            true,
        )
        .await;
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(outcome.exit_code(), 0);
        assert_eq!(out, "答案\n");
    }

    /// Each exit code must have exactly one meaning. In particular 2 is what
    /// clap returns for a bad command line (outside `run_once`), so no
    /// `RunOutcome` may reuse it.
    #[test]
    fn exit_codes_are_unique_and_keep_2_for_cli_usage_errors() {
        let cases = [
            ("completed", RunOutcome::Completed.exit_code(), EXIT_OK),
            (
                "agent error",
                RunOutcome::AgentError {
                    code: "LlmApi".into(),
                    message: "boom".into(),
                }
                .exit_code(),
                EXIT_AGENT_ERROR,
            ),
            (
                "verification failed",
                RunOutcome::VerificationFailed {
                    summary: "缺陷".into(),
                }
                .exit_code(),
                EXIT_VERIFICATION_FAILED,
            ),
            (
                "stream closed",
                RunOutcome::StreamClosed.exit_code(),
                EXIT_STREAM_CLOSED,
            ),
            (
                "verification incomplete",
                RunOutcome::VerificationIncomplete {
                    summary: "没做成".into(),
                }
                .exit_code(),
                EXIT_VERIFICATION_INCOMPLETE,
            ),
            (
                "interrupted",
                RunOutcome::Interrupted.exit_code(),
                EXIT_INTERRUPTED,
            ),
        ];
        for (name, got, want) in cases {
            assert_eq!(got, want, "{name} 的退出码被改变了");
        }

        let mut codes: Vec<i32> = cases.iter().map(|(_, got, _)| *got).collect();
        codes.push(EXIT_USAGE);
        let total = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(
            codes.len(),
            total,
            "每个退出码只能有一种含义，实际: {codes:?}"
        );
        assert_eq!(
            EXIT_VERIFICATION_FAILED, 5,
            "2 留给 clap 的用法错误，验证失败必须用别的值"
        );
    }

    #[tokio::test]
    async fn sub_agent_approval_is_granted_with_yes_for_verifier_commands() {
        // 验证者作为子会话运行；--yes 必须同时放行它的命令审批。
        let (_, _, _, sent, stats) = run(
            vec![approval("verifier-session", "q1"), done("parent")],
            true,
        )
        .await;
        assert_eq!(selected(&sent[0]).0, APPROVE_ONCE);
        assert_eq!(stats.approvals_granted, 1);
    }
}
