//! visp-acp — visp 的 ACP(Agent Client Protocol)agent。
//!
//! 由 ACP 客户端(如 Zed)以子进程方式启动,stdin/stdout 走 JSON-RPC(ACP),
//! 内部通过 gRPC 连接(默认自行拉起)`visp-daemon`。日志一律走 stderr,
//! stdout 仅输出 ACP 协议消息。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

pub mod agent;
pub mod approval;
pub mod grpc;
pub mod sessions;
pub mod supervisor;
pub mod translate;

/// 自拉起模式的端口探测基准地址。
pub const DEFAULT_LISTEN_ADDR: &str = "[::1]:50051";

/// visp-acp 命令行参数。
#[derive(Parser, Debug)]
#[command(name = "visp-acp", version, about = "visp ACP agent (stdio)")]
pub struct Cli {
    /// 项目路径(默认当前目录)
    #[arg(short = 'p', long, default_value = ".")]
    pub project: PathBuf,

    /// 直连的 daemon gRPC 地址;缺省时自动探测空闲端口并拉起 daemon
    #[arg(short = 'a', long, value_parser = clap::value_parser!(SocketAddr))]
    pub addr: Option<SocketAddr>,

    /// 独立 config 目录(默认隔离;显式指向同一目录可与 CLI 共享会话存储)
    #[arg(long)]
    pub config_dir: Option<PathBuf>,

    /// 直连模式下退出时也向 daemon 发送 Shutdown(默认直连不关 daemon)
    #[arg(long)]
    pub shutdown_on_exit: bool,

    /// 新会话的 LLM 模型(API model key)。ACP v1 没有模型选择请求,
    /// 客户端(`visp-tui run --model`)以此启动参数传给 visp-acp。
    #[arg(long)]
    pub model: Option<String>,

    /// 新会话的 LLM temperature。
    #[arg(long)]
    pub temperature: Option<f64>,

    /// 新会话的 thinking budget(写入 llm.extra.thinking_budget_tokens)。
    #[arg(long)]
    pub thinking_budget: Option<u32>,
}

impl Cli {
    /// 自拉起模式的端口探测基准地址(未指定 `--addr` 时)。
    pub fn base_addr(&self) -> SocketAddr {
        self.addr
            .unwrap_or_else(|| DEFAULT_LISTEN_ADDR.parse().expect("valid default addr"))
    }

    /// 启动参数映射出的初始会话配置(全部未指定时返回 `None`,沿用 daemon 默认)。
    pub fn llm_config(&self) -> Option<visp_proto::visp::LlmConfig> {
        let mut extra = std::collections::HashMap::new();
        if let Some(budget) = self.thinking_budget {
            extra.insert("thinking_budget_tokens".to_string(), budget.to_string());
        }
        (self.model.is_some() || self.temperature.is_some() || !extra.is_empty()).then(|| {
            visp_proto::visp::LlmConfig {
                model: self.model.clone(),
                model_key: None,
                temperature: self.temperature,
                max_tokens: None,
                max_context_tokens: None,
                extra,
            }
        })
    }
}

