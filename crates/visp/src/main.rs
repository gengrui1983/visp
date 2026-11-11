use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use tokio::process::{Child, Command};
use tonic::transport::Endpoint;
use visp_proto::visp::ShutdownRequest;
use visp_proto::visp::coder_daemon_client::CoderDaemonClient;

mod hook_cmd;
mod run_cmd;

/// visp launcher — starts daemon + CLI in one command
#[derive(Parser)]
#[command(name = "visp")]
struct Cli {
    #[arg(short = 'p', long, default_value = ".", global = true)]
    project: String,

    #[arg(short = 'a', long, default_value = "[::1]:50051")]
    addr: String,

    #[arg(long, global = true)]
    model: Option<String>,

    #[arg(long, global = true)]
    temperature: Option<f64>,

    #[arg(long, global = true)]
    thinking_budget: Option<u32>,

    #[arg(short = 's', long)]
    session: Option<String>,

    /// Custom global config directory (overrides ~/.config/visp).
    /// Passed to the daemon as --config-dir.
    #[arg(long, global = true)]
    config_dir: Option<String>,

    /// Do not spawn any hook process; only match and print.
    #[arg(long, global = true)]
    dry_run: bool,

    #[command(subcommand)]
    command: Option<LauncherCommand>,
}

/// Launcher subcommands (default: start daemon + TUI).
#[derive(Subcommand, Debug)]
enum LauncherCommand {
    /// Inspect lifecycle hooks: list / doctor / test / logs.
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// Run one prompt non-interactively and exit (for scripts / CI).
    ///
    /// The answer is printed on stdout, progress on stderr. The prompt is
    /// taken from the arguments, or from stdin when none (or "-") is given.
    /// Exit code: 0 done, 1 agent error, 3 daemon stream lost, 130 interrupted.
    Run {
        /// Approve every tool request once (without it, all are denied).
        #[arg(short = 'y', long)]
        yes: bool,
        /// The prompt (or "-" / nothing to read it from stdin).
        prompt: Vec<String>,
    },
}

/// `visp hooks` actions (design §15 / D10 / D11).
#[derive(Subcommand, Debug)]
enum HooksAction {
    /// List hook rules with source (global/project/builtin) and trust status.
    List,
    /// Static checks plus read-only runtime counters (GetHookStats).
    Doctor,
    /// Dry-run match rules against a sample payload for an event name or rule id.
    Test {
        /// Event name (e.g. `Stop`) or rule id.
        target: String,
    },
    /// Show where hook runtime logs are written.
    Logs,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // If --config-dir is given, export it as VISP_CONFIG_DIR so that the
    // launcher's own path resolution (log_dir etc.) uses it too.
    if let Some(ref dir) = cli.config_dir {
        // SAFETY: single-threaded setup before any threads are spawned.
        unsafe { std::env::set_var("VISP_CONFIG_DIR", dir) };
    }

    // `visp run ...`: one prompt, no TUI.
    if let Some(LauncherCommand::Run { yes, prompt }) = &cli.command {
        let code = run_once(&cli, *yes, prompt).await;
        std::process::exit(code);
    }

    // `visp hooks ...`: inspect hooks without launching the daemon/TUI.
    if let Some(ref command) = cli.command {
        let code = run_subcommand(command, &cli).await;
        std::process::exit(code);
    }

    let Some((mut daemon, addr)) = start_daemon(&cli).await else {
        std::process::exit(1);
    };
    let cli_bin = resolve_bin("visp-tui");

    // 6. Build CLI args
    let mut cli_args = vec![
        "visp-tui".to_string(),
        "--addr".to_string(),
        addr.clone(),
        "-p".to_string(),
        cli.project,
    ];
    if let Some(model) = &cli.model {
        cli_args.push("--model".to_string());
        cli_args.push(model.clone());
    }
    if let Some(temp) = cli.temperature {
        cli_args.push("--temperature".to_string());
        cli_args.push(temp.to_string());
    }
    if let Some(budget) = cli.thinking_budget {
        cli_args.push("--thinking-budget".to_string());
        cli_args.push(budget.to_string());
    }
    if let Some(session) = &cli.session {
        cli_args.push("--session".to_string());
        cli_args.push(session.clone());
    }

