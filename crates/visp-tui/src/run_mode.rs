//! `visp-tui run`: one prompt, non-interactive, then exit.
//!
//! This mode is an **ACP client**: it spawns `visp-acp` as a child process and
//! talks to it over ACP (JSON-RPC on stdio). `visp-acp` in turn starts or
//! connects to the daemon. The TUI code itself never talks gRPC in this mode.
//!
//! - stdout: the main agent's answer text only (pipe-friendly).
// - stderr: progress (tool calls, tool errors, denied approvals, summary).
//! - `--yes` approves every permission request once; without it every request
//!   is rejected and the agent is told so (it can continue without the tool).
//!
//! Exit codes: see [`EXIT_OK`] … [`EXIT_INTERRUPTED`].

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, ContentChunk, InitializeRequest, NewSessionRequest,
    PermissionOption, PermissionOptionKind, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate,
};
use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo, is_incoming_transport_closed,
    on_receive_notification, on_receive_request,
};
use clap::Args;
use futures::future::BoxFuture;

pub const EXIT_OK: i32 = 0;
/// A runtime error: agent error, spawn/initialize/session failure, bad project.
pub const EXIT_ERROR: i32 = 1;
/// Command-line usage error (clap's own parse errors also use 2).
pub const EXIT_USAGE: i32 = 2;
/// The ACP connection to the agent was lost unexpectedly.
pub const EXIT_CONNECTION_LOST: i32 = 3;
/// Interrupted by Ctrl-C (conventional 128 + SIGINT).
pub const EXIT_INTERRUPTED: i32 = 130;

/// How long to wait for the agent to acknowledge `session/cancel`.
const CANCEL_GRACE: Duration = Duration::from_secs(10);

/// Arguments for `visp-tui run`.
#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// Approve every tool request once (without it, all are denied).
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Project path.
    #[arg(short = 'p', long, default_value = ".")]
    pub project: PathBuf,
    /// LLM model override (passed to the ACP agent as a launch argument; ACP
    /// itself has no model-selection request).
    #[arg(long)]
    pub model: Option<String>,
    /// LLM temperature override.
    #[arg(long)]
    pub temperature: Option<f64>,
    /// Thinking budget override.
    #[arg(long)]
    pub thinking_budget: Option<u32>,
    /// Global config directory.
    #[arg(long)]
    pub config_dir: Option<PathBuf>,
    /// Prompt words; empty or a single "-" reads stdin.
    #[arg(value_name = "PROMPT")]
    pub prompt: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunStats {
    pub tool_calls: u32,
    pub approvals_granted: u32,
    pub approvals_denied: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    AgentError { message: String },
    Interrupted,
    ConnectionLost { message: String },
}

impl RunOutcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            RunOutcome::Completed => EXIT_OK,
            RunOutcome::AgentError { .. } => EXIT_ERROR,
            RunOutcome::ConnectionLost { .. } => EXIT_CONNECTION_LOST,
            RunOutcome::Interrupted => EXIT_INTERRUPTED,
        }
    }
}

/// An output side effect produced by the pure core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Answer text (no trailing newline is added).
    Stdout(String),
    /// One progress line (a newline is appended).
    Stderr(String),
}

/// State shared by the ACP notification and permission handlers.
pub struct RunCore {
    pub yes: bool,
    pub stats: RunStats,
    /// True when the last answer chunk did not end with a newline.
    pub text_open: bool,
    /// Tool-call id → title, so failure updates can name the tool.
    tool_titles: HashMap<String, String>,
}

impl RunCore {
    pub fn new(yes: bool) -> Self {
        Self {
            yes,
            stats: RunStats::default(),
            text_open: false,
            tool_titles: HashMap::new(),
        }
    }
}

/// Flatten whitespace and truncate to `max` characters.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn content_text(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::Text(t) => Some(&t.text),
        _ => None,
    }
}

fn chunk_text(chunk: &ContentChunk) -> Option<&str> {
    content_text(&chunk.content)
}

fn tool_content_text(content: &ToolCallContent) -> Option<&str> {
    match content {
        ToolCallContent::Content(c) => content_text(&c.content),
        _ => None,
    }
}

