//! daemon 编排器:端口探测、spawn、health check(含子进程存活监控)、按模式退出收尾。
//!
//! 逻辑移植自 launcher(`crates/visp/src/main.rs`);按设计 §4.4 不抽公共 crate。
//! 关键修订(设计 §6.1 三轮):health check 期间监控子进程存活(TOCTOU 防御)、
//! 退出收尾按启动模式区分(直连默认不 Shutdown)。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail};
use tokio::process::{Child, Command};
use tonic::transport::Endpoint;
use visp_proto::visp::ShutdownRequest;
use visp_proto::visp::coder_daemon_client::CoderDaemonClient;

/// 自拉起探测的最大递增端口数。
const MAX_PORT_ATTEMPTS: u16 = 1000;

/// 端口探测基准地址(`--addr` 未指定时)。
pub const DEFAULT_LISTEN_ADDR: &str = "[::1]:50051";

// ===== 3a 端口探测 =====

/// 解析地址("[::1]:50051" / "127.0.0.1:9090" / "host:port")为 (host, port)。
pub fn parse_addr(addr: &str) -> anyhow::Result<(String, u16)> {
    // 方括号 IPv6:[::1]:50051
    if let Some(rest) = addr.strip_prefix('[') {
        let bracket_end = rest
            .find(']')
            .ok_or_else(|| anyhow!("invalid address (missing ']'): {addr}"))?;
        let host = &rest[..bracket_end];
        let after = &rest[bracket_end + 1..];
        let port_str = after
            .strip_prefix(':')
            .ok_or_else(|| anyhow!("invalid address (no port after ']'): {addr}"))?;
        let port: u16 = port_str
            .parse()
            .map_err(|_| anyhow!("invalid port in address: {addr}"))?;
        return Ok((host.to_string(), port));
    }
    // 普通 host:port
    let (host, port_str) = addr
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("invalid address (no port): {addr}"))?;
    let port: u16 = port_str
        .parse()
        .map_err(|_| anyhow!("invalid port in address: {addr}"))?;
    Ok((host.to_string(), port))
}

/// 重组 host+port 为地址字符串(IPv6 host 加方括号)。
pub fn format_addr(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// 从基准地址开始探测空闲端口(试绑验证),占用则递增。
pub fn find_available_addr(base_addr: &str) -> anyhow::Result<String> {
    find_available_addr_from(base_addr, MAX_PORT_ATTEMPTS)
}

fn find_available_addr_from(base_addr: &str, max_attempts: u16) -> anyhow::Result<String> {
    let (host, base_port) = parse_addr(base_addr)?;
    for offset in 0..max_attempts {
        let port = base_port.saturating_add(offset);
        let addr_str = format_addr(&host, port);
        let sock_addr: SocketAddr = addr_str
            .parse()
            .map_err(|e| anyhow!("invalid address {addr_str}: {e}"))?;
        // 试绑验证:成功立即释放,真实绑定由 daemon 完成
        if std::net::TcpListener::bind(sock_addr).is_ok() {
            return Ok(addr_str);
        }
    }
    bail!("could not find an available port starting from {base_port} ({max_attempts} attempts)")
}

// ===== 3b spawn =====

/// 解析兄弟二进制路径(同目录优先,回退 PATH)。
pub fn resolve_bin(name: &str) -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let sibling = parent.join(name);
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from(name)
}

/// 子进程早期输出捕获文件路径(`{log_dir}/daemon-{timestamp}.log`)。
///
/// 该文件只承接 daemon 的 stdout/stderr(panic/早期错误,以及显式关闭
/// 文件日志时的全部输出);daemon 自身的 tracing 滚动日志另见
/// [`daemon_log_notice`]。
pub fn daemon_log_path() -> anyhow::Result<PathBuf> {
    let dir = visp_config::path::log_dir().unwrap_or_else(std::env::temp_dir);
    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    Ok(dir.join(format!("daemon-{timestamp}.log")))
}