    // 7. Start CLI
    let mut cli_child = match Command::new(&cli_bin)
        .args(&cli_args[1..]) // skip the program name
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("[visp] Failed to start CLI: {e}");
            eprintln!("[visp] Make sure 'visp-tui' is installed.");
            kill_daemon(&mut daemon).await;
            std::process::exit(1);
        }
    };

    // 8. Wait for CLI to exit
    let cli_status = cli_child.wait().await.unwrap_or_else(|e| {
        eprintln!("[visp] Failed to wait for CLI: {e}");
        std::process::exit(1);
    });
    let exit_code = cli_status.code().unwrap_or(1);

    // 9. Send shutdown to daemon via gRPC, then wait for it to exit on its own.
    //    The timeout is only a safety net so the launcher never hangs.
    match shutdown_and_wait(
        &mut daemon,
        std::time::Duration::from_secs(5),
        send_shutdown(&addr),
    )
    .await
    {
        DaemonExit::Exited => eprintln!("[visp] Daemon stopped."),
        DaemonExit::WaitError(e) => eprintln!("[visp] Daemon wait error: {e}"),
        DaemonExit::TimedOutKilled => {
            eprintln!("[visp] Daemon did not stop in time, killing...");
        }
    }

    std::process::exit(exit_code);
}

/// Steps 1-5 shared by the TUI launcher and `visp run`: log file, free port,
/// spawn `visp-daemon`, wait for its health check. `None` = failed (already
/// reported on stderr, daemon killed).
async fn start_daemon(cli: &Cli) -> Option<(Child, String)> {
    // Resolve sibling binary paths (same dir as launcher for cargo run, or PATH)
    let daemon_bin = resolve_bin("visp-daemon");

    // 1. Create log directory
    let log_dir = visp_config::path::log_dir().unwrap_or_else(|| PathBuf::from("."));
    tokio::fs::create_dir_all(&log_dir)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to create log directory {}: {e}", log_dir.display());
            std::process::exit(1)
        });

    // 2. Prepare daemon log file
    let timestamp = format_timestamp();
    let log_path = log_dir.join(format!("daemon-{timestamp}.log"));
    let log_file = tokio::fs::File::create(&log_path)
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to create log file {}: {e}", log_path.display());
            std::process::exit(1)
        });
    let log_file_stdout = log_file.try_clone().await.unwrap();
    let log_file_stderr = log_file.try_clone().await.unwrap();

    // 3. Find an available port for the daemon.
    let addr = find_available_addr(&cli.addr).unwrap_or_else(|e| {
        eprintln!("[visp] {e}");
        std::process::exit(1);
    });
    eprintln!("[visp] Daemon will listen on {addr}");

    // 4. Start daemon, passing the selected address via env var.
    eprintln!("[visp] Starting daemon (log: {})...", log_path.display());
    let mut daemon_cmd = Command::new(&daemon_bin);
    daemon_cmd
        .env("VISP_LISTEN_ADDR", &addr)
        .stdout(log_file_stdout.into_std().await)
        .stderr(log_file_stderr.into_std().await);
    // Forward --config-dir if specified.
    if let Some(ref dir) = cli.config_dir {
        daemon_cmd.arg("--config-dir").arg(dir);
    }
    let mut daemon = match daemon_cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("[visp] Failed to start daemon: {e}");
            eprintln!("[visp] Make sure 'visp-daemon' is built. Try: cargo build");
            return None;
        }
    };

    // 5. Health check with timeout
    eprintln!("[visp] Waiting for daemon to be ready...");
    let health_ok =
        tokio::time::timeout(std::time::Duration::from_secs(15), wait_for_health(&addr)).await;

    match health_ok {
        Ok(true) => eprintln!("[visp] Daemon is ready."),
        Ok(false) | Err(_) => {
            // Daemon failed to start. Check if it wrote a startup error message.
            if let Some(err_file) = visp_config::path::startup_error_file()
                && let Ok(msg) = std::fs::read_to_string(&err_file)
                && !msg.is_empty()
            {
                // Print in red
                eprintln!("\x1b[31m{msg}\x1b[0m");
                let _ = std::fs::remove_file(&err_file);
            } else if health_ok.is_err() {
                eprintln!("[visp] Daemon did not become ready within 15s.");
            } else {
                eprintln!("[visp] Daemon health check failed.");
            }
            kill_daemon(&mut daemon).await;
            return None;
        }
    }

    Some((daemon, addr))
}