fn update_tool_output(update: &ToolCallUpdate) -> String {
    update
        .fields
        .content
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(tool_content_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pure ACP event → output mapping. Called for every `session/update`.
pub fn on_update(core: &mut RunCore, update: &SessionUpdate) -> Vec<Action> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match chunk_text(chunk) {
            Some(text) if !text.is_empty() => {
                core.text_open = !text.ends_with('\n');
                vec![Action::Stdout(text.to_string())]
            }
            _ => Vec::new(),
        },
        SessionUpdate::ToolCall(call) => {
            core.stats.tool_calls += 1;
            core.tool_titles
                .insert(call.tool_call_id.0.to_string(), call.title.clone());
            vec![Action::Stderr(format!(
                "[tool #{}] {}",
                core.stats.tool_calls,
                one_line(&tool_title(call), 200)
            ))]
        }
        SessionUpdate::ToolCallUpdate(update) => {
            if update.fields.status == Some(ToolCallStatus::Failed) {
                let id = update.tool_call_id.0.to_string();
                let title = update
                    .fields
                    .title
                    .clone()
                    .or_else(|| core.tool_titles.get(&id).cloned())
                    .unwrap_or_else(|| id.clone());
                vec![Action::Stderr(format!(
                    "[tool error] {title}: {}",
                    one_line(&update_tool_output(update), 200)
                ))]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

fn tool_title(call: &ToolCall) -> String {
    if call.title.is_empty() {
        call.name.clone().unwrap_or_else(|| "tool".to_string())
    } else {
        call.title.clone()
    }
}

fn find_option(
    options: &[PermissionOption],
    kind: PermissionOptionKind,
) -> Option<&PermissionOption> {
    options.iter().find(|o| o.kind == kind)
}

fn selected(option: &PermissionOption) -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
        SelectedPermissionOutcome::new(option.option_id.clone()),
    ))
}

/// Pure approval policy: `--yes` allows every request once, otherwise every
/// request is rejected and a hint is printed. Requests with no matching option
/// are cancelled (the agent must not block).
pub fn on_permission(
    core: &mut RunCore,
    req: &RequestPermissionRequest,
) -> (Vec<Action>, RequestPermissionResponse) {
    if core.yes {
        if let Some(option) = find_option(&req.options, PermissionOptionKind::AllowOnce) {
            core.stats.approvals_granted += 1;
            return (Vec::new(), selected(option));
        }
        return (
            Vec::new(),
            RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
        );
    }

    // Without `--yes` every request is denied. Prefer the one-shot reject
    // option; if the agent only offers "always" variants, still refuse by
    // cancelling instead of escalating to a permanent choice.
    core.stats.approvals_denied += 1;
    let what = req
        .tool_call
        .fields
        .title
        .clone()
        .unwrap_or_else(|| req.tool_call.tool_call_id.0.to_string());
    let hint = Action::Stderr(format!(
        "denied (use --yes to approve): {}",
        one_line(&what, 160)
    ));
    let response = match find_option(&req.options, PermissionOptionKind::RejectOnce) {
        Some(option) => selected(option),
        None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
    };
    (vec![hint], response)
}

/// Read the prompt: positional words joined by spaces, or stdin when the
/// words are empty or a single "-".
pub fn resolve_prompt(words: &[String], stdin: impl Read) -> Result<String, String> {
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

/// Map an ACP stop reason to the process outcome. `interrupted` tells whether
/// the client itself sent `session/cancel` (then `Cancelled` means Ctrl-C).
pub fn outcome_from_stop_reason(reason: StopReason, interrupted: bool) -> RunOutcome {
    match reason {
        StopReason::EndTurn => RunOutcome::Completed,
        StopReason::Cancelled => {
            if interrupted {
                RunOutcome::Interrupted
            } else {
                RunOutcome::AgentError {
                    message: "agent cancelled the turn".to_string(),
                }
            }
        }
        StopReason::Refusal => RunOutcome::AgentError {
            message: "agent refused the prompt".to_string(),
        },
        StopReason::MaxTokens => RunOutcome::AgentError {
            message: "agent stopped: maximum token limit reached".to_string(),
        },
        StopReason::MaxTurnRequests => RunOutcome::AgentError {
            message: "agent stopped: maximum turn requests reached".to_string(),
        },
        other => RunOutcome::AgentError {
            message: format!("agent stopped for an unknown reason: {other:?}"),
        },
    }
}

/// Why sending a prompt failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptError {
    pub message: String,
    pub connection_lost: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptDrive {
    Response(StopReason),
    Interrupted,
    Timeout,
    Failed(PromptError),
}

/// Transport-independent prompt side of one ACP session.
pub trait PromptIo {
    fn send_prompt(&mut self, prompt: &str) -> BoxFuture<'static, Result<StopReason, PromptError>>;
    fn send_cancel(&self) -> Result<(), String>;
}

/// Send one prompt, racing it with an interrupt (Ctrl-C).
///
/// On interrupt the cancel notification is sent *before* waiting for the
/// response (up to `grace`); a response then still counts as interrupted.
pub async fn drive_prompt<I: PromptIo>(
    io: &mut I,
    prompt: &str,
    interrupt: impl std::future::Future<Output = ()>,
    grace: Duration,
    on_interrupt: &mut dyn FnMut(),
) -> PromptDrive {
    let prompt_fut = io.send_prompt(prompt);
    tokio::pin!(prompt_fut);
    tokio::pin!(interrupt);

    tokio::select! {
        result = &mut prompt_fut => match result {
            Ok(reason) => PromptDrive::Response(reason),
            Err(err) => PromptDrive::Failed(err),
        },
        _ = &mut interrupt => {
            on_interrupt();
            if let Err(err) = io.send_cancel() {
                return PromptDrive::Failed(PromptError {
                    message: format!("failed to send session/cancel: {err}"),
                    connection_lost: false,
                });
            }
            match tokio::time::timeout(grace, &mut prompt_fut).await {
                Ok(_) => PromptDrive::Interrupted,
                Err(_) => PromptDrive::Timeout,
            }
        }
    }
}

impl PromptDrive {
    fn into_outcome(self) -> RunOutcome {
        match self {
            PromptDrive::Response(reason) => outcome_from_stop_reason(reason, false),
            PromptDrive::Interrupted | PromptDrive::Timeout => RunOutcome::Interrupted,
            PromptDrive::Failed(err) => {
                if err.connection_lost {
                    RunOutcome::ConnectionLost {
                        message: err.message,
                    }
                } else {
                    RunOutcome::AgentError {
                        message: err.message,
                    }
                }
            }
        }
    }
}

/// ACP-backed [`PromptIo`]: owns a connection handle to the agent.
struct AcpPromptIo {
    cx: ConnectionTo<Agent>,
    session_id: SessionId,
}

impl PromptIo for AcpPromptIo {
    fn send_prompt(&mut self, prompt: &str) -> BoxFuture<'static, Result<StopReason, PromptError>> {
        let cx = self.cx.clone();
        let session_id = self.session_id.clone();
        let request = PromptRequest::new(
            session_id,
            vec![ContentBlock::Text(TextContent::new(prompt.to_string()))],
        );
        Box::pin(async move {
            cx.send_request(request)
                .block_task()
                .await
                .map(|response| response.stop_reason)
                .map_err(|err| PromptError {
                    connection_lost: is_incoming_transport_closed(&err),
                    message: err.to_string(),
                })
        })
    }

    fn send_cancel(&self) -> Result<(), String> {
        self.cx
            .send_notification(CancelNotification::new(self.session_id.clone()))
            .map_err(|e| e.to_string())
    }
}

/// Resolve the ACP agent binary: test hook first, then a sibling binary, then
/// `PATH`.
fn resolve_agent_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("VISP_ACP_BIN") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let sibling = parent.join("visp-acp");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("visp-acp")
}

