use super::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
    SelectedPermissionOutcome, SessionUpdate, TextContent, ToolCall, ToolCallContent, ToolCallId,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use futures::future::BoxFuture;
use tokio::sync::Semaphore;

fn text_update(text: &str) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        text,
    ))))
}

fn tool_call(id: &str, title: &str) -> SessionUpdate {
    SessionUpdate::ToolCall(ToolCall::new(ToolCallId::new(id), title).kind(ToolKind::Execute))
}

fn tool_failed(id: &str, output: &str) -> SessionUpdate {
    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::new(id),
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .content(vec![ToolCallContent::from(ContentBlock::Text(
                TextContent::new(output),
            ))]),
    ))
}

fn permission_request(id: &str) -> RequestPermissionRequest {
    RequestPermissionRequest::new(
        "s1",
        ToolCallUpdate::new(
            ToolCallId::new(id),
            ToolCallUpdateFields::new().title("Allow tool: bash(ls)?"),
        ),
        vec![
            PermissionOption::new("allow_once", "允许", PermissionOptionKind::AllowOnce),
            PermissionOption::new(
                "allow_always",
                "始终允许",
                PermissionOptionKind::AllowAlways,
            ),
            PermissionOption::new("reject_once", "拒绝", PermissionOptionKind::RejectOnce),
            PermissionOption::new(
                "reject_always",
                "永久拒绝",
                PermissionOptionKind::RejectAlways,
            ),
        ],
    )
}

fn selected(resp: &RequestPermissionResponse) -> String {
    match &resp.outcome {
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => {
            option_id.0.to_string()
        }
        other => panic!("expected selected outcome, got {other:?}"),
    }
}

// ===== ACP 事件 → stdout / stderr =====

#[test]
fn agent_text_goes_to_stdout_only() {
    let mut core = RunCore::new(false);
    let actions = on_update(&mut core, &text_update("Hello "));
    assert_eq!(actions, vec![Action::Stdout("Hello ".into())]);
    assert_eq!(core.stats, RunStats::default());
}

#[test]
fn text_open_tracks_the_last_chunk() {
    let mut core = RunCore::new(false);
    on_update(&mut core, &text_update("Hello "));
    assert!(core.text_open);
    on_update(&mut core, &text_update("world\n"));
    assert!(!core.text_open);
}

#[test]
fn tool_calls_are_counted_and_reported_on_stderr() {
    let mut core = RunCore::new(false);
    let actions = on_update(&mut core, &tool_call("c1", "bash(ls -la)"));
    assert_eq!(core.stats.tool_calls, 1);
    assert!(
        matches!(&actions[..], [Action::Stderr(line)] if line.contains("[tool #1] bash(ls -la)")),
        "unexpected actions: {actions:?}"
    );
}

#[test]
fn failed_tool_updates_report_tool_error_on_stderr() {
    let mut core = RunCore::new(false);
    on_update(&mut core, &tool_call("c1", "bash(ls)"));
    let actions = on_update(&mut core, &tool_failed("c1", "boom\nsecond line"));
    assert!(
        matches!(&actions[..], [Action::Stderr(line)]
            if line.contains("[tool error] bash(ls): boom second line")),
        "unexpected actions: {actions:?}"
    );
}

#[test]
fn completed_tool_updates_are_silent() {
    let mut core = RunCore::new(false);
    on_update(&mut core, &tool_call("c1", "bash(ls)"));
    let completed = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::new("c1"),
        ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
    ));
    assert!(on_update(&mut core, &completed).is_empty());
}

// ===== 审批 =====

#[test]
fn yes_approves_every_request_once_and_counts_them() {
    let mut core = RunCore::new(true);
    for id in ["q1", "q2"] {
        let actions = on_permission(&mut core, &permission_request(id));
        assert!(actions.0.is_empty(), "approval must be quiet");
        assert_eq!(selected(&actions.1), "allow_once");
    }
    assert_eq!(core.stats.approvals_granted, 2);
    assert_eq!(core.stats.approvals_denied, 0);
}