/// `visp run`: start the daemon, run one prompt, shut the daemon down.
async fn run_once(cli: &Cli, approve: bool, words: &[String]) -> i32 {
    use visp_proto::visp::{CreateSessionRequest, LlmConfig};

    if cli.session.is_some() {
        eprintln!("[visp run] --session is not supported yet; each run starts a new session");
        return 2;
    }
    let prompt = match run_cmd::resolve_prompt(words, std::io::stdin()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[visp run] {e}");
            return 2;
        }
    };

    let Some((mut daemon, addr)) = start_daemon(cli).await else {
        return 1;
    };

    let outcome =
        async {
            let mut client = CoderDaemonClient::connect(format!("http://{addr}"))
                .await
                .map_err(|e| format!("connect daemon: {e}"))?;

            // Same session config as the TUI's new-session path.
            let mut extra = std::collections::HashMap::new();
            if let Some(budget) = cli.thinking_budget {
                extra.insert("thinking_budget_tokens".into(), budget.to_string());
            }
            let config = (cli.model.is_some() || cli.temperature.is_some() || !extra.is_empty())
                .then(|| LlmConfig {
                    model: cli.model.clone(),
                    model_key: None,
                    temperature: cli.temperature,
                    max_tokens: None,
                    max_context_tokens: None,
                    extra,
                });
            let project = PathBuf::from(&cli.project);
            let project = project.canonicalize().unwrap_or(project);
            let session_id = client
                .create_session(CreateSessionRequest {
                    project_path: project.to_string_lossy().to_string(),
                    config,
                })
                .await
                .map_err(|e| format!("create session: {e}"))?
                .into_inner()
                .session_id;

            let (out_tx, out_rx) = tokio::sync::mpsc::channel(64);
            let mut stream = client
                .chat(tokio_stream::wrappers::ReceiverStream::new(out_rx))
                .await
                .map_err(|e| format!("chat stream: {e}"))?
                .into_inner();
            let (in_tx, mut in_rx) = tokio::sync::mpsc::channel(256);
            tokio::spawn(async move {
                while let Ok(Some(m)) = stream.message().await {
                    if in_tx.send(m).await.is_err() {
                        break;
                    }
                }
            });

            out_tx
                .send(run_cmd::user_input(&session_id, &prompt))
                .await
                .map_err(|_| "chat stream closed before the prompt was sent".to_string())?;

            let mut stats = run_cmd::RunStats::default();
            let started = std::time::Instant::now();
            let outcome = run_cmd::drive(
                &mut in_rx,
                &out_tx,
                &session_id,
                approve,
                &mut std::io::stdout(),
                &mut std::io::stderr(),
                async {
                    let _ = tokio::signal::ctrl_c().await;
                },
                &mut stats,
            )
            .await;
            eprintln!(
                "[visp run] {:?} in {:.0}s: {} tool calls, approvals {} granted / {} denied, \
             tokens (all agents) in {} / out {}",
                outcome,
                started.elapsed().as_secs_f64(),
                stats.tool_calls,
                stats.approvals_granted,
                stats.approvals_denied,
                stats.input_tokens,
                stats.output_tokens
            );
            Ok::<_, String>(outcome)
        }
        .await;

    let code = match outcome {
        Ok(o) => {
            if let run_cmd::RunOutcome::AgentError { code, message } = &o {
                eprintln!("[visp run] agent error {code}: {message}");
            }
            o.exit_code()
        }
        Err(e) => {
            eprintln!("[visp run] {e}");
            1
        }
    };
    let _ = shutdown_and_wait(
        &mut daemon,
        std::time::Duration::from_secs(5),
        send_shutdown(&addr),
    )
    .await;
    code
}

