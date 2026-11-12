mod app;
mod client;
mod command;
mod connection;
mod event;
mod image;
mod notify;
mod run_mode;
mod selection;
mod theme;
mod tool_ui;
mod ui;

use clap::{Parser, Subcommand};
use client::VispClient;
use crossterm::tty::IsTty;
use visp_proto::visp::LlmConfig as ProtoLlmConfig;

#[derive(Parser)]
#[command(name = "visp", about = "visp CLI — AI coding assistant")]
struct Cli {
    #[arg(short = 'a', long, default_value = "[::1]:50051")]
    addr: String,

    #[arg(short = 'p', long, default_value = ".")]
    project: String,

    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    temperature: Option<f64>,

    #[arg(long)]
    thinking_budget: Option<u32>,

    #[arg(short = 's', long)]
    session: Option<String>,

    #[arg(long)]
    list: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run one prompt non-interactively and exit (for scripts / CI).
    ///
    /// Acts as an ACP client: spawns `visp-acp`, which in turn starts the
    /// daemon. The answer is printed on stdout, progress on stderr.
    Run(run_mode::RunArgs),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Non-interactive mode: no TUI, no direct daemon connection.
    if let Some(Command::Run(args)) = cli.command {
        std::process::exit(run_mode::run(args).await);
    }

    // ── 加载配置（通知等可选功能；失败不阻断启动）──────────────────
    let config = match visp_config::load_config(None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Warning: failed to load config, using defaults: {e}");
            visp_config::DaemonConfig::default()
        }
    };
    let notification = config.notification;
    // 协议 override 目前只影响 selected_protocol() 的可观测值；BEL 编码与协议无关。
    let protocol_override = match notification.protocol {
        visp_config::NotificationProtocol::Auto => None,
        visp_config::NotificationProtocol::Osc9 => Some(notify::Protocol::Osc9),
        visp_config::NotificationProtocol::Osc777 => Some(notify::Protocol::Osc777),
        visp_config::NotificationProtocol::Kitty99 => Some(notify::Protocol::Kitty99),
    };
    let notify_engine = notify::NotifyEngine::new(
        std::io::stdout().is_tty(),
        protocol_override,
        notification.enabled,
        notification.on_complete,
        notification.on_user_input,
        std::time::Duration::from_secs(notification.min_interval_secs),
    );
    tracing::debug!(
        "notification engine: protocol={:?}",
        notify_engine.selected_protocol()
    );

    let mut client = match VispClient::connect(&cli.addr).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to connect to daemon at {}: {}", cli.addr, e);
            eprintln!("Start daemon with: visp-daemon");
            std::process::exit(1);
        }
    };

    match client.health_check().await {
        Ok(true) => {}
        _ => {
            eprintln!("Daemon is not healthy at {}", cli.addr);
            std::process::exit(1);
        }
    }

    // ── --list mode ──────────────────────────────────────────
    if cli.list {
        match client.list_sessions().await {
            Ok(sessions) => {
                if sessions.is_empty() {
                    println!("No sessions found.");
                } else {
                    // Filter by project if -p provided
                    let project_path = std::path::PathBuf::from(&cli.project);
                    let project_str = project_path.to_string_lossy().to_string();
                    let filtered: Vec<_> = sessions
                        .iter()
                        .filter(|s| s.project_path == project_str)
                        .collect();

                    if filtered.is_empty() {
                        println!("No sessions found for project: {}", project_str);
                    } else {
                        println!("Sessions (project: {}):", project_str);
                        for s in &filtered {
                            let short_id: String = s.session_id.chars().take(8).collect();
                            let status = format!("{:?}", s.status);
                            println!("  {}  {}  {}", short_id, status, s.session_id);
                        }
                        println!("\nUse: visp -s <short-id> to resume a session");
                    }
                }
            }
            Err(e) => {
                eprintln!("Failed to list sessions: {}", e);
            }
        }
        return;
    }

    // ── Resolve session (new or existing) ───────────────────
    let session = if let Some(session_id) = &cli.session {
        // Resume existing session
        match client.get_session(session_id).await {
            Ok(s) => {
                // Validate -p matches session's project_path
                let project_path = std::path::PathBuf::from(&cli.project);
                let resolved_path = project_path.canonicalize().unwrap_or(project_path);
                let project_str = resolved_path.to_string_lossy().to_string();
                if s.project_path != project_str {
                    eprintln!(
                        "Error: Session '{}' belongs to project '{}', but -p is '{}'.",
                        s.session_id.chars().take(8).collect::<String>(),
                        s.project_path,
                        project_str
                    );
                    eprintln!("Use the correct project path: visp -p <project> -s <session-id>");
                    std::process::exit(1);
                }
                s
            }
            Err(e) => {
                eprintln!("Session '{}' not found: {}", session_id, e);
                // List recent sessions for guidance
                match client.list_sessions().await {
                    Ok(sessions) if !sessions.is_empty() => {
                        let project_path = std::path::PathBuf::from(&cli.project);
                        let resolved_path = project_path.canonicalize().unwrap_or(project_path);
                        let project_str = resolved_path.to_string_lossy().to_string();
                        let filtered: Vec<_> = sessions
                            .iter()
                            .filter(|s| s.project_path == project_str)
                            .collect();
                        if !filtered.is_empty() {
                            eprintln!("\nRecent sessions for '{}':", project_str);
                            for s in &filtered {
                                let short_id: String = s.session_id.chars().take(8).collect();
                                let status = format!("{:?}", s.status);
                                eprintln!("  {}  {}", short_id, status);
                            }
                            eprintln!("\nUse: visp -s <short-id> to resume");
                        }
                    }
                    _ => {}
                }
                std::process::exit(1);
            }
        }
    } else {
        // Create new session (existing logic)
        let mut extra = std::collections::HashMap::new();
        if let Some(budget) = cli.thinking_budget {
            extra.insert("thinking_budget_tokens".into(), budget.to_string());
        }
        let config = if cli.model.is_some() || cli.temperature.is_some() || !extra.is_empty() {
            Some(ProtoLlmConfig {
                model: cli.model.clone(),
                model_key: None,
                temperature: cli.temperature,
                max_tokens: None,
                max_context_tokens: None,
                extra,
            })
        } else {
            None
        };

        let project = std::path::PathBuf::from(&cli.project);
        let project_path = project.canonicalize().unwrap_or(project);
        let project_str = project_path.to_string_lossy().to_string();

        match client.create_session(&project_str, config).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to create session: {}", e);
                std::process::exit(1);
            }
        }
    };

    // ── Start chat (shared for both new and resumed) ────────
    let session_id = session.session_id.clone();
    let chat_handle = match client.chat(&session_id).await {
        Ok(ch) => ch,
        Err(e) => {
            eprintln!("Failed to start chat: {}", e);
            std::process::exit(1);
        }
    };

    // Send join message so daemon sends history immediately (before first UserInput)
    chat_handle.send_join();

    let model = cli.model.clone().unwrap_or_else(|| session.model.clone());
    let model_key = session.model_key.clone();
    let sid_for_display = session.session_id.clone();
    if let Err(e) = event::run(
        session_id,
        chat_handle,
        model.clone(),
        model_key,
        client,
        cli.addr.clone(),
        session.project_path.as_str(),
        session.available_models.clone(),
        session.model_keys.clone(),
        notify_engine,
    )
    .await
    {
        eprintln!("Event loop error: {}", e);
    }

    eprintln!("Session closed: {}", sid_for_display);
}