#[test]
fn yes_never_selects_allow_always() {
    let mut core = RunCore::new(true);
    let req = RequestPermissionRequest::new(
        "s1",
        ToolCallUpdate::new(ToolCallId::new("q1"), ToolCallUpdateFields::new()),
        vec![PermissionOption::new(
            "allow_always",
            "始终允许",
            PermissionOptionKind::AllowAlways,
        )],
    );
    let (_, resp) = on_permission(&mut core, &req);
    assert_eq!(resp.outcome, RequestPermissionOutcome::Cancelled);
    assert_eq!(core.stats.approvals_granted, 0);
}

#[test]
fn without_yes_every_request_is_rejected_with_a_hint() {
    let mut core = RunCore::new(false);
    let (actions, resp) = on_permission(&mut core, &permission_request("q1"));
    assert_eq!(selected(&resp), "reject_once");
    assert!(
        matches!(&actions[..], [Action::Stderr(line)]
            if line.contains("denied") && line.contains("--yes")),
        "unexpected actions: {actions:?}"
    );
    assert_eq!(core.stats.approvals_denied, 1);

    on_permission(&mut core, &permission_request("q2"));
    assert_eq!(core.stats.approvals_denied, 2);
}

#[test]
fn without_yes_an_allow_only_request_is_cancelled_and_reported() {
    let mut core = RunCore::new(false);
    let req = RequestPermissionRequest::new(
        "s1",
        ToolCallUpdate::new(
            ToolCallId::new("q1"),
            ToolCallUpdateFields::new().title("Allow tool: bash(ls)?"),
        ),
        vec![PermissionOption::new(
            "allow_always",
            "始终允许",
            PermissionOptionKind::AllowAlways,
        )],
    );
    let (actions, resp) = on_permission(&mut core, &req);
    assert_eq!(resp.outcome, RequestPermissionOutcome::Cancelled);
    assert_eq!(core.stats.approvals_denied, 1);
    assert!(
        matches!(&actions[..], [Action::Stderr(line)] if line.contains("--yes")),
        "unexpected actions: {actions:?}"
    );
}

#[test]
fn permission_without_matching_option_is_cancelled() {
    let mut core = RunCore::new(true);
    let req = RequestPermissionRequest::new(
        "s1",
        ToolCallUpdate::new(ToolCallId::new("q1"), ToolCallUpdateFields::new()),
        vec![PermissionOption::new(
            "weird",
            "?",
            PermissionOptionKind::RejectAlways,
        )],
    );
    let (_, resp) = on_permission(&mut core, &req);
    assert_eq!(resp.outcome, RequestPermissionOutcome::Cancelled);
    assert_eq!(core.stats.approvals_granted, 0);
}

// ===== prompt 解析 =====

#[test]
fn prompt_comes_from_words_or_stdin() {
    assert_eq!(
        resolve_prompt(&["fix".into(), "the bug".into()], std::io::empty()).unwrap(),
        "fix the bug"
    );
    assert_eq!(
        resolve_prompt(&[], "from stdin\n".as_bytes()).unwrap(),
        "from stdin\n"
    );
    assert_eq!(
        resolve_prompt(&["-".into()], "piped".as_bytes()).unwrap(),
        "piped"
    );
    assert!(resolve_prompt(&[], "   \n".as_bytes()).is_err());
}

// ===== stop reason / exit codes =====

#[test]
fn stop_reasons_map_to_unique_exit_codes() {
    assert_eq!(
        outcome_from_stop_reason(StopReason::EndTurn, false).exit_code(),
        EXIT_OK
    );
    assert_eq!(
        outcome_from_stop_reason(StopReason::Refusal, false).exit_code(),
        EXIT_ERROR
    );
    assert_eq!(
        outcome_from_stop_reason(StopReason::Cancelled, true).exit_code(),
        EXIT_INTERRUPTED
    );
    assert_eq!(
        outcome_from_stop_reason(StopReason::Cancelled, false).exit_code(),
        EXIT_ERROR
    );
    let codes = [
        EXIT_OK,
        EXIT_ERROR,
        EXIT_USAGE,
        EXIT_CONNECTION_LOST,
        EXIT_INTERRUPTED,
    ];
    let mut unique = codes.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), codes.len(), "exit codes must be unique");
}

