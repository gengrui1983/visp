#![allow(dead_code)]

/// 写入 /tmp/visp-tui-debug.log，用于诊断 textarea 折行/粘贴等问题。
/// 不在生产环境启用，无性能影响（为空时直接返回）。
#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {{
        #[cfg(debug_assertions)]
        {
            use std::io::Write;
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/visp-tui-debug.log")
                .and_then(|mut f| {
                    let ts = chrono::Local::now().format("%H:%M:%S%.3f");
                    writeln!(f, "[{ts}] {}", format!($($arg)*))
                });
        }
    }};
}

use crate::app::{AppState, ConfirmState, LineType, TabCompletionState};
use crate::client::{ChatHandle, VispClient};
use crate::connection::{
    ConnEvent, ConnState, IDLE_TIMEOUT, IdleClock, LiveConnection, ReconnectSuccess, Recovered,
    backoff_delay, probe_with_timeout, recover, transition,
};
use crate::notify::{NotifyEngine, NotifyKind};
use crate::ui::render;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use std::io::{self, Write};
use visp_proto::visp::{LlmConfig, reload_config_response, server_message};

/// 将一段文本插入到 textarea 中（模拟逐字输入）
/// 注意：`\n` 必须映射为 `Key::Enter`，否则 ratatui_textarea 会丢弃换行符前的内容
fn paste_text(textarea: &mut ratatui_textarea::TextArea<'static>, text: &str) {
    let has_cr = text.contains('\r');
    let _newline_count = text.chars().filter(|&c| c == '\n' || c == '\r').count();
    debug_log!(
        "paste: len={}, newlines={}, contains_cr={has_cr}",
        text.len(),
        _newline_count
    );
    // 统一换行符：\r\n → \n, \r → \n
    // 终端粘贴可能携带 \r\n(Windows) 或 \r(旧 Mac)，不处理会导致多余空行或光标回行首覆盖
    let text = if has_cr {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text.to_string()
    };
    for c in text.chars() {
        if c == '\n' {
            textarea.input(ratatui_textarea::Input {
                key: ratatui_textarea::Key::Enter,
                ctrl: false,
                alt: false,
                shift: false,
            });
        } else {
            textarea.input(ratatui_textarea::Input {
                key: ratatui_textarea::Key::Char(c),
                ctrl: false,
                alt: false,
                shift: false,
            });
        }
    }
}