/// Dispatch launcher subcommands; returns the process exit code.
async fn run_subcommand(command: &LauncherCommand, cli: &Cli) -> i32 {
    match command {
        LauncherCommand::Hooks { action } => {
            hook_cmd::run(action, Path::new(&cli.project), &cli.addr, cli.dry_run).await
        }
        // Handled in main() before this dispatcher (it needs the daemon).
        LauncherCommand::Run { .. } => unreachable!("visp run is dispatched in main"),
    }
}

/// Outcome of waiting for the daemon process to exit.
#[derive(Debug)]
enum DaemonExit {
    /// The daemon exited on its own within the timeout.
    Exited,
    /// Waiting on the daemon failed.
    WaitError(std::io::Error),
    /// The daemon did not exit within the timeout; it was killed as a fallback.
    TimedOutKilled,
}

/// Wait for the daemon to exit on its own, killing it only if it exceeds
/// `timeout`.
///
/// This is the graceful-shutdown policy: the daemon is expected to exit on its
/// own after receiving the shutdown RPC; the timeout is a safety net so the
/// launcher never hangs.
async fn wait_for_daemon_exit(daemon: &mut Child, timeout: std::time::Duration) -> DaemonExit {
    match tokio::time::timeout(timeout, daemon.wait()).await {
        Ok(Ok(_)) => DaemonExit::Exited,
        Ok(Err(e)) => DaemonExit::WaitError(e),
        Err(_) => {
            let _ = kill_daemon(daemon).await;
            DaemonExit::TimedOutKilled
        }
    }
}

/// Request a graceful daemon shutdown, then wait for it to exit with a bounded
/// timeout fallback.
///
/// A failed `send_shutdown` is only logged; the wait (and the fallback kill)
/// still runs so the launcher never hangs or panics.
async fn shutdown_and_wait<Fut>(
    daemon: &mut Child,
    timeout: std::time::Duration,
    send_shutdown: Fut,
) -> DaemonExit
where
    Fut: std::future::Future<Output = Result<(), String>>,
{
    eprintln!("[visp] Shutting down daemon...");
    if let Err(e) = send_shutdown.await {
        eprintln!("[visp] gRPC shutdown failed: {e}");
    }
    wait_for_daemon_exit(daemon, timeout).await
}

/// 查找二进制路径：优先同目录（cargo run 场景），其次 PATH。
fn resolve_bin(name: &str) -> PathBuf {
    // 检查 launcher 同目录
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let sibling = parent.join(name);
        if sibling.is_file() {
            return sibling;
        }
    }
    // 回退到 PATH
    PathBuf::from(name)
}

/// Parse an address like "[::1]:50051" or "127.0.0.1:9090" into (host, port).
fn parse_addr(addr: &str) -> Result<(String, u16), String> {
    // Handle bracketed IPv6: [::1]:50051
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some(bracket_end) = rest.find(']') {
            let host = &rest[..bracket_end];
            let after = &rest[bracket_end + 1..];
            if let Some(port_str) = after.strip_prefix(':') {
                let port: u16 = port_str
                    .parse()
                    .map_err(|_| format!("invalid port in address: {addr}"))?;
                return Ok((host.to_string(), port));
            }
        }
        return Err(format!("invalid address: {addr}"));
    }
    // Handle plain host:port
    let (host, port_str) = addr
        .rsplit_once(':')
        .ok_or_else(|| format!("invalid address (no port): {addr}"))?;
    let port: u16 = port_str
        .parse()
        .map_err(|_| format!("invalid port in address: {addr}"))?;
    Ok((host.to_string(), port))
}