/// Build the ACP agent launch configuration. `--model`/`--temperature`/
/// `--thinking-budget` are visp-specific launch arguments: ACP v1 has no
/// model-selection message, so the client passes them to the agent process.
fn agent_config(args: &RunArgs, project: &std::path::Path) -> AcpAgentConfig {
    let mut config = AcpAgentConfig::new(resolve_agent_bin())
        .arg("--project")
        .arg(project.to_string_lossy().to_string());
    if let Some(dir) = &args.config_dir {
        config = config
            .arg("--config-dir")
            .arg(dir.to_string_lossy().to_string());
    }
    if let Some(model) = &args.model {
        config = config.arg("--model").arg(model.clone());
    }
    if let Some(temperature) = args.temperature {
        config = config.arg("--temperature").arg(temperature.to_string());
    }
    if let Some(budget) = args.thinking_budget {
        config = config.arg("--thinking-budget").arg(budget.to_string());
    }
    config
}

fn apply_actions(actions: &[Action]) {
    let mut out = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    for action in actions {
        match action {
            Action::Stdout(text) => {
                let _ = write!(out, "{text}");
            }
            Action::Stderr(line) => {
                let _ = writeln!(err, "[visp-tui run] {line}");
            }
        }
    }
    let _ = out.flush();
    let _ = err.flush();
}