/// Drop guard: 离开作用域时保证恢复终端状态
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        // 关闭 mouse mode 1000 + 1002 + 1006 和 bracketed paste mode 2004
        let _ = write!(io::stdout(), "\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?2004l");
        let _ = io::stdout().flush();
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    session_id: String,
    mut chat_handle: ChatHandle,
    model: String,
    model_key: String,
    mut client: VispClient,
    addr: String,
    project_path: &str,
    available_models: Vec<String>,
    model_keys: Vec<String>,
    notification: NotifyEngine,
) -> io::Result<()> {
    if let Ok((_w, _h)) = crossterm::terminal::size() {
        debug_log!("session start: {_w}x{_h}, model={model}");
    }
    crossterm::terminal::enable_raw_mode()?;
    // 启用 mouse mode 1000（按钮事件）+ 1002（按钮+拖拽）+ 1006（SGR 坐标编码）
    // 1002 使我们在 chat area 内可以收到 Drag 事件，实现文字选择
    write!(io::stdout(), "\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h")?;
    io::stdout().flush()?;
    let _guard = TerminalGuard;
    let mut terminal = ratatui::init();
    let mut app = AppState::new(
        session_id.clone(),
        model.clone(),
        model_key,
        project_path.to_string(),
    );
    app.available_models = available_models;
    app.model_keys = model_keys;
    app.notify = notification;

    // exit 信号：键盘线程检测到 Ctrl+D 时通知主循环无条件退出
    let (exit_tx, mut exit_rx) = tokio::sync::watch::channel(false);
    let thread_exit_tx = exit_tx.clone();

    // 键盘事件采集线程，同时负责 Ctrl+D 退出检测
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        loop {
            if key_tx.is_closed() {
                break;
            }
            if crossterm::event::poll(std::time::Duration::from_millis(100)).unwrap_or(false)
                && let Ok(event) = crossterm::event::read()
            {
                // 独立检测 Ctrl+D：无论任何状态，无条件触发退出
                if matches!(event, Event::Key(KeyEvent { code: KeyCode::Char('d'), modifiers, .. })
                    if modifiers.contains(KeyModifiers::CONTROL))
                {
                    let _ = thread_exit_tx.send(true);
                    break;
                }
                // MouseMoved 在源头过滤
                if matches!(event, Event::Mouse(ref m) if m.kind == MouseEventKind::Moved) {
                    continue;
                }
                if key_tx.send(event).is_err() {
                    break;
                }
            }
        }
    });
    // 主线程的 exit_tx 后续不再使用，drop 后 thread_exit_tx 是唯一 sender
    drop(exit_tx);

    let _ = terminal.draw(|f| render(&mut app, f));

    // Points spinner 动画 tick：generating 期间每 140ms 推进一帧
    let mut spinner_tick = tokio::time::interval(std::time::Duration::from_millis(140));

    // 重连通道：后台恢复任务把新 client/Chat 流送回主循环（设计 §5.9）。
    let (reconnect_tx, mut reconnect_rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<ReconnectSuccess, String>>();
    let mut reconnect_pending = false;
    let mut next_retry_at: Option<tokio::time::Instant> = None;

    // idle 看门狗：空闲到期发起独立 unary 探测（不按 generating 豁免，设计 §7 决策 14）。
    let (probe_tx, mut probe_rx) = tokio::sync::mpsc::unbounded_channel::<Result<bool, String>>();
    let mut probe_pending = false;
    let mut idle_clock = IdleClock::new(std::time::Instant::now());
    let mut idle_deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;

    loop {
        tokio::select! {
            event = key_rx.recv() => {
                match event {
                    Some(e) => { if handle_key_event(e, &mut app, &mut chat_handle) { break; } }
                    None => break,
                }
            }
            msg = chat_handle.recv(), if app.connection_state.is_connected() => {
                match msg {
                    Some(msg) => {
                        // 任何 Chat 流帧到达即重置 idle 计时（看门狗只在空闲连接上活动）。
                        idle_clock.on_frame(std::time::Instant::now());
                        idle_deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
                        handle_grpc_message(msg, &mut app, &chat_handle);
                    }
                    None => {
                        // 断线检测入口①：流干净关闭。不再退出，交状态机进入重连。
                        on_disconnect(&mut app, DisconnectReason::StreamClosed);
                        next_retry_at = Some(tokio::time::Instant::now() + backoff_delay(0));
                    }
                }
            }
            _ = exit_rx.changed() => {
                // Ctrl+D: 先发 Cancel 让 daemon 优雅停止，再断开连接
                chat_handle.send_cancel();
                break;
            }
            _ = spinner_tick.tick() => {
                // 仅 generating 期间推进 spinner 帧并请求重绘
                if app.generating() {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                    app.needs_render = true;
                }
            }
            Some(()) = app.image_ready_rx.recv() => {
                // 网络图片下载完成，重新渲染以显示图片
                app.needs_render = true;
            }
            // 断线检测入口②：idle 到期发起独立健康探测（后台执行，带 5s 超时）
            _ = tokio::time::sleep_until(idle_deadline),
                if app.connection_state.is_connected() && !probe_pending =>
            {
                probe_pending = true;
                let mut probe = client.probe_client();
                let tx = probe_tx.clone();
                tokio::spawn(async move {
                    let result = probe_with_timeout(async move {
                        probe
                            .health_check(())
                            .await
                            .map(|r| r.into_inner().alive)
                            .map_err(|e| format!("health probe: {e}"))
                    })
                    .await;
                    let _ = tx.send(result);
                });
            }
            // 探测结果：成功重置 idle 计时；失败/超时进入重连（单一探测、无宽限）
            Some(result) = probe_rx.recv(), if probe_pending => {
                probe_pending = false;
                let alive = matches!(result, Ok(true));
                match apply_probe_result(&mut app, alive) {
                    ProbeOutcome::Reset => {
                        idle_clock.reset(std::time::Instant::now());
                        idle_deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
                    }
                    ProbeOutcome::Reconnect => {
                        next_retry_at = Some(tokio::time::Instant::now() + backoff_delay(0));
                    }
                    ProbeOutcome::Ignored => {}
                }
            }
            // 退避到期：后台发起一次重连恢复（exit 通道保持活跃）
            _ = tokio::time::sleep_until(next_retry_at.unwrap_or_else(tokio::time::Instant::now)),
                if app.connection_state.is_reconnecting() && !reconnect_pending && next_retry_at.is_some() =>
            {
                reconnect_pending = true;
                next_retry_at = None;
                let tx = reconnect_tx.clone();
                let addr = addr.clone();
                let sid = app.main_session_id.clone();
                tokio::spawn(async move {
                    let mut conn = LiveConnection::new(addr, sid);
                    let result = match recover(&mut conn).await {
                        Ok(recovered) => {
                            let (client, chat) = conn.into_parts();
                            Ok(ReconnectSuccess { client, chat, recovered })
                        }
                        Err(e) => Err(e),
                    };
                    let _ = tx.send(result);
                });
            }
            // 重连结果：成功换用新 client/流并恢复；失败按退避继续重试（无限重试）
            Some(result) = reconnect_rx.recv(), if reconnect_pending => {
                reconnect_pending = false;
                match result {
                    Ok(success) => {
                        client = success.client;
                        chat_handle = success.chat;
                        let (state, _) = transition(app.connection_state, ConnEvent::Reconnected);
                        app.connection_state = state;
                        apply_recovery(&mut app, success.recovered);
                        // 重连成功后 idle 计时归零，进入下一周期
                        idle_clock.reset(std::time::Instant::now());
                        idle_deadline = tokio::time::Instant::now() + IDLE_TIMEOUT;
                    }
                    Err(e) => {
                        let (state, _) =
                            transition(app.connection_state, ConnEvent::ReconnectFailed);
                        let attempt = match state {
                            ConnState::Reconnecting { attempt } => attempt,
                            ConnState::Connected => 0,
                        };
                        app.connection_state = state;
                        app.add_message(
                            LineType::Status,
                            format!("重连失败（第 {attempt} 次）：{e}"),
                        );
                        next_retry_at =
                            Some(tokio::time::Instant::now() + backoff_delay(attempt));
                    }
                }
            }
        }
        if app.should_quit {
            break;
        }

        // 处理 /new 命令：创建新 session 并替换 chat_handle 的 session_id
        if app.pending_new_session {
            match client.create_session(project_path, None).await {
                Ok(session) => {
                    chat_handle.send_cancel();
                    chat_handle.session_id = session.session_id.clone();
                    let model = session.model.clone();
                    let model_key = session.model_key.clone();
                    app.reset_for_new_session(session.session_id, model, model_key);
                    app.add_message(
                        LineType::Status,
                        "New session started. Use /help for available commands.".into(),
                    );
                }
                Err(e) => {
                    app.add_message(
                        LineType::Error,
                        format!("Failed to create new session: {e}"),
                    );
                    app.pending_new_session = false;
                }
            }
        }

        // 处理 /list 命令：创建交互式 session 选择器
        if app.pending_list_sessions {
            match client.list_sessions().await {
                Ok(sessions) => {
                    if sessions.is_empty() {
                        app.add_message(LineType::Status, "No sessions found.".into());
                    } else {
                        let labels: Vec<String> = sessions
                            .iter()
                            .map(|s| {
                                let short_id: String = s.session_id.chars().take(8).collect();
                                let status_str = match s.status {
                                    0 => "IDLE",
                                    1 => "RUNNING",
                                    2 => "COMPLETED",
                                    3 => "ERROR",
                                    _ => "UNKNOWN",
                                };
                                let last_msg = s.last_user_message.as_str();
                                format!("  {short_id}  {status_str:>9}  {last_msg}")
                            })
                            .collect();
                        let session_ids: Vec<String> =
                            sessions.iter().map(|s| s.session_id.clone()).collect();
                        let mut state = ratatui::widgets::ListState::default();
                        state.select(Some(0));
                        app.session_select = Some(crate::app::SessionSelectState {
                            labels,
                            state,
                            session_ids,
                        });
                        app.needs_render = true;
                    }
                }
                Err(e) => {
                    app.add_message(LineType::Error, format!("Failed to list sessions: {e}"));
                }
            }
            app.pending_list_sessions = false;
        }

        // 处理 /model 命令：创建交互式模型选择器
        if app.pending_model_select {
            if !app.available_models.is_empty() {
                let display_labels = app.available_models.clone();
                let model_keys = if app.model_keys.is_empty() {
                    app.available_models.clone()
                } else {
                    app.model_keys.clone()
                };
                let mut state = ratatui::widgets::ListState::default();
                state.select(Some(0));
                app.model_select = Some(crate::app::ModelSelectState {
                    display_labels,
                    model_keys,
                    state,
                });
                app.needs_render = true;
            }
            app.pending_model_select = false;
        }

        // 处理 /sessions <id> 命令：切换到指定 session
        if let Some(ref target_id) = app.pending_switch_session.clone() {
            match client.get_session(target_id).await {
                Ok(session) => {
                    chat_handle.send_cancel();
                    let full_id = session.session_id.clone();
                    let model = session.model.clone();
                    let model_key = session.model_key.clone();
                    let _short: String = full_id.chars().take(8).collect();
                    chat_handle.session_id = full_id.clone();
                    app.reset_for_new_session(full_id, model, model_key);
                    app.pending_switch_session = None;
                    chat_handle.send_join();
                    app.needs_render = true;
                }
                Err(e) => {
                    app.add_message(LineType::Error, format!("Session not found: {e}"));
                    app.pending_switch_session = None;
                }
            }
        }

        // 处理 /reload 命令：调用 ReloadConfig unary RPC 并逐项渲染结果
        if app.pending_reload {
            let result = client.reload_config().await;
            apply_reload_results(&mut app, result);
        }

        // 当复制提示显示时，保持渲染以自动清除提示
        if app.last_copy_time.is_some() {
            app.needs_render = true;
        }

        if app.needs_render {
            // 确认状态始终需要渲染，不受流节流影响
            if app.generating() && app.confirm.is_none() && !app.try_begin_stream_render() {
                app.needs_render = false;
            }
            if app.needs_render {
                let _ = terminal.draw(|f| render(&mut app, f));
                app.needs_render = false;
            }
        }

        // draw() 完成后执行 OSC 52 复制（避免被 ratatui 输出覆盖）
        if let Some(text) = app.pending_copy_text.take() {
            crate::selection::osc52_copy(&text);
            app.last_copy_msg = Some(format!("Copied {} chars", text.chars().count()));
            app.last_copy_time = Some(std::time::Instant::now());
            app.needs_render = true; // 触发重绘显示 toast
        }
    }

    ratatui::restore();
    Ok(())
}