/// Reassemble host+port into an address string, bracketing IPv6 hosts.
fn format_addr(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Find an available address starting from `base_addr`. Tries the given port
/// first; if it is already in use, increments the port up to 1000 attempts.
fn find_available_addr(base_addr: &str) -> Result<String, String> {
    let (host, base_port) = parse_addr(base_addr)?;
    for offset in 0..1000u16 {
        let port = base_port.saturating_add(offset);
        let addr = format_addr(&host, port);
        let sock_addr: std::net::SocketAddr = addr
            .parse()
            .map_err(|e| format!("invalid address {addr}: {e}"))?;
        if std::net::TcpListener::bind(sock_addr).is_ok() {
            return Ok(addr);
        }
        // Port is in use; try the next one.
        if offset == 0 {
            eprintln!("[visp] Port {port} is in use, trying next available port...");
        }
    }
    Err(format!(
        "could not find an available port starting from {base_port}"
    ))
}

async fn wait_for_health(addr: &str) -> bool {
    let endpoint = format!("http://{addr}");
    for _ in 0..30 {
        if let Ok(true) = connect_and_check(&endpoint).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    false
}

async fn connect_and_check(endpoint: &str) -> Result<bool, String> {
    let ch = Endpoint::new(endpoint.to_string())
        .map_err(|e| format!("endpoint: {e}"))?
        .connect()
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let mut client = CoderDaemonClient::new(ch);
    let resp = client
        .health_check(())
        .await
        .map_err(|e| format!("health: {e}"))?;
    Ok(resp.into_inner().alive)
}

async fn send_shutdown(addr: &str) -> Result<(), String> {
    let endpoint = format!("http://{addr}");
    let ch = Endpoint::new(endpoint)
        .map_err(|e| format!("endpoint: {e}"))?
        .connect()
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let mut client = CoderDaemonClient::new(ch);
    client
        .shutdown(ShutdownRequest { force: false })
        .await
        .map_err(|e| format!("shutdown: {e}"))?;
    Ok(())
}

fn format_timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string()
}

async fn kill_daemon(daemon: &mut Child) {
    let _ = daemon.kill().await;
    let _ = daemon.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Spawn a `sleep <secs>` child used to emulate the daemon process.
    async fn spawn_sleep(secs: &str) -> Child {
        Command::new("sleep").arg(secs).spawn().unwrap()
    }

    #[tokio::test]
    async fn daemon_self_exit_within_timeout_is_not_killed() {
        let mut daemon = spawn_sleep("0.05").await;
        let outcome = wait_for_daemon_exit(&mut daemon, Duration::from_secs(5)).await;
        assert!(matches!(outcome, DaemonExit::Exited));
        // Exited on its own; the fallback kill was not the reason it is gone.
        assert!(daemon.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn daemon_timeout_falls_back_to_kill() {
        let mut daemon = spawn_sleep("10").await;
        let outcome = wait_for_daemon_exit(&mut daemon, Duration::from_millis(100)).await;
        assert!(matches!(outcome, DaemonExit::TimedOutKilled));
        // The fallback kill reaped the child, so nothing is left dangling.
        assert!(daemon.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn kill_daemon_terminates_running_child() {
        let mut daemon = spawn_sleep("10").await;
        kill_daemon(&mut daemon).await;
        assert!(daemon.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn send_shutdown_failure_still_waits_for_self_exit() {
        let mut daemon = spawn_sleep("0.05").await;
        let outcome = shutdown_and_wait(&mut daemon, Duration::from_secs(5), async {
            Err("boom".to_string())
        })
        .await;
        assert!(matches!(outcome, DaemonExit::Exited));
    }

    #[tokio::test]
    async fn send_shutdown_failure_still_falls_back_to_kill() {
        let mut daemon = spawn_sleep("10").await;
        let outcome = shutdown_and_wait(&mut daemon, Duration::from_millis(100), async {
            Err("boom".to_string())
        })
        .await;
        assert!(matches!(outcome, DaemonExit::TimedOutKilled));
        assert!(daemon.try_wait().unwrap().is_some());
    }

    #[test]
    fn session_short_flag_passthrough() {
        let cli = Cli::try_parse_from(["visp", "-s", "abc"]).unwrap();
        assert_eq!(cli.session.as_deref(), Some("abc"));
    }

    #[test]
    fn session_long_flag_passthrough() {
        let cli = Cli::try_parse_from(["visp", "--session", "xyz"]).unwrap();
        assert_eq!(cli.session.as_deref(), Some("xyz"));
    }

    #[test]
    fn no_session_defaults_to_none() {
        let cli = Cli::try_parse_from(["visp"]).unwrap();
        assert!(cli.session.is_none());
    }

    #[test]
    fn session_with_project_short() {
        let cli = Cli::try_parse_from(["visp", "-s", "abc", "-p", "/tmp"]).unwrap();
        assert_eq!(cli.session.as_deref(), Some("abc"));
        assert_eq!(cli.project, "/tmp");
    }

    #[test]
    fn session_with_project_long() {
        let cli = Cli::try_parse_from(["visp", "--session", "abc", "--project", "/tmp"]).unwrap();
        assert_eq!(cli.session.as_deref(), Some("abc"));
        assert_eq!(cli.project, "/tmp");
    }

    #[test]
    fn session_alone_uses_default_project() {
        let cli = Cli::try_parse_from(["visp", "-s", "abc"]).unwrap();
        assert_eq!(cli.session.as_deref(), Some("abc"));
        assert_eq!(cli.project, ".");
    }

    #[test]
    fn config_dir_parsed() {
        let cli = Cli::try_parse_from(["visp", "--config-dir", "/custom/config"]).unwrap();
        assert_eq!(cli.config_dir.as_deref(), Some("/custom/config"));
    }

    #[test]
    fn config_dir_absent_defaults_none() {
        let cli = Cli::try_parse_from(["visp"]).unwrap();
        assert!(cli.config_dir.is_none());
    }

    #[test]
    fn hooks_list_subcommand_parses() {
        let cli = Cli::try_parse_from(["visp", "hooks", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(LauncherCommand::Hooks {
                action: HooksAction::List
            })
        ));
        assert!(!cli.dry_run);
    }

    #[test]
    fn hooks_doctor_subcommand_parses() {
        let cli = Cli::try_parse_from(["visp", "hooks", "doctor"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(LauncherCommand::Hooks {
                action: HooksAction::Doctor
            })
        ));
    }

    #[test]
    fn hooks_test_subcommand_takes_target() {
        let cli = Cli::try_parse_from(["visp", "hooks", "test", "Stop"]).unwrap();
        match cli.command {
            Some(LauncherCommand::Hooks {
                action: HooksAction::Test { target },
            }) => assert_eq!(target, "Stop"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn hooks_logs_subcommand_parses() {
        let cli = Cli::try_parse_from(["visp", "hooks", "logs"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(LauncherCommand::Hooks {
                action: HooksAction::Logs
            })
        ));
    }

    #[test]
    fn global_dry_run_flag_parses_after_subcommand() {
        let cli = Cli::try_parse_from(["visp", "hooks", "test", "Stop", "--dry-run"]).unwrap();
        assert!(cli.dry_run);
    }

    #[test]
    fn no_subcommand_defaults_to_none() {
        let cli = Cli::try_parse_from(["visp"]).unwrap();
        assert!(cli.command.is_none());
    }
}