fn finish_stdout(core: &RunCore) {
    if core.text_open {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out);
        let _ = out.flush();
    }
}

/// Run one prompt against `visp-acp` and return the process exit code.
pub async fn run(args: RunArgs) -> i32 {
    let prompt = match resolve_prompt(&args.prompt, std::io::stdin()) {
        Ok(prompt) => prompt,
        Err(err) => {
            eprintln!("[visp-tui run] {err}");
            return EXIT_USAGE;
        }
    };

    let project = match args.project.canonicalize() {
        Ok(path) => path,
        Err(err) => {
            eprintln!(
                "[visp-tui run] invalid project path {}: {err}",
                args.project.display()
            );
            return EXIT_ERROR;
        }
    };

    let started = std::time::Instant::now();
    let core = Arc::new(Mutex::new(RunCore::new(args.yes)));
    let transport = AcpAgent::new(agent_config(&args, &project));

    let notify_core = Arc::clone(&core);
    let request_core = Arc::clone(&core);
    let session_project = project.clone();

    let result = Client
        .builder()
        .name("visp-tui")
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                let actions = {
                    let mut core = notify_core
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    on_update(&mut core, &notification.update)
                };
                apply_actions(&actions);
                Ok(())
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let (actions, response) = {
                    let mut core = request_core
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    on_permission(&mut core, &request)
                };
                apply_actions(&actions);
                responder.respond(response)
            },
            on_receive_request!(),
        )
        .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(session_project))
                .block_task()
                .await?;
            let mut io = AcpPromptIo {
                cx: cx.clone(),
                session_id: session.session_id,
            };
            let drive = drive_prompt(
                &mut io,
                &prompt,
                async {
                    let _ = tokio::signal::ctrl_c().await;
                },
                CANCEL_GRACE,
                &mut || eprintln!("[visp-tui run] interrupted, cancelling..."),
            )
            .await;
            Ok(drive.into_outcome())
        })
        .await;

    finish_stdout(&core.lock().unwrap_or_else(|p| p.into_inner()));
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(err) => {
            if is_incoming_transport_closed(&err) {
                RunOutcome::ConnectionLost {
                    message: err.to_string(),
                }
            } else {
                RunOutcome::AgentError {
                    message: format!("ACP failure: {err}"),
                }
            }
        }
    };

    if let RunOutcome::AgentError { message } = &outcome {
        eprintln!("[visp-tui run] agent error: {message}");
    }
    if let RunOutcome::ConnectionLost { message } = &outcome {
        eprintln!("[visp-tui run] connection to the agent was lost: {message}");
    }
    if let RunOutcome::Interrupted = &outcome {
        eprintln!("[visp-tui run] interrupted");
    }

    let stats = core.lock().unwrap_or_else(|p| p.into_inner()).stats.clone();
    eprintln!(
        "[visp-tui run] {:?} in {:.0}s: {} tool calls, approvals {} granted / {} denied",
        outcome,
        started.elapsed().as_secs_f64(),
        stats.tool_calls,
        stats.approvals_granted,
        stats.approvals_denied
    );

    outcome.exit_code()
}

#[cfg(test)]
#[path = "run_mode_tests.rs"]
mod tests;