/// 将 `/reload` 的逐项结果渲染进当前激活 tab。
///
/// 成功条目用 Status 样式、失败条目用 Error 样式；RPC 失败（如 daemon
/// 不可达）只报一条错误、不重试——`/reload` 是用户显式动作，重试语义由
/// 用户决定（设计 §5.9）。无论成败都清除 `pending_reload` 标志。
fn apply_reload_results(
    app: &mut AppState,
    result: Result<Vec<reload_config_response::Item>, String>,
) {
    match result {
        Ok(items) => {
            for item in items {
                let line_type = if item.success {
                    LineType::Status
                } else {
                    LineType::Error
                };
                let text = format!("{}: {}", item.category, item.message);
                app.active_tab_mut().push_chat_line(line_type, text, None);
            }
        }
        Err(e) => {
            app.active_tab_mut().push_chat_line(
                LineType::Error,
                format!("Reload failed: {e}"),
                None,
            );
        }
    }
    app.pending_reload = false;
}

/// 断线检测入口（设计 §5.9）：两个入口（流干净关闭 / idle 探测失败）汇聚同一状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisconnectReason {
    /// 下行流干净关闭（`recv()` 返回 None）。
    StreamClosed,
    /// idle 心跳看门狗的探测失败/超时。
    WatchdogProbeFailed,
}

/// 看门狗探测结果的处置结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// 探测成功：重置 idle 计时，继续下一周期（不迁移状态）。
    Reset,
    /// 探测失败/超时：进入重连。
    Reconnect,
    /// 陈旧结果（已不在 Connected 态），忽略。
    Ignored,
}

/// 将一次健康探测结果交状态机判定（设计 §7 决策 14）。
///
/// 单一探测语义：成功即重置、失败即重连，**无宽限、无累计**。
fn apply_probe_result(app: &mut AppState, alive: bool) -> ProbeOutcome {
    if !app.connection_state.is_connected() {
        // 重连期间到达的陈旧探测结果不得触发任何迁移（负向防回潮）。
        return ProbeOutcome::Ignored;
    }
    if alive {
        let (state, _) = transition(app.connection_state, ConnEvent::ProbeSuccess);
        app.connection_state = state;
        ProbeOutcome::Reset
    } else {
        on_disconnect(app, DisconnectReason::WatchdogProbeFailed);
        ProbeOutcome::Reconnect
    }
}

/// 清理主 tab 的本地 streaming 残留（断线入口使用）。
///
/// 断线后不得残留上一轮 `streaming_text`/`generating`/`pending_usage` 与计时器；
/// `frames`/`rendered_up_to` 等 per-tab 不变量在重连恢复时由
/// [`AppState::cleanup_after_reconnect`] 统一复位。
fn clear_main_streaming_residue(app: &mut AppState) {
    let tab = &mut app.tab_bar.tabs[0];
    tab.streaming_text.clear();
    tab.generating = false;
    tab.stream_started_at = None;
    tab.stream_output_tokens = 0;
    tab.pending_usage = None;
    tab.last_stream_tps = None;
    tab.last_stream_elapsed = None;
    app.current_request_id = None;
    app.stale_done_expected = false;
}

/// 断线：统一迁移至 `Reconnecting` 并给出用户可见提示。
///
/// 不按 generating 状态豁免——断线检测在此已完成，统一进入重连。
fn on_disconnect(app: &mut AppState, reason: DisconnectReason) {
    let was_generating = app.tab_bar.tabs[0].generating;
    clear_main_streaming_residue(app);
    app.connection_state = ConnState::Reconnecting { attempt: 0 };
    if was_generating {
        // daemon 端 agent loop 不因 Chat 断开而取消，但 TUI 已无法接收后续帧——
        // 明确告知生成已终止，要求用户重新输入（设计 §5.9 决策 5）。
        app.add_message(LineType::Error, "连接中断，生成已终止".into());
    } else {
        let label = match reason {
            DisconnectReason::StreamClosed => "Daemon disconnected",
            DisconnectReason::WatchdogProbeFailed => "Daemon unresponsive",
        };
        app.add_message(LineType::Status, format!("{label} — reconnecting…"));
    }
    app.needs_render = true;
}