/// 构造「daemon 真实日志位置」提示文本,用于替掉误导性的重定向文件提示。
///
/// daemon 自身把 tracing 日志写入 `[observability] log_file` 目录下的滚动文件
/// `visp-daemon.log.<date>`(默认 `~/.visp/logs`,见
/// `crates/visp-daemon/src/observability/init.rs`);而 `redirect_path`
/// 对应的 `daemon-<ts>.log` **不是** daemon 的日志,只捕获其 stdout/stderr。
///
/// 之所以不把两者合一:`observability.log_file` 没有 CLI/环境变量注入点
/// (仅 `daemon.toml` 可配置,见 `visp-config` 的 `load_config`),无法让 daemon
/// 直接写到 `daemon-<ts>.log`。
pub fn daemon_log_notice(redirect_path: &Path) -> String {
    let dir = visp_config::path::log_dir().unwrap_or_else(std::env::temp_dir);
    format!(
        "daemon rolling log: {}/visp-daemon.log.<date> \
         (default [observability] log_file; set log_file in daemon.toml to change); \
         early stdout/stderr captured in {}",
        dir.display(),
        redirect_path.display()
    )
}

/// 启动 daemon:注入 `VISP_LISTEN_ADDR`,stdout/stderr 重定向到日志文件,
/// `--config-dir` 透传(额外参数用于测试注入假进程)。
pub async fn spawn_daemon(
    daemon_bin: &Path,
    extra_args: &[String],
    addr: &str,
    log_path: &Path,
    config_dir: Option<&Path>,
) -> anyhow::Result<Child> {
    // 全新环境下日志目录(默认 ~/.visp/logs)可能尚不存在;launcher 会先建目录,
    // visp-acp 自拉起路径此前没有,导致首次运行即失败。
    if let Some(parent) = log_path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| anyhow!("failed to create log directory {}: {e}", parent.display()))?;
    }
    let log_file = tokio::fs::File::create(log_path)
        .await
        .map_err(|e| anyhow!("failed to create log file {}: {e}", log_path.display()))?;
    let log_stdout = log_file.try_clone().await?;
    let log_stderr = log_file.try_clone().await?;

    let mut cmd = Command::new(daemon_bin);
    for arg in extra_args {
        cmd.arg(arg);
    }
    if let Some(dir) = config_dir {
        cmd.arg("--config-dir").arg(dir);
    }
    cmd.env("VISP_LISTEN_ADDR", addr)
        .stdout(log_stdout.into_std().await)
        .stderr(log_stderr.into_std().await);

    cmd.spawn()
        .map_err(|e| anyhow!("failed to start daemon {}: {e}", daemon_bin.display()))
}

// ===== 3c health check =====

/// 轮询 HealthCheck 直至就绪(带超时)。
///
/// **每轮先检查子进程存活**(`try_wait`):子进程已退出(典型为端口绑定失败,
/// 并发启动 TOCTOU)→ 立即报错并附带 `startup_error_file()` 诊断——
/// 不得继续对探测端口做 health check,否则会误连占用该端口的其他 daemon 实例。
pub async fn wait_for_health_ready(
    child: &mut Child,
    addr: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let endpoint = format!("http://{addr}");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // TOCTOU 防御:子进程已退出 → 立即失败,不继续探测
        if let Some(status) = child.try_wait()? {
            let diag = take_startup_error().unwrap_or_default();
            bail!(
                "daemon exited early (status: {status}){}",
                if diag.is_empty() {
                    String::new()
                } else {
                    format!("; startup error: {diag}")
                }
            );
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("daemon did not become ready within {timeout:?}");
        }
        if matches!(connect_and_check(&endpoint).await, Ok(true)) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn connect_and_check(endpoint: &str) -> anyhow::Result<bool> {
    let ch = Endpoint::new(endpoint.to_string())
        .map_err(|e| anyhow!("endpoint: {e}"))?
        .connect()
        .await
        .map_err(|e| anyhow!("connect: {e}"))?;
    let mut client = CoderDaemonClient::new(ch);
    let resp = client
        .health_check(())
        .await
        .map_err(|e| anyhow!("health: {e}"))?;
    Ok(resp.into_inner().alive)
}

/// 读取并删除 daemon 启动错误文件(存在且非空时)。
fn take_startup_error() -> Option<String> {
    let path = visp_config::path::startup_error_file()?;
    let msg = std::fs::read_to_string(&path).ok()?;
    if msg.is_empty() {
        return None;
    }
    let _ = std::fs::remove_file(&path);
    Some(msg)
}

// ===== 3d 退出收尾 =====

/// daemon 启动模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// 本进程拉起(默认路径)
    Spawned,
    /// `--addr` 直连既有 daemon
    Attached,
}