/// 初始化 stderr 日志(ACP 纪律:stdout 仅输出协议消息,日志绝不写 stdout)。
pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// visp-acp 主流程(§6.1):daemon 获取 → ACP 事件循环 → 按模式退出收尾。
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    use agent_client_protocol::Stdio;

    // 1. daemon 获取:`--addr` 直连(受 V1 约束)或自拉起(默认,§4.3)
    let (addr, launch_mode, mut child) = match cli.addr {
        Some(addr) => (addr.to_string(), supervisor::LaunchMode::Attached, None),
        None => {
            let addr = supervisor::find_available_addr(supervisor::DEFAULT_LISTEN_ADDR)?;
            let log_path = supervisor::daemon_log_path()?;
            tracing::info!(%addr, "starting daemon: {}", supervisor::daemon_log_notice(&log_path));
            let daemon_bin = supervisor::resolve_bin("visp-daemon");
            let mut child = supervisor::spawn_daemon(
                &daemon_bin,
                &[],
                &addr,
                &log_path,
                cli.config_dir.as_deref(),
            )
            .await?;
            // health check 期间监控子进程存活(TOCTOU 防御,§6.1 步骤 3)
            if let Err(e) =
                supervisor::wait_for_health_ready(&mut child, &addr, Duration::from_secs(15)).await
            {
                supervisor::kill(&mut child).await?;
                return Err(e);
            }
            (addr, supervisor::LaunchMode::Spawned, Some(child))
        }
    };

    // 2. ACP agent 事件循环(stdout 仅协议消息);同时监听 SIGTERM/SIGINT
    let result = tokio::select! {
        r = async {
            let session = crate::grpc::GrpcSession::connect(&addr).await?;
            let (outbound, inbound, client) = session.into_parts();
            let state = Arc::new(
                agent::AgentState::new(outbound, inbound, client)
                    .with_llm_config(cli.llm_config()),
            );
            agent::run_agent(Stdio::new(), state).await
        } => r,
        signal = supervisor::termination_signal() => {
            tracing::info!(signal = %signal, "received termination signal; shutting down");
            // 信号路径收尾:自拉起发 Shutdown(超时强杀);直连默认不动(§6.1 步骤 6)
            return supervisor::handle_termination(
                child.as_mut(),
                &addr,
                launch_mode,
                cli.shutdown_on_exit,
            )
            .await;
        }
    };

    // 3. 正常结束收尾:自拉起发 Shutdown(超时强杀);直连默认不动(§6.1 步骤 6)
    if let Some(child) = child.as_mut() {
        supervisor::exit_daemon(
            Some(child),
            &addr,
            supervisor::shutdown_policy(launch_mode, cli.shutdown_on_exit),
        )
        .await?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn parse<const N: usize>(args: [&str; N]) -> Cli {
        Cli::parse_from(args)
    }

    #[test]
    fn project_defaults_to_current_dir() {
        let cli = parse(["visp-acp"]);
        assert_eq!(cli.project, PathBuf::from("."));
    }

    #[test]
    fn addr_defaults_to_probe_mode_with_base_addr() {
        let cli = parse(["visp-acp"]);
        assert!(cli.addr.is_none(), "缺省应处于自拉起探测模式");
        assert_eq!(
            cli.base_addr(),
            SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 50051)
        );
    }

    #[test]
    fn addr_direct_connect_when_given() {
        let cli = parse(["visp-acp", "--addr", "127.0.0.1:9999"]);
        assert_eq!(
            cli.addr,
            Some(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 9999))
        );
        assert_eq!(cli.base_addr(), cli.addr.unwrap());
    }

    #[test]
    fn addr_rejects_invalid_value() {
        let result = Cli::try_parse_from(["visp-acp", "--addr", "not-an-address"]);
        assert!(result.is_err(), "非法地址应解析失败");
    }

    #[test]
    fn config_dir_and_shutdown_on_exit_parse() {
        let cli = parse([
            "visp-acp",
            "--config-dir",
            "/tmp/visp-test",
            "--shutdown-on-exit",
        ]);
        assert_eq!(cli.config_dir, Some(PathBuf::from("/tmp/visp-test")));
        assert!(cli.shutdown_on_exit);
    }

    #[test]
    fn shutdown_on_exit_defaults_to_false() {
        let cli = parse(["visp-acp"]);
        assert!(!cli.shutdown_on_exit);
    }

    #[test]
    fn llm_config_is_none_without_overrides() {
        assert!(parse(["visp-acp"]).llm_config().is_none());
    }

    #[test]
    fn llm_config_carries_launch_overrides() {
        let cli = parse([
            "visp-acp",
            "--model",
            "provider/model",
            "--temperature",
            "0.2",
            "--thinking-budget",
            "2048",
        ]);
        let config = cli.llm_config().expect("overrides must produce a config");
        assert_eq!(config.model.as_deref(), Some("provider/model"));
        assert_eq!(config.model_key, None);
        assert_eq!(config.temperature, Some(0.2));
        assert_eq!(
            config
                .extra
                .get("thinking_budget_tokens")
                .map(String::as_str),
            Some("2048")
        );
    }
}