/// 重连恢复完成后的本地收尾：刷新模型列表、多 tab 恢复清理、迁回 `Connected`。
///
/// 模型列表为空（`get_session` 刷新失败，非致命）时保留旧列表，不覆盖为空白。
fn apply_recovery(app: &mut AppState, recovered: Recovered) {
    if !recovered.available_models.is_empty() {
        app.available_models = recovered.available_models;
        app.model_keys = recovered.model_keys;
    }
    // 丢弃 subagent tab、遍历复位 per-tab 状态并给出恢复提示（设计 §5.9）
    app.cleanup_after_reconnect();
    app.connection_state = ConnState::Connected;
    app.needs_render = true;
}

fn handle_key_event(event: Event, app: &mut AppState, chat_handle: &mut ChatHandle) -> bool {
    // 选择模式下：↑↓ 导航，Enter 确认，Esc/q 退出
    if let Some(ref mut ss) = app.session_select {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    ss.state.select_previous();
                    app.needs_render = true;
                    return false;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    ss.state.select_next();
                    app.needs_render = true;
                    return false;
                }
                KeyCode::Enter => {
                    if let Some(idx) = ss.state.selected()
                        && idx < ss.session_ids.len()
                    {
                        let target_id = ss.session_ids[idx].clone();
                        app.session_select = None;
                        app.clear_streaming();
                        app.set_generating(false);
                        app.stale_done_expected = false;
                        app.current_request_id = None;
                        app.confirm = None;
                        app.pending_switch_session = Some(target_id);
                        app.add_message(LineType::Status, "Switching session...".into());
                    }
                    app.needs_render = true;
                    return false;
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    app.session_select = None;
                    app.needs_render = true;
                    return false;
                }
                _ => {}
            }
        }
        // 选择模式下拦截所有其他按键
        return false;
    }

    // 模型选择模式下：↑↓ 导航，Enter 确认，Esc/q 退出
    if let Some(ref mut ms) = app.model_select {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    ms.state.select_previous();
                    app.needs_render = true;
                    return false;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    ms.state.select_next();
                    app.needs_render = true;
                    return false;
                }
                KeyCode::Enter => {
                    if let Some(ms) = app.model_select.take()
                        && let Some(idx) = ms.state.selected()
                        && idx < ms.model_keys.len()
                    {
                        let model_key = ms.model_keys[idx].clone();
                        let display_label = ms.display_labels[idx].clone();
                        app.model_key = model_key.clone();
                        chat_handle.send_config_update(LlmConfig {
                            model_key: Some(model_key),
                            model: None,
                            temperature: None,
                            max_tokens: None,
                            max_context_tokens: None,
                            extra: Default::default(),
                        });
                        app.add_message(
                            LineType::Status,
                            format!("Model set to {}", display_label),
                        );
                    }
                    app.needs_render = true;
                    return false;
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    app.model_select = None;
                    app.needs_render = true;
                    return false;
                }
                _ => {}
            }
        }
        // 模型选择模式下拦截所有其他按键
        return false;
    }

    app.needs_render = true;
    // ── 滚动条拖拽：优先处理，未命中滚动条的事件透传给下方逻辑 ──────
    if handle_scrollbar_mouse(&event, app) {
        return false;
    }
    match event {
        Event::Key(key) => {
            // F1: 切换帮助弹窗
            if key.code == KeyCode::F(1) {
                app.show_help = !app.show_help;
                return false;
            }

            // 帮助弹窗打开时，按任意键关闭（F1 在上面已经处理了切换）
            if app.show_help {
                app.show_help = false;
                return false;
            }

            // 文本选择模式：Esc 清除选择
            if app.text_selection.is_active() && key.code == KeyCode::Esc {
                app.text_selection.clear();
                app.needs_render = true;
                return false;
            }

            // Ctrl+C: 在任何模式下取消当前 LLM 推理（优先于所有其他按键处理）
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                if app.generating() {
                    // 如果确认框存在，先将其关闭（向 agent 回复一个拒绝响应）
                    if let Some(q) = app.confirm.take() {
                        chat_handle.send_response(&q.query_id, 1, "");
                    }
                    app.stale_done_expected = true;
                    app.flush_streaming();
                    app.clear_pending_usage();
                    app.current_request_id = None;
                    app.set_generating(false);
                    chat_handle.send_cancel();
                } else {
                    // Idle 状态：清空输入框
                    app.textarea = AppState::new_textarea();
                    app.tab_completion = None;
                    app.needs_render = true;
                }
                return false;
            }

            if app.confirm.is_some() {
                match key.code {
                    KeyCode::Left | KeyCode::Up => {
                        if let Some(ref mut confirm) = app.confirm {
                            let has_other = !confirm.options.is_empty();
                            let total = if confirm.options.is_empty() {
                                3 // Approve, Deny, Always Allow
                            } else {
                                confirm.options.len() + if has_other { 1 } else { 0 }
                            };
                            if confirm.other_active {
                                confirm.other_active = false;
                            } else if confirm.selected_index == 0 {
                                confirm.selected_index = total.saturating_sub(1);
                            } else {
                                confirm.selected_index -= 1;
                            }
                        }
                    }
                    KeyCode::Right | KeyCode::Down => {
                        if let Some(ref mut confirm) = app.confirm {
                            let has_other = !confirm.options.is_empty();
                            let total = if confirm.options.is_empty() {
                                3 // Approve, Deny, Always Allow
                            } else {
                                confirm.options.len() + if has_other { 1 } else { 0 }
                            };
                            if confirm.other_active {
                                confirm.other_active = false;
                            } else {
                                let next = confirm.selected_index + 1;
                                if next >= total {
                                    confirm.selected_index = 0;
                                } else {
                                    confirm.selected_index = next;
                                }
                            }
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(ref mut confirm) = app.confirm {
                            if confirm.other_active {
                                let q = app.confirm.take().unwrap();
                                let text = app.textarea.lines().join("\n");
                                app.textarea = AppState::new_textarea();
                                chat_handle.send_response(&q.query_id, -1, &text);
                            } else if !confirm.options.is_empty() {
                                let opts_len = confirm.options.len();
                                if confirm.selected_index == opts_len {
                                    confirm.other_active = true;
                                    return false;
                                }
                                let q = app.confirm.take().unwrap();
                                chat_handle.send_response(&q.query_id, q.selected_index as i32, "");
                            } else {
                                let q = app.confirm.take().unwrap();
                                chat_handle.send_response(&q.query_id, q.selected_index as i32, "");
                            }
                        }
                    }
                    KeyCode::Esc => {
                        if let Some(ref mut confirm) = app.confirm {
                            if confirm.other_active {
                                confirm.other_active = false;
                                app.textarea = AppState::new_textarea();
                            } else {
                                let q = app.confirm.take().unwrap();
                                chat_handle.send_response(&q.query_id, 1, "");
                                if app.generating() {
                                    app.stale_done_expected = true;
                                    app.flush_streaming();
                                    app.clear_pending_usage();
                                    app.current_request_id = None;
                                    app.set_generating(false);
                                    chat_handle.send_cancel();
                                }
                            }
                        }
                    }
                    _ => {
                        if let Some(ref confirm) = app.confirm {
                            if confirm.other_active {
                                app.textarea.input(build_input_from_key(key));
                            } else {
                                app.needs_render = false;
                            }
                        }
                    }
                }
                return false;
            }
            // Ctrl+C: 取消正在生成的请求
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                if app.generating() {
                    app.stale_done_expected = true;
                    app.clear_streaming();
                    app.clear_pending_usage();
                    app.current_request_id = None;
                    app.set_generating(false);
                    chat_handle.send_cancel();
                }
                return false;
            }
            // Alt+Shift+Right / Alt+Shift+Left: 翻页（边界停止）
            if key.code == KeyCode::Right
                && key.modifiers == (KeyModifiers::ALT | KeyModifiers::SHIFT)
            {
                app.tab_bar.next_page(app.tab_bar.last_term_width);
                return false;
            }
            if key.code == KeyCode::Left
                && key.modifiers == (KeyModifiers::ALT | KeyModifiers::SHIFT)
            {
                app.tab_bar.prev_page();
                return false;
            }

            // Alt+. / Alt+,: 循环切换 tab（精确匹配 ALT，排除 Alt+Shift）
            // 不使用 Alt+[/] 因为 ESC+[ 是 ANSI CSI 前缀，crossterm 需要等待
            // 后续字节判断是否为转义序列，会造成数十毫秒的卡顿
            if key.code == KeyCode::Char('.') && key.modifiers == KeyModifiers::ALT {
                app.tab_bar.activate_next();
                app.text_selection.clear();
                app.scroll_following = true;
                return false;
            }
            if key.code == KeyCode::Char(',') && key.modifiers == KeyModifiers::ALT {
                app.tab_bar.activate_prev();
                app.text_selection.clear();
                app.scroll_following = true;
                return false;
            }

            // Ctrl+W: 关闭当前 sub-agent tab（Running 状态也可关闭，不打断 agent 工作）
            if (key.code == KeyCode::Char('w') || key.code == KeyCode::Char('W'))
                && key.modifiers == KeyModifiers::CONTROL
            {
                if app.tab_bar.close_active() {
                    app.needs_render = true;
                }
                return false;
            }

            // Sub-agent tab: 禁止键盘输入（仅 default tab 可输入；保证全局快捷键已在前方处理完毕）
            if app.tab_bar.active != 0 {
                // ViewOnly tab: ↑↓ browse input_history, Enter shows hint, others blocked
                if app.active_tab().status == crate::app::AgentStatus::ViewOnly {
                    match key.code {
                        KeyCode::Up => {
                            if !app.input_history.is_empty() {
                                let idx = app
                                    .history_index
                                    .map_or(app.input_history.len().saturating_sub(1), |i| {
                                        i.saturating_sub(1)
                                    });
                                app.history_index = Some(idx);
                                app.textarea = crate::app::AppState::new_textarea();
                                app.textarea.insert_str(&app.input_history[idx]);
                            }
                            return false;
                        }
                        KeyCode::Down => {
                            if let Some(idx) = app.history_index {
                                let ni = idx + 1;
                                if ni >= app.input_history.len() {
                                    app.history_index = None;
                                    app.textarea = crate::app::AppState::new_textarea();
                                } else {
                                    app.history_index = Some(ni);
                                    app.textarea = crate::app::AppState::new_textarea();
                                    app.textarea.insert_str(&app.input_history[ni]);
                                }
                            }
                            return false;
                        }
                        KeyCode::Enter => {
                            app.add_message(
                                crate::app::LineType::Status,
                                "此 tab 为只读历史，无法输入".into(),
                            );
                            return false;
                        }
                        _ => return false,
                    }
                }
                return false;
            }

            // F2 已在键盘线程处理，此处不再需要
            if app.generating() {
                return false;
            }
            // 重连期间：Enter 提交提示未连接，不发送不清空（设计 §5.9）。
            if !app.connection_state.is_connected() && key.code == KeyCode::Enter {
                app.add_message(LineType::Error, "未连接到 daemon，稍后自动重连".into());
                app.needs_render = true;
                return false;
            }
            if key.code == KeyCode::Enter {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    app.textarea.insert_newline();
                    return false;
                }
                let text: String = app.textarea.lines().join("\n");
                app.textarea = AppState::new_textarea();
                app.tab_completion = None;
                if text.trim().is_empty() {
                    return false;
                }
                if text.starts_with('/') {
                    crate::command::handle(&text, app, chat_handle);
                } else {
                    // 新用户输入清零 L2（上一轮 request 的 token 统计）
                    app.current_request_usage = (0, 0, 0, 0);
                    let processed = crate::image::parse_image_refs(&text, &app.project_path);
                    app.add_message(LineType::User, processed.clone());
                    app.input_history.push(text.clone());
                    app.history_index = None;
                    app.set_generating(true);
                    app.scroll_following = true;
                    let rid = chat_handle.send_input(&processed);
                    app.current_request_id = Some(rid);
                }
                return false;
            }
            match key.code {
                KeyCode::Tab => {
                    let current = app
                        .textarea
                        .lines()
                        .first()
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    if current.starts_with('/') {
                        let cmds = [
                            "/clear",
                            "/help",
                            "/init",
                            "/init-agent ",
                            "/init-skill ",
                            "/list",
                            "/model ",
                            "/reload",
                            "/sessions ",
                            "/temp ",
                        ];

                        // 如果已有补全状态且当前文本是上次补全结果之一，循环到下一个
                        if let Some(tc) = &mut app.tab_completion
                            && tc.matches.contains(&current)
                            && current.starts_with(&tc.prefix)
                        {
                            tc.index = (tc.index + 1) % tc.matches.len();
                            let next = tc.matches[tc.index].clone();
                            app.textarea = AppState::new_textarea();
                            app.textarea.insert_str(&next);
                            return false;
                        }

                        // 新一轮补全：以当前文本为前缀，查找所有匹配
                        let prefix = current.trim_end().to_string();
                        let matches: Vec<String> = if prefix.len() > 1 {
                            cmds.iter()
                                .filter(|c| c.starts_with(prefix.as_str()))
                                .map(|c| c.to_string())
                                .collect()
                        } else {
                            // 仅 "/"：列出全部命令
                            cmds.iter().map(|c| c.to_string()).collect()
                        };

                        if matches.is_empty() {
                            app.tab_completion = None;
                        } else {
                            app.tab_completion = Some(TabCompletionState {
                                prefix,
                                matches: matches.clone(),
                                index: 0,
                            });
                            app.textarea = AppState::new_textarea();
                            app.textarea.insert_str(&matches[0]);
                        }
                    }
                    return false;
                }
                KeyCode::Up => {
                    app.tab_completion = None;
                    if !app.input_history.is_empty() {
                        let idx = app
                            .history_index
                            .map_or(app.input_history.len().saturating_sub(1), |i| {
                                i.saturating_sub(1)
                            });
                        app.history_index = Some(idx);
                        app.textarea = AppState::new_textarea();
                        app.textarea.insert_str(&app.input_history[idx]);
                    }
                }
                KeyCode::Down => {
                    app.tab_completion = None;
                    if let Some(idx) = app.history_index {
                        let ni = idx + 1;
                        if ni >= app.input_history.len() {
                            app.history_index = None;
                            app.textarea = AppState::new_textarea();
                        } else {
                            app.history_index = Some(ni);
                            app.textarea = AppState::new_textarea();
                            app.textarea.insert_str(&app.input_history[ni]);
                        }
                    }
                }
                KeyCode::PageUp => {
                    app.scroll_state.y = app.scroll_state.y.saturating_sub(10);
                    app.scroll_following = false;
                }
                KeyCode::PageDown => {
                    app.scroll_state.y = app.scroll_state.y.saturating_add(10);
                }
                _ => {
                    let input = build_input_from_key(key);
                    debug_log!(
                        "key -> textarea: {:?} (lines={})",
                        input.key,
                        app.textarea.lines().len()
                    );
                    // 普通按键输入时重置 Tab 补全状态
                    app.tab_completion = None;
                    app.textarea.input(input);
                }
            }
        }
        // 帮助弹窗打开时，鼠标点击关闭
        Event::Mouse(m)
            if m.kind == MouseEventKind::Down(crossterm::event::MouseButton::Left)
                && app.show_help =>
        {
            app.show_help = false;
            return false;
        }
        // 鼠标拖拽：更新选择范围
        Event::Mouse(m)
            if m.kind == MouseEventKind::Drag(crossterm::event::MouseButton::Left)
                && app.text_selection.start.is_some()
                && !app.show_help =>
        {
            // 屏幕坐标 -> 内容坐标（加 scroll 偏移）
            app.text_selection.end = Some((m.column, m.row + app.scroll_state.y));
            app.needs_render = true;
            return false;
        }
        // 鼠标左键释放：如果有选择范围，通过 OSC 52 自动复制到剪贴板
        Event::Mouse(m)
            if m.kind == MouseEventKind::Up(crossterm::event::MouseButton::Left)
                && app.text_selection.start.is_some() =>
        {
            // 纯点击（start==end）：清除选择，不复制
            if app.text_selection.start == app.text_selection.end {
                app.text_selection.clear();
                app.needs_render = true;
            } else {
                // 拖拽结束：标记待复制（渲染时从 buffer 提取文本）
                app.pending_copy = true;
                app.needs_render = true;
            }
            return false;
        }
        // 鼠标左键按下：清除旧选择，记录起点（点击不高亮，拖拽才高亮）
        Event::Mouse(m) if m.kind == MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            // 先检查是否在 chat area 内（用于文本选择）
            let (cx, cy, cw, ch) = app.chat_area_rect;
            let in_chat = cw > 0
                && ch > 0
                && m.column >= cx
                && m.column < cx + cw
                && m.row >= cy
                && m.row < cy + ch;

            if in_chat && !app.show_help {
                let virtual_row = m.row.saturating_sub(cy) + app.scroll_state.y;

                // 确保缓存是最新的（渲染可能被流节流跳过，导致缓存过时）。
                // 若自上次渲染以来布局发生变化（新消息、缓存重建、宽度变化），
                // 屏幕显示的还是旧布局，本次点击坐标已不可信：
                // 先强制渲染一帧对齐屏幕与缓存，吞掉此次点击，
                // 用户下一次点击即可命中真实布局。
                let content_w = app.cache_width;
                if content_w > 0 && crate::ui::ensure_all_caches(app, content_w) {
                    app.needs_render = true;
                    return false;
                }

                // 子 tab 的 task_prompt 占据渲染最顶部的若干行，
                // hit test 需要扣除这部分高度偏移
                let prompt_h: u16 = app
                    .active_tab()
                    .task_prompt
                    .as_ref()
                    .map(|p| {
                        let text = format!("📋 {}", p);
                        let lc = crate::app::wrap_text(&text, content_w).len() as u16;
                        crate::theme::USER_STYLE.total_height(lc)
                    })
                    .unwrap_or(0);
                let virtual_row = virtual_row.saturating_sub(prompt_h);

                // 先检查是否点击了 AgentCall 块的 "[show in new tab]" 按钮
                let rel_col = m.column.saturating_sub(cx);
                if let Some(sub_sid) = crate::tool_ui::agent_open_tab_hit_test(
                    app.messages(),
                    &app.message_caches,
                    virtual_row,
                    rel_col,
                    content_w,
                ) {
                    // 切换到或恢复子 agent 的 tab
                    if let Some(idx) = app.tab_bar.find_or_restore_tab(&sub_sid) {
                        app.tab_bar.activate(idx);
                        app.scroll_following = true;
                        app.needs_render = true;
                    }
                    return false;
                }

                // 检查是否点击在工具调用块 header（tool_name/args、agent_name/prompt 第一行，
                // 切换展开/折叠）
                if let Some(call_id) = crate::tool_ui::tool_block_hit_test(
                    app.messages(),
                    &app.message_caches,
                    virtual_row,
                    content_w,
                ) {
                    app.toggle_tool_call_expansion(&call_id);
                    app.scroll_following = false;
                    app.needs_render = true;
                    return false;
                }

                // Double-click on image address lines to copy URL/path
                let now = std::time::Instant::now();
                let is_double_click = app
                    .last_mouse_click
                    .is_some_and(|t| now.duration_since(t).as_millis() < 400);
                app.last_mouse_click = Some(now);

                if is_double_click
                    && let Some(text) = crate::image::image_address_hit_test(
                        app.messages(),
                        &app.message_caches,
                        virtual_row,
                        content_w,
                    )
                {
                    crate::selection::osc52_copy(&text);
                    app.last_copy_msg = Some(format!("已复制: {}", text));
                    app.last_copy_time = Some(now);
                    app.needs_render = true;
                    return false;
                }

                // 没有点击在工具调用头部：记录起点（内容坐标），清除旧选择
                let content_row = m.row + app.scroll_state.y;
                app.text_selection.clear();
                app.text_selection.start = Some((m.column, content_row));
                app.text_selection.end = Some((m.column, content_row));
                app.needs_render = true;
                return false;
            }

            // 不在 chat area 内：清除选择，走原有逻辑
            app.text_selection.clear();

            // 点击状态栏第 1 行的 session_id：复制完整会话 ID 到系统剪贴板。
            // 复制提示（toast）显示期间第 1 行左侧不是 session_id，不响应。
            if app.last_copy_msg.is_none()
                && let Some((sx, sy)) = app.status_bar_first_row
                && m.row == sy
                && m.column >= sx
                && m.column < sx + app.session_id.chars().take(8).count() as u16
            {
                crate::selection::osc52_copy(&app.session_id);
                let now = std::time::Instant::now();
                app.last_copy_msg = Some(format!("已复制: {}", app.session_id));
                app.last_copy_time = Some(now);
                app.needs_render = true;
                return false;
            }

            // 优先：点击子 tab 的 ✕ 关闭按钮
            if let Some(idx) = crate::ui::tab_close_at_screen(&app.tab_bar, m.column, m.row) {
                if app.tab_bar.close_tab(idx) {
                    app.needs_render = true;
                }
                return false;
            }

            // tab bar 点击切换 tab
            if let Some(idx) = crate::ui::tab_at_screen(&app.tab_bar, m.column, m.row) {
                if idx != app.tab_bar.active {
                    app.tab_bar.activate(idx);
                    app.scroll_following = true;
                    app.needs_render = true;
                }
                return false;
            }
        }
        Event::Mouse(m) => match m.kind {
            MouseEventKind::ScrollUp => {
                if app.try_begin_scroll() {
                    app.scroll_state.scroll_up();
                    app.scroll_state.scroll_up();
                    app.scroll_state.scroll_up();
                    app.scroll_following = false;
                } else {
                    app.needs_render = false;
                }
            }
            MouseEventKind::ScrollDown => {
                if app.try_begin_scroll() {
                    app.scroll_state.scroll_down();
                    app.scroll_state.scroll_down();
                    app.scroll_state.scroll_down();
                } else {
                    app.needs_render = false;
                }
            }
            _ => {}
        },
        // 处理终端粘贴事件（bracketed paste）
        Event::Paste(text)
            if app.tab_bar.active == 0 && app.confirm.as_ref().is_none_or(|c| c.other_active) =>
        {
            debug_log!("paste event received: len={}", text.len());
            paste_text(&mut app.textarea, &text);
        }
        _ => {}
    }
    false
}