/// 退出收尾策略(§6.1 步骤 6,按模式区分)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownPolicy {
    /// 发 Shutdown,超时强杀子进程(仅自拉起模式)
    SendThenKill,
    /// 发 Shutdown,但无子进程可杀(直连 + 显式 --shutdown-on-exit)
    SendOnly,
    /// 不发 Shutdown、不强杀(直连默认:daemon 可能正被其他客户端使用)
    Leave,
}

pub fn shutdown_policy(mode: LaunchMode, shutdown_on_exit: bool) -> ShutdownPolicy {
    match mode {
        LaunchMode::Spawned => ShutdownPolicy::SendThenKill,
        LaunchMode::Attached if shutdown_on_exit => ShutdownPolicy::SendOnly,
        LaunchMode::Attached => ShutdownPolicy::Leave,
    }
}

/// 执行退出收尾。`child` 仅自拉起模式持有;直连模式传 `None`。
pub async fn exit_daemon(
    child: Option<&mut Child>,
    addr: &str,
    policy: ShutdownPolicy,
) -> anyhow::Result<()> {
    match policy {
        ShutdownPolicy::Leave => Ok(()),
        ShutdownPolicy::SendOnly => send_shutdown(addr).await,
        ShutdownPolicy::SendThenKill => {
            send_shutdown(addr).await?;
            let Some(child) = child else {
                return Ok(());
            };
            // 优雅退出窗口;超时强杀
            match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(_) => Ok(()),
                Err(_) => kill(child).await,
            }
        }
    }
}

/// 向 daemon 发送 Shutdown 请求。
pub async fn send_shutdown(addr: &str) -> anyhow::Result<()> {
    let ch = Endpoint::new(format!("http://{addr}"))
        .map_err(|e| anyhow!("endpoint: {e}"))?
        .connect()
        .await
        .map_err(|e| anyhow!("connect: {e}"))?;
    let mut client = CoderDaemonClient::new(ch);
    client
        .shutdown(ShutdownRequest { force: false })
        .await
        .map_err(|e| anyhow!("shutdown: {e}"))?;
    Ok(())
}

/// 强杀子进程并等待退出。
pub async fn kill(child: &mut Child) -> anyhow::Result<()> {
    let _ = child.kill().await;
    let _ = child.wait().await;
    Ok(())
}

/// 等待终止信号(SIGTERM/SIGINT),返回触发退出的信号名用于日志。
///
/// 非 Unix 平台退化为 Ctrl-C(SIGINT)。信号注册失败时返回的 future 永不就绪,
/// 避免误退出。
pub async fn termination_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (mut sigterm, mut sigint) = match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(t), Ok(i)) => (t, i),
            _ => {
                // 注册失败:永不就绪,避免误退出
                std::future::pending::<()>().await;
                unreachable!()
            }
        };
        tokio::select! {
            _ = sigterm.recv() => "SIGTERM",
            _ = sigint.recv() => "SIGINT",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
}