// ===== agent 启动参数 =====

fn run_args() -> RunArgs {
    RunArgs {
        yes: false,
        project: PathBuf::from("."),
        model: None,
        temperature: None,
        thinking_budget: None,
        config_dir: None,
        prompt: Vec::new(),
    }
}

#[test]
fn agent_launch_args_carry_project_and_config_dir() {
    let args = RunArgs {
        config_dir: Some(PathBuf::from("/cfg")),
        model: Some("provider/model".into()),
        temperature: Some(0.2),
        thinking_budget: Some(1024),
        ..run_args()
    };
    let config = agent_config(&args, std::path::Path::new("/work/project"));
    let argv = config.arguments();
    assert!(argv.windows(2).any(|w| w == ["--project", "/work/project"]));
    assert!(argv.windows(2).any(|w| w == ["--config-dir", "/cfg"]));
    assert!(argv.windows(2).any(|w| w == ["--model", "provider/model"]));
    assert!(argv.windows(2).any(|w| w == ["--temperature", "0.2"]));
    assert!(argv.windows(2).any(|w| w == ["--thinking-budget", "1024"]));
}

// ===== Ctrl-C：先发 cancel 再退出 =====

struct FakeIo {
    cancelled: Arc<AtomicBool>,
    release: Arc<Semaphore>,
    release_on_cancel: bool,
}

impl FakeIo {
    fn new(release_on_cancel: bool) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            release: Arc::new(Semaphore::new(0)),
            release_on_cancel,
        }
    }
}

impl PromptIo for FakeIo {
    fn send_prompt(
        &mut self,
        _prompt: &str,
    ) -> BoxFuture<'static, Result<StopReason, PromptError>> {
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            release.acquire().await.unwrap().forget();
            Ok(StopReason::Cancelled)
        })
    }

    fn send_cancel(&self) -> Result<(), String> {
        self.cancelled.store(true, Ordering::SeqCst);
        if self.release_on_cancel {
            self.release.add_permits(1);
        }
        Ok(())
    }
}

#[tokio::test]
async fn interrupt_sends_cancel_before_exiting() {
    let mut io = FakeIo::new(true);
    let cancelled = Arc::clone(&io.cancelled);
    let mut announced = false;
    let drive = drive_prompt(
        &mut io,
        "do it",
        async {},
        Duration::from_secs(1),
        &mut || announced = true,
    )
    .await;
    assert_eq!(drive, PromptDrive::Interrupted);
    assert!(cancelled.load(Ordering::SeqCst), "cancel must be sent");
    assert!(announced, "the interrupt must be announced on stderr");
}

#[tokio::test]
async fn cancel_grace_timeout_also_counts_as_interrupted() {
    let mut io = FakeIo::new(false);
    let cancelled = Arc::clone(&io.cancelled);
    let drive = drive_prompt(
        &mut io,
        "do it",
        async {},
        Duration::from_millis(20),
        &mut || {},
    )
    .await;
    assert_eq!(drive, PromptDrive::Timeout);
    assert!(cancelled.load(Ordering::SeqCst));
    assert_eq!(
        PromptDrive::Timeout.into_outcome().exit_code(),
        EXIT_INTERRUPTED
    );
}

#[tokio::test]
async fn prompt_response_without_interrupt_is_used() {
    struct OnceIo;
    impl PromptIo for OnceIo {
        fn send_prompt(
            &mut self,
            _prompt: &str,
        ) -> BoxFuture<'static, Result<StopReason, PromptError>> {
            Box::pin(async { Ok(StopReason::EndTurn) })
        }
        fn send_cancel(&self) -> Result<(), String> {
            panic!("cancel must not be sent without an interrupt")
        }
    }
    let mut io = OnceIo;
    let drive = drive_prompt(
        &mut io,
        "do it",
        std::future::pending::<()>(),
        Duration::from_secs(1),
        &mut || {},
    )
    .await;
    assert_eq!(drive, PromptDrive::Response(StopReason::EndTurn));
    assert_eq!(drive.into_outcome().exit_code(), EXIT_OK);
}