/// Done 通知的固定文案（BEL 编码忽略 body，仅作语义占位；OSC 编码时使用）。
const DONE_BODY: &str = "任务完成，等待输入";

fn handle_grpc_message(
    msg: visp_proto::visp::ServerMessage,
    app: &mut AppState,
    chat_handle: &ChatHandle,
) {
    app.needs_render = true;

    // Phase 1: 控制流副作用（只保留非渲染逻辑）
    match &msg.payload {
        Some(server_message::Payload::UsageInfo(ui)) => {
            app.apply_usage_info(
                &ui.session_id,
                ui.input_tokens,
                ui.output_tokens,
                ui.tool_calls,
                ui.cache_creation_input_tokens,
                ui.cache_read_input_tokens,
            );
            app.apply_usage_cost(&ui.session_id, ui.cost);
        }
        Some(server_message::Payload::UsageDelta(d)) => {
            // 逐事件增量：仅供实时速率展示，不参与结算累加
            app.apply_usage_delta(&d.session_id, d.output_tokens);
        }
        Some(server_message::Payload::UserQuery(uq)) => {
            // 将已积累的 streaming text 刷入消息列表，使其走 markdown 渲染路径
            app.flush_streaming();
            app.confirm = Some(ConfirmState {
                query_id: uq.query_id.clone(),
                message: uq.message.clone(),
                options: uq.options.clone(),
                selected_index: 0,
                other_active: false,
            });
            // 终端通知：仅主 session 响铃（BEL；TUI 持有终端，只写控制字节）。
            // 会话过滤在挂接点完成——notify 模块不感知 session。
            let is_main = uq.session_id.is_empty() || uq.session_id == app.main_session_id;
            if is_main
                && let Some(bytes) = app.notify.on_event(
                    NotifyKind::UserQuery,
                    &uq.message,
                    std::time::Instant::now(),
                )
            {
                crate::notify::write_to_stdout(&bytes);
            }
        }
        Some(server_message::Payload::StatusUpdate(su)) => {
            // 加载 session 历史中的用户输入到 input_history（↑↓ 翻找历史提问）
            if !su.user_inputs.is_empty() {
                for input in &su.user_inputs {
                    if !app.input_history.contains(input) {
                        app.input_history.push(input.clone());
                    }
                }
            }
            app.needs_render = true;
        }
        Some(server_message::Payload::Error(e)) => {
            let is_main = e.session_id.is_empty() || e.session_id == app.main_session_id;

            // stale_done_expected 仅与主 session 的 Cancel 相关：
            // Cancel 会触发 daemon 回发 Error，需跳过以避免误显示
            if is_main && app.stale_done_expected {
                app.stale_done_expected = false;
                return;
            }

            // 忙拒绝（SessionBusy）不代表本轮失败：不提前释放输入门禁、
            // 不清 current_request_id，让该帧继续走到 route_frame，由既有特判
            // 渲染成一行 Status 提示（否则会丢失时钟并绕过门禁保护）
            if e.code != "SessionBusy" {
                // 按 session_id 定位 tab 并设置 generating = false
                let idx = if is_main {
                    Some(0)
                } else {
                    app.tab_bar.find_index_by_session(&e.session_id)
                };
                if let Some(idx) = idx {
                    app.tab_bar.tabs[idx].stop_generating();
                    // current_request_id 仅与主 session 相关
                    if is_main {
                        app.current_request_id = None;
                    }
                }
            }
            // 未知 session 的 Error 会 fall through 到 route_frame 创建 tab 并处理
        }
        Some(server_message::Payload::Done(d)) => {
            let is_main = d.session_id.is_empty() || d.session_id == app.main_session_id;

            // stale_done_expected 仅与主 session 的 Cancel 相关
            if is_main && app.stale_done_expected {
                app.stale_done_expected = false;
                return;
            }

            // 终端通知：仅主 session 响铃（BEL）。必须位于 stale 守卫之后——
            // Ctrl+C 造成的 stale Done 不应响铃。会话过滤在挂接点完成，
            // notify 模块不感知 session。
            if is_main
                && let Some(bytes) =
                    app.notify
                        .on_event(NotifyKind::Done, DONE_BODY, std::time::Instant::now())
            {
                crate::notify::write_to_stdout(&bytes);
            }

            // 按 session_id 定位 tab 并设置 generating = false
            let idx = if is_main {
                Some(0)
            } else {
                app.tab_bar.find_index_by_session(&d.session_id)
            };
            if let Some(idx) = idx {
                app.tab_bar.tabs[idx].stop_generating();
                app.apply_done_token_settlement(&d.session_id);
                // current_request_id + ack 仅与主 session 相关
                if is_main && let Some(rid) = app.current_request_id.take() {
                    chat_handle.send_ack(&rid);
                }
            }
            // 未知 session 的 Done 会 fall through 到 route_frame 创建 tab 并处理
        }
        _ => {}
    }

    // Phase 2: 路由 frame 到正确 tab 进行渲染
    app.route_frame(msg);
}