/// 终止信号到达后的收尾入口:复用 [`exit_daemon`](§6.1 步骤 6)语义,
/// 自拉起模式向 daemon 发 Shutdown(超时强杀),直连模式默认不打扰。
///
/// **局限**:若父进程(如 Zed)以 SIGKILL 终止本进程,信号不可捕获,本函数
/// 不会执行,自拉起的 daemon 仍可能残留为孤儿;彻底解法是 daemon 侧
/// 「父进程死亡自退」(设计级改动,不属本函数职责)。
pub async fn handle_termination(
    child: Option<&mut Child>,
    addr: &str,
    mode: LaunchMode,
    shutdown_on_exit: bool,
) -> anyhow::Result<()> {
    let policy = shutdown_policy(mode, shutdown_on_exit);
    tracing::info!(
        ?policy,
        "termination signal received; running shutdown cleanup"
    );
    exit_daemon(child, addr, policy).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn parse_addr_ipv6_bracketed() {
        assert_eq!(parse_addr("[::1]:50051").unwrap(), ("::1".into(), 50051));
    }

    #[test]
    fn parse_addr_ipv4() {
        assert_eq!(
            parse_addr("127.0.0.1:9090").unwrap(),
            ("127.0.0.1".into(), 9090)
        );
    }

    #[test]
    fn parse_addr_rejects_invalid() {
        assert!(parse_addr("no-port").is_err());
        assert!(parse_addr("[::1]:notaport").is_err());
        assert!(parse_addr("[::1").is_err());
    }

    #[test]
    fn format_addr_brackets_ipv6_only() {
        assert_eq!(format_addr("::1", 50051), "[::1]:50051");
        assert_eq!(format_addr("127.0.0.1", 9090), "127.0.0.1:9090");
    }

    #[test]
    fn default_listen_addr_is_valid() {
        let (host, port) = parse_addr(DEFAULT_LISTEN_ADDR).unwrap();
        let sock: SocketAddr = format_addr(&host, port).parse().unwrap();
        assert_eq!(sock.ip(), Ipv6Addr::LOCALHOST);
        assert_eq!(sock.port(), 50051);
    }

    #[tokio::test]
    async fn find_available_addr_skips_occupied_port() {
        // 占住一个冷门端口,探测应从下一个端口找到
        let listener = std::net::TcpListener::bind("127.0.0.1:59918").unwrap();
        let base = listener.local_addr().unwrap().to_string();
        let found = find_available_addr_from(&base, 10).unwrap();
        let found: SocketAddr = found.parse().unwrap();
        assert_eq!(found.ip(), Ipv4Addr::LOCALHOST);
        assert_eq!(found.port(), 59919, "应跳过被占端口递增");
        drop(listener);
    }

    #[test]
    fn find_available_addr_gives_up_after_max_attempts() {
        // 0 次尝试:即使端口空闲也直接放弃(测试 Err 路径)
        assert!(find_available_addr_from("127.0.0.1:59930", 0).is_err());
    }

    #[tokio::test]
    async fn spawn_injects_env_and_redirects_output() {
        let dir = std::env::temp_dir().join(format!("visp-acp-sup-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let log_path = dir.join("daemon-test.log");

        let mut child = spawn_daemon(
            Path::new("/bin/bash"),
            &["-c".into(), "echo VISP=$VISP_LISTEN_ADDR".into()],
            "127.0.0.1:59931",
            &log_path,
            None,
        )
        .await
        .unwrap();
        child.wait().await.unwrap();

        let log = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(
            log.contains("VISP=127.0.0.1:59931"),
            "env 未注入或输出未重定向, log: {log}"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn spawn_creates_missing_log_dir() {
        // 全新机器上 ~/.visp/logs 尚不存在:visp-acp 自拉起 daemon 时不能因此失败
        let root = std::env::temp_dir().join(format!("visp-acp-sup-mkdir-{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&root).await;
        let log_path = root.join("nested").join("logs").join("daemon-test.log");

        let mut child = spawn_daemon(
            Path::new("/bin/bash"),
            &["-c".into(), "echo started".into()],
            "127.0.0.1:59935",
            &log_path,
            None,
        )
        .await
        .expect("spawn_daemon should create the missing log directory");
        child.wait().await.unwrap();

        let log = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(
            log.contains("started"),
            "输出未重定向到新建目录下的日志, log: {log}"
        );
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn health_check_fails_fast_when_child_exits() {
        let dir = std::env::temp_dir().join(format!("visp-acp-sup2-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let log_path = dir.join("daemon-exit.log");
        // 立即退出的假 daemon(端口上无服务)
        let mut child = spawn_daemon(
            Path::new("/bin/bash"),
            &["-c".into(), "exit 1".into()],
            "127.0.0.1:59932",
            &log_path,
            None,
        )
        .await
        .unwrap();

        let start = std::time::Instant::now();
        let result =
            wait_for_health_ready(&mut child, "127.0.0.1:59932", Duration::from_secs(10)).await;
        let elapsed = start.elapsed();

        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("exited early"),
            "应因子进程退出快速失败, got: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "存活监控应立即失败而非等待 10s 超时, elapsed: {elapsed:?}"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn health_check_times_out_when_child_alive_but_no_service() {
        let dir = std::env::temp_dir().join(format!("visp-acp-sup3-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let log_path = dir.join("daemon-sleep.log");
        // 活着但不监听的假 daemon
        let mut child = spawn_daemon(
            Path::new("/bin/bash"),
            &["-c".into(), "sleep 30".into()],
            "127.0.0.1:59933",
            &log_path,
            None,
        )
        .await
        .unwrap();

        let result =
            wait_for_health_ready(&mut child, "127.0.0.1:59933", Duration::from_millis(700)).await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("did not become ready"),
            "无服务时应以超时失败"
        );
        let _ = child.kill().await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn shutdown_policy_matrix() {
        assert_eq!(
            shutdown_policy(LaunchMode::Spawned, false),
            ShutdownPolicy::SendThenKill
        );
        assert_eq!(
            shutdown_policy(LaunchMode::Spawned, true),
            ShutdownPolicy::SendThenKill
        );
        assert_eq!(
            shutdown_policy(LaunchMode::Attached, false),
            ShutdownPolicy::Leave
        );
        assert_eq!(
            shutdown_policy(LaunchMode::Attached, true),
            ShutdownPolicy::SendOnly
        );
    }

    // ===== A:用户可见日志位置与 daemon 实际写入位置对齐 =====

    #[tokio::test]
    async fn daemon_log_notice_reports_real_rolling_log_not_redirect_file() {
        let dir = std::env::temp_dir().join(format!("visp-acp-sup-log-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let redirect = dir.join("daemon-2026-01-01_00-00-00.log");

        // 假 daemon:确认 spawn_daemon 的重定向确实捕获子进程 stdout/stderr
        let mut child = spawn_daemon(
            Path::new("/bin/bash"),
            &["-c".into(), "echo early".into()],
            "127.0.0.1:59941",
            &redirect,
            None,
        )
        .await
        .unwrap();
        child.wait().await.unwrap();
        let captured = tokio::fs::read_to_string(&redirect).await.unwrap();
        assert!(
            captured.contains("early"),
            "重定向文件应捕获子进程早期输出, got: {captured}"
        );

        let notice = daemon_log_notice(&redirect);

        // 1) 报告的是 daemon 真实滚动日志的命名与目录(init.rs 写 {log_file}/visp-daemon.log.<date>)
        assert!(
            notice.contains("visp-daemon.log"),
            "应报告 daemon 真实滚动日志命名, got: {notice}"
        );
        if let Some(real_dir) = visp_config::path::log_dir() {
            assert!(
                notice.contains(&real_dir.display().to_string()),
                "应报告 daemon 真实日志目录, got: {notice}"
            );
        }

        // 2) 不再把重定向空文件谎称为日志(log= 提示)
        assert!(
            !notice.contains(&format!("log={}", redirect.display())),
            "不得再以 log= 指向只含早期输出的重定向文件, got: {notice}"
        );

        // 3) 明确标注重定向文件仅承接 stdout/stderr
        assert!(
            notice.contains(&redirect.display().to_string()),
            "应说明重定向捕获文件位置, got: {notice}"
        );
        assert!(
            notice.contains("stdout/stderr"),
            "应说明重定向文件仅用于早期 stdout/stderr, got: {notice}"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ===== B:终止信号收尾 harness(断言 Shutdown 被调用 + 子进程被回收) =====

    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tonic::{Request, Response, Status, Streaming};
    use visp_proto::visp::ClientMessage;
    use visp_proto::visp::ServerMessage;
    use visp_proto::visp::coder_daemon_server::{CoderDaemon, CoderDaemonServer};

    type StubChatStream =
        Pin<Box<dyn futures::Stream<Item = Result<ServerMessage, Status>> + Send>>;

    /// 仅记录 `Shutdown` 调用的最小 CoderDaemon stub。
    struct ShutdownStub {
        shutdowns: Arc<AtomicUsize>,
    }

    #[tonic::async_trait]
    impl CoderDaemon for ShutdownStub {
        type ChatStream = StubChatStream;

        async fn chat(
            &self,
            _request: Request<Streaming<ClientMessage>>,
        ) -> Result<Response<Self::ChatStream>, Status> {
            Err(Status::unimplemented("stub"))
        }
        async fn create_session(
            &self,
            _request: Request<visp_proto::visp::CreateSessionRequest>,
        ) -> Result<Response<visp_proto::visp::Session>, Status> {
            Err(Status::unimplemented("stub"))
        }
        async fn list_sessions(
            &self,
            _request: Request<()>,
        ) -> Result<Response<visp_proto::visp::ListSessionsResponse>, Status> {
            Err(Status::unimplemented("stub"))
        }
        async fn delete_session(
            &self,
            _request: Request<visp_proto::visp::DeleteSessionRequest>,
        ) -> Result<Response<()>, Status> {
            Err(Status::unimplemented("stub"))
        }
        async fn get_session(
            &self,
            _request: Request<visp_proto::visp::GetSessionRequest>,
        ) -> Result<Response<visp_proto::visp::Session>, Status> {
            Err(Status::unimplemented("stub"))
        }
        async fn read_file(
            &self,
            _request: Request<visp_proto::visp::ReadFileRequest>,
        ) -> Result<Response<visp_proto::visp::ReadFileResponse>, Status> {
            Err(Status::unimplemented("stub"))
        }
        async fn search_symbols(
            &self,
            _request: Request<visp_proto::visp::SearchSymbolsRequest>,
        ) -> Result<Response<visp_proto::visp::SearchSymbolsResponse>, Status> {
            Err(Status::unimplemented("stub"))
        }
        async fn get_symbol_details(
            &self,
            _request: Request<visp_proto::visp::GetSymbolDetailsRequest>,
        ) -> Result<Response<visp_proto::visp::SymbolDetails>, Status> {
            Err(Status::unimplemented("stub"))
        }
        async fn health_check(
            &self,
            _request: Request<()>,
        ) -> Result<Response<visp_proto::visp::HealthStatus>, Status> {
            Err(Status::unimplemented("stub"))
        }
        async fn shutdown(
            &self,
            _request: Request<ShutdownRequest>,
        ) -> Result<Response<()>, Status> {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Ok(Response::new(()))
        }
        async fn reload_config(
            &self,
            _request: Request<visp_proto::visp::ReloadConfigRequest>,
        ) -> Result<Response<visp_proto::visp::ReloadConfigResponse>, Status> {
            Err(Status::unimplemented("stub"))
        }
        async fn get_hook_stats(
            &self,
            _request: Request<visp_proto::visp::GetHookStatsRequest>,
        ) -> Result<Response<visp_proto::visp::HookStatsResponse>, Status> {
            Err(Status::unimplemented("stub"))
        }
    }

    async fn spawn_shutdown_stub() -> (String, Arc<AtomicUsize>) {
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let stub = ShutdownStub {
            shutdowns: shutdowns.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(CoderDaemonServer::new(stub))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        (addr.to_string(), shutdowns)
    }

    /// 自拉起模式的终止信号收尾 → 向 daemon 发 Shutdown 并回收子进程。
    #[tokio::test]
    async fn handle_termination_spawned_sends_shutdown_and_reaps_child() {
        let (addr, shutdowns) = spawn_shutdown_stub().await;
        // 假 daemon:短暂存活后自行退出 → 优雅窗口内 child.wait() 成功回收
        let mut child = Command::new("sleep").arg("0.3").spawn().unwrap();

        handle_termination(Some(&mut child), &addr, LaunchMode::Spawned, false)
            .await
            .unwrap();

        assert_eq!(
            shutdowns.load(Ordering::SeqCst),
            1,
            "自拉起模式收到终止信号必须向 daemon 发 Shutdown"
        );
        assert!(
            child.try_wait().unwrap().is_some(),
            "收尾后子进程应已被回收"
        );
    }

    /// 直连默认(Leave)不打扰既有 daemon:不发 Shutdown、不动子进程。
    #[tokio::test]
    async fn handle_termination_attached_default_leaves_daemon() {
        let (addr, shutdowns) = spawn_shutdown_stub().await;
        let mut child = Command::new("sleep").arg("0.3").spawn().unwrap();

        handle_termination(Some(&mut child), &addr, LaunchMode::Attached, false)
            .await
            .unwrap();

        assert_eq!(
            shutdowns.load(Ordering::SeqCst),
            0,
            "直连默认不应向既有 daemon 发 Shutdown"
        );
        let _ = kill(&mut child).await;
    }
}