/// 格式化工具调用参数显示
fn tc_display(tc: &visp_proto::visp::ToolCall) -> String {
    match serde_json::from_str::<serde_json::Value>(&tc.arguments) {
        Ok(serde_json::Value::Object(obj)) => {
            let vals: Vec<String> = obj
                .values()
                .filter_map(|v| v.as_str().map(|s| format!("\"{}\"", s)))
                .collect();
            if vals.is_empty() {
                format!("{} {}", tc.tool_name, tc.arguments)
            } else {
                format!("{}: {}", tc.tool_name, vals.join(" "))
            }
        }
        _ => format!("{}: {}", tc.tool_name, tc.arguments),
    }
}

// ════════════════════════════════════════════════════════════════
// 滚动条拖拽
// ════════════════════════════════════════════════════════════════

/// 滚动条鼠标处理（抓取滑块 / 拖拽 / 点击轨道跳转 / 释放）。
/// 返回 true 表示事件已消费；未命中滚动条返回 false 透传给常规鼠标逻辑。
fn handle_scrollbar_mouse(event: &Event, app: &mut AppState) -> bool {
    let Event::Mouse(m) = event else {
        return false;
    };
    // 拖拽中：指针行映射为滚动偏移；左键抬起结束拖拽
    if app.scrollbar_drag_offset.is_some() {
        match m.kind {
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                if let (Some(geo), Some(offset)) = (app.scrollbar_geo, app.scrollbar_drag_offset) {
                    let track = geo.area.height;
                    // 目标滑块顶部偏移（纵向跟随指针，允许拖出滚动条列）
                    let target = (m.row as i32 - geo.area.y as i32 - offset as i32)
                        .clamp(0, track.saturating_sub(geo.thumb_len) as i32)
                        as u16;
                    app.scroll_state.y = crate::app::scrollbar_scroll_from_thumb(
                        target,
                        track,
                        geo.area.height,
                        geo.max_scroll,
                    );
                    app.scroll_following = false;
                } else {
                    // 内容变矮（滚动条消失）时结束拖拽
                    app.scrollbar_drag_offset = None;
                }
                return true;
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                app.scrollbar_drag_offset = None;
                return true;
            }
            _ => {}
        }
    }
    // 左键按下：命中滚动条轨道 → 抓滑块或跳转
    if m.kind == MouseEventKind::Down(crossterm::event::MouseButton::Left)
        && let Some(geo) = app.scrollbar_geo
    {
        let in_track =
            m.column == geo.area.x && m.row >= geo.area.y && m.row < geo.area.y + geo.area.height;
        if in_track {
            let track = geo.area.height;
            let row = m.row - geo.area.y;
            if row >= geo.thumb_start && row < geo.thumb_start.saturating_add(geo.thumb_len) {
                // 抓住滑块：记录指针相对滑块顶部的偏移，保持相对位置不跳变
                app.scrollbar_drag_offset = Some(row - geo.thumb_start);
            } else {
                // 点击轨道空白：滑块中心跳到点击行，并立即进入拖拽
                app.scrollbar_drag_offset = Some(geo.thumb_len / 2);
                let target = row
                    .saturating_sub(geo.thumb_len / 2)
                    .min(track.saturating_sub(geo.thumb_len));
                app.scroll_state.y = crate::app::scrollbar_scroll_from_thumb(
                    target,
                    track,
                    geo.area.height,
                    geo.max_scroll,
                );
                app.scroll_following = false;
            }
            return true;
        }
    }
    false
}

fn build_input_from_key(key: KeyEvent) -> ratatui_textarea::Input {
    ratatui_textarea::Input {
        key: match key.code {
            KeyCode::Char(c) => ratatui_textarea::Key::Char(c),
            KeyCode::Enter => ratatui_textarea::Key::Enter,
            KeyCode::Backspace => ratatui_textarea::Key::Backspace,
            KeyCode::Delete => ratatui_textarea::Key::Delete,
            KeyCode::Left => ratatui_textarea::Key::Left,
            KeyCode::Right => ratatui_textarea::Key::Right,
            KeyCode::Up => ratatui_textarea::Key::Up,
            KeyCode::Down => ratatui_textarea::Key::Down,
            KeyCode::Tab => ratatui_textarea::Key::Tab,
            KeyCode::Esc => ratatui_textarea::Key::Esc,
            KeyCode::Home => ratatui_textarea::Key::Home,
            KeyCode::End => ratatui_textarea::Key::End,
            _ => ratatui_textarea::Key::Char(' '),
        },
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT),
    }
}

#[cfg(test)]
#[path = "event_tests.rs"]
mod tests;
