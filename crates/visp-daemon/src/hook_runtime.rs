//! hook 运行时：规则适配 + 信任门控 + 分发 + 有界 drain
//! （设计 §5 D4 执行语义 / §7.2–§7.3 规则与作用域 / §5 D13 关停 drain）。
//!
//! 本模块把配置层（[`HookRule`]）与执行内核（[`visp_hooks::Executor`]）接线起来：
//!
//! - **规则适配**：`HookRule` → [`DispatchRule`]（匹配/排序/队列）与 `HookRule` → [`ProcessSpec`]
//!   （进程执行规格）；`cwd` 中的字面量 `project` 解析为会话/项目路径。
//! - **信任门控**（设计 §7.3 / §8.2）：全局规则全部进入执行集；项目规则**仅**在
//!   [`verify`] `== Trusted` 时进入，未信任则整体跳过。
//! - **分发**：`BusEvent::Hook` 直接转 [`DispatchInput`]；`BusEvent::Frame` 经
//!   [`crate::hook_map`] 映射，未映射的帧忽略。总线 `seq` 回填到事件信封。
//! - **关停 drain**（设计 D13）：实现 [`HookDrainHost`]，在 budget 内停止接收新工作并
//!   尽力排空执行队列。
//!
//! [`HookDrainHost`] 原定义于 daemon 的优雅关停管道；为让执行器在**库层**即可实现该
//! 契约（bin 无法被库反向依赖），契约定义迁入本模块，并由 `shutdown` 重新导出以保持
//! 既有引用（`service.rs`/`main.rs`）不变。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::broadcast;

use visp_config::hooks::{HookRule, HookScope, HooksConfig, OnFull};
use visp_config::trust::{HookTrustStore, TrustStatus, verify};
use visp_core::bus::BusEvent;
use visp_hooks::{
    DispatchInput, DispatchRule, Executor, Handler, HookEvent, HookPayload, Origin, PermissionKind,
    ProcessSpec, QueuePolicy, SessionSource, SpawnHandler,
};

use crate::bus::EventBus;
use crate::hook_map::{MapCtx, map_frame};

/// 关停期的 hook drain 宿主（设计 D13）。
///
/// 契约：`drain` 必须在 `budget` 内返回；真实执行器停止接收新 hook 工作、等待在飞
/// 执行完成后返回。调用方另有同长硬超时兜底，宿主即使超时也不会拖住关停。
#[async_trait]
#[allow(clippy::double_must_use)] // async_trait 展开的 Pin<Box<dyn Future>> 触发误报
pub trait HookDrainHost: Send + Sync {
    /// 在 `budget` 内排空在飞的 hook 执行。
    async fn drain(&self, budget: Duration);

    /// 关停期**同步直投**终态事件（如 `SessionEnd`）。
    ///
    /// 与总线投递不同，本方法不经消费者任务、不受 `accepting` 门控限制：即使已在
    /// [`HookDrainHost::drain`] 之后调用，终态事件仍会入队执行，杜绝「终态 hook 因
    /// 总线/关停竞态被丢弃」。
    async fn emit_terminal(&self, event: HookEvent);
}

/// 生效规则摘要：`id` / 作用域 / 启用状态（设计 §15 `GetHookStats` 规则列表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSummary {
    /// 规则唯一名。
    pub id: String,
    /// 规则来源作用域。
    pub scope: HookScope,
    /// 是否启用。
    pub enabled: bool,
}

impl RuleSummary {
    /// 作用域的 proto 拼写（与 `HookScope` serde `lowercase` 一致）。
    pub fn scope_str(&self) -> &'static str {
        match self.scope {
            HookScope::Global => "global",
            HookScope::Project => "project",
        }
    }
}

/// hook 运行时只读计数 + 生效规则摘要（设计 §15：五类计数）。
///
/// `emitted`/`dropped` 由 [`HookRuntime`] 计数；`executed`/`failed`/`timed_out`
/// 取自执行器（默认 [`SpawnHandler`] 的 `HookStats`）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HookStatsSnapshot {
    /// 进入运行时的 hook 事件数。
    pub emitted: u64,
    /// 运行时丢弃的事件数（关停后到达 / 总线消费者滞后）。
    pub dropped: u64,
    /// 进程正常退出（exit code 0）次数。
    pub executed: u64,
    /// 启动后失败次数（非零退出 / 被信号终止 / 等待出错）。
    pub failed: u64,
    /// 超时被杀次数。
    pub timed_out: u64,
    /// 生效规则摘要（按执行集顺序：全局在前）。
    pub rules: Vec<RuleSummary>,
}

/// 进程执行计数来源（设计 §15：`executed`/`failed`/`timed_out`）。
///
/// 默认执行器 [`SpawnHandler`] 自带 `HookStats`；注入自定义 [`Handler`] 时缺省为
/// [`NoopExecCounters`]（恒零），测试可注入其它实现。
pub trait ExecCounters: Send + Sync {
    /// 正常退出次数。
    fn executed(&self) -> u64;
    /// 失败次数。
    fn failed(&self) -> u64;
    /// 超时次数。
    fn timed_out(&self) -> u64;
}

impl ExecCounters for SpawnHandler {
    fn executed(&self) -> u64 {
        self.stats().executed()
    }
    fn failed(&self) -> u64 {
        self.stats().failed()
    }
    fn timed_out(&self) -> u64 {
        self.stats().timed_out()
    }
}

/// 恒零执行计数（注入 handler 无进程计数时的兜底）。
#[derive(Debug, Default)]
pub struct NoopExecCounters;

impl ExecCounters for NoopExecCounters {
    fn executed(&self) -> u64 {
        0
    }
    fn failed(&self) -> u64 {
        0
    }
    fn timed_out(&self) -> u64 {
        0
    }
}

/// 只读 hook 统计来源（`GetHookStats` RPC 的数据面）。
///
/// 实现必须**只读**且线程安全：读取不得触发派发/执行等副作用。
pub trait HookStatsSource: Send + Sync {
    /// 读取当前计数与生效规则摘要。
    fn hook_stats(&self) -> HookStatsSnapshot;
}

/// 无 hook 运行时的只读统计实现（全零计数 + 空规则表）。
#[derive(Debug, Default)]
pub struct NoopHookStats;

impl HookStatsSource for NoopHookStats {
    fn hook_stats(&self) -> HookStatsSnapshot {
        HookStatsSnapshot::default()
    }
}

/// 以会话/项目路径构造默认映射上下文（`cwd=project_path`、`source=Startup`、`origin=Tui`）。
pub fn session_ctx(project_path: impl Into<String>) -> MapCtx {
    let project_path = project_path.into();
    MapCtx {
        cwd: project_path.clone(),
        source: SessionSource::Startup,
        origin: Origin::Tui,
        project_path,
    }
}

/// 规则队列溢出策略适配：`OnFull` → [`QueuePolicy`]（同义枚举，crate 边界单向映射）。
pub fn queue_policy(on_full: OnFull) -> QueuePolicy {
    match on_full {
        OnFull::DropNew => QueuePolicy::DropNew,
        OnFull::DropOld => QueuePolicy::DropOld,
        OnFull::CoalesceLatest => QueuePolicy::CoalesceLatest,
    }
}

/// 规则适配：`HookRule` → [`DispatchRule`]（仅匹配/排序/队列字段）。
pub fn dispatch_rule(rule: &HookRule) -> DispatchRule {
    DispatchRule {
        id: rule.id.clone(),
        order: rule.order,
        event: rule.event.clone(),
        matcher: rule.matcher.clone(),
        enabled: rule.enabled,
        on_full: queue_policy(rule.on_full),
        parallel: rule.parallel,
        cooldown_ms: rule.cooldown_ms,
    }
}

/// 规则适配：`HookRule` → [`ProcessSpec`]（进程执行规格）。
///
/// `env` 按 key 排序以确定性；`cwd` 的字面量 `project` 解析为 `project_path`，
/// 缺省保留 `None`（由 [`SpawnHandler`] 取 `project_path`）。
pub fn process_spec(rule: &HookRule, project_path: &str) -> ProcessSpec {
    let mut env: Vec<(String, String)> = rule
        .env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    env.sort();
    ProcessSpec {
        command: rule.command.clone(),
        args: rule.args.clone(),
        env,
        timeout_ms: rule.timeout_ms,
        cwd: resolve_cwd(rule.cwd.as_deref(), project_path),
    }
}

/// 解析规则 `cwd`：字面量 `project` → 会话/项目路径；其余原样。
fn resolve_cwd(cwd: Option<&str>, project_path: &str) -> Option<PathBuf> {
    match cwd {
        None => None,
        Some("project") => Some(PathBuf::from(project_path)),
        Some(path) => Some(PathBuf::from(path)),
    }
}

/// 便捷入口：以信任存储判定项目级规则是否可运行（`trust::verify(...) == Trusted`）。
///
/// 无项目级规则时 [`verify`] 返回 `NotRequired`，此处返回 `false`（无需门控，交由
/// 调用方按「无项目规则」处理）。
pub fn project_rules_trusted(project: &Path, rules: &[HookRule], store: &HookTrustStore) -> bool {
    matches!(verify(project, rules, store), TrustStatus::Trusted)
}

/// 未受信任的项目规则被排除时的统一告警。
fn warn_untrusted_project_rules(project_rules: &[HookRule]) {
    tracing::warn!(
        count = project_rules.len(),
        "项目 hook 规则未受信任，本次不加载"
    );
}

/// 接线 hook 运行时到事件总线（进程装配入口）。
///
/// 规则取自合并后的 [`HooksConfig`]：按 [`HookScope`] 拆分全局/项目规则，项目规则经
/// [`project_rules_trusted`] 门控。**生效规则为空**（无规则，或仅有未受信任的项目规则）
/// 时返回 `None`——不构建执行器、不订阅总线、零开销。有生效规则时以默认
/// [`SpawnHandler`] 构建运行时、订阅总线并 spawn 消费循环：`BusEnvelope.event` 与 `seq`
/// 逐条交给 [`HookRuntime::dispatch_event`]（`Lagged` 记 warn 后继续，`Closed` 退出）。
///
/// 返回的运行时兼作关停期 [`HookDrainHost`]。
pub fn setup_hook_runtime(
    config: &HooksConfig,
    project_path: &Path,
    trust_store: &HookTrustStore,
    bus: &Arc<EventBus>,
) -> Option<Arc<HookRuntime>> {
    setup_hook_runtime_inner(config, project_path, trust_store, bus, None)
}

/// 可注入 [`Handler`] 的接线入口（测试 / 内嵌消费者用）。
///
/// 与 [`setup_hook_runtime`] 行为一致，仅以 `handler` 替代默认 [`SpawnHandler`]。
pub fn setup_hook_runtime_with_handler(
    config: &HooksConfig,
    project_path: &Path,
    trust_store: &HookTrustStore,
    bus: &Arc<EventBus>,
    handler: Arc<dyn Handler>,
) -> Option<Arc<HookRuntime>> {
    setup_hook_runtime_inner(config, project_path, trust_store, bus, Some(handler))
}

/// 拆分合并后的规则集为（全局, 项目）两份，保持原顺序。
fn split_scopes(config: &HooksConfig) -> (Vec<HookRule>, Vec<HookRule>) {
    let global = config
        .rules
        .iter()
        .filter(|rule| rule.scope == HookScope::Global)
        .cloned()
        .collect();
    let project = config
        .rules
        .iter()
        .filter(|rule| rule.scope == HookScope::Project)
        .cloned()
        .collect();
    (global, project)
}

/// 订阅总线并 spawn 消费循环（`Lagged` 记 warn 后继续，`Closed` 退出）。
fn subscribe_bus(runtime: Arc<HookRuntime>, bus: &Arc<EventBus>) {
    let mut receiver = bus.subscribe();
    let consumer = Arc::clone(&runtime);
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(envelope) => consumer.dispatch_event(envelope.event, envelope.seq),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    consumer.record_dropped(skipped);
                    tracing::warn!(skipped, "hook 运行时消费滞后，丢弃部分事件");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// 接线主体：规则拆分 + 信任门控 + 零开销短路 + 构建 + 订阅 + 消费循环。
fn setup_hook_runtime_inner(
    config: &HooksConfig,
    project_path: &Path,
    trust_store: &HookTrustStore,
    bus: &Arc<EventBus>,
    handler: Option<Arc<dyn Handler>>,
) -> Option<Arc<HookRuntime>> {
    let (global, project) = split_scopes(config);
    let trusted = project_rules_trusted(project_path, &project, trust_store);

    // 生效规则为零 → 不构建、不订阅（零开销）。未受信任的项目规则在此显式告警，
    // 不进入下方 [`HookRuntime::build`] 的 `select_rules` 路径。
    if global.is_empty() && (project.is_empty() || !trusted) {
        if !project.is_empty() {
            warn_untrusted_project_rules(&project);
        }
        return None;
    }

    let map_ctx = session_ctx(project_path.to_string_lossy());
    let runtime = match handler {
        Some(handler) => {
            HookRuntime::build_with_handler(&global, &project, trusted, map_ctx, handler)
        }
        None => HookRuntime::build(&global, &project, trusted, map_ctx),
    };
    let runtime = Arc::new(runtime);
    subscribe_bus(Arc::clone(&runtime), bus);
    Some(runtime)
}

/// hook 运行时：持有执行器与映射上下文，向总线消费方提供分发、drain 与只读计数入口。
pub struct HookRuntime {
    executor: Executor,
    map_ctx: MapCtx,
    /// 是否接收新派发；drain 后置 `false`。
    accepting: AtomicBool,
    /// 进入运行时的 hook 事件数（设计 §15 `emitted`）。
    emitted: AtomicU64,
    /// 运行时丢弃的事件数（关停后到达 / 总线消费者滞后）。
    dropped: AtomicU64,
    /// 进程执行计数来源（默认 [`SpawnHandler`]）。
    exec_counters: Arc<dyn ExecCounters>,
    /// 生效规则摘要（供 `GetHookStats`）。
    rules_summary: Vec<RuleSummary>,
}

impl HookRuntime {
    /// 构建运行时（进程 Handler 为 [`SpawnHandler`]）。
    ///
    /// 全局规则全部进入执行集；项目规则**仅**在 `trusted` 为真时进入。**零规则**时
    /// 返回一个「空运行时不派发」的实例（不返回 `None`）——是否构建由调用方决定。
    /// `trusted` 由调用方经 [`project_rules_trusted`] / [`verify`] 得出。
    pub fn build(
        global_rules: &[HookRule],
        project_rules: &[HookRule],
        trusted: bool,
        map_ctx: MapCtx,
    ) -> Self {
        let project_path = map_ctx.project_path.clone();
        let selected = select_rules(global_rules, project_rules, trusted);
        let specs: HashMap<String, ProcessSpec> = selected
            .iter()
            .map(|rule| (rule.id.clone(), process_spec(rule, &project_path)))
            .collect();
        let handler = Arc::new(SpawnHandler::new(project_path, specs));
        let counters: Arc<dyn ExecCounters> = handler.clone();
        Self::assemble(&selected, map_ctx, handler, counters)
    }

    /// 以可注入 [`Handler`] 构建运行时（测试/内嵌 consumers 用）。
    ///
    /// 注入 handler 无进程计数 → 执行计数恒为零（[`NoopExecCounters`]）。
    pub fn build_with_handler(
        global_rules: &[HookRule],
        project_rules: &[HookRule],
        trusted: bool,
        map_ctx: MapCtx,
        handler: Arc<dyn Handler>,
    ) -> Self {
        let selected = select_rules(global_rules, project_rules, trusted);
        Self::assemble(&selected, map_ctx, handler, Arc::new(NoopExecCounters))
    }

    /// 以可注入 [`Handler`] **与**执行计数来源构建运行时（测试用）。
    pub fn build_with_handler_and_counters(
        global_rules: &[HookRule],
        project_rules: &[HookRule],
        trusted: bool,
        map_ctx: MapCtx,
        handler: Arc<dyn Handler>,
        exec_counters: Arc<dyn ExecCounters>,
    ) -> Self {
        let selected = select_rules(global_rules, project_rules, trusted);
        Self::assemble(&selected, map_ctx, handler, exec_counters)
    }

    /// 由已选规则集与 Handler 组装。
    fn assemble(
        selected: &[&HookRule],
        map_ctx: MapCtx,
        handler: Arc<dyn Handler>,
        exec_counters: Arc<dyn ExecCounters>,
    ) -> Self {
        let rules: Vec<DispatchRule> = selected.iter().map(|rule| dispatch_rule(rule)).collect();
        let rules_summary: Vec<RuleSummary> = selected
            .iter()
            .map(|rule| RuleSummary {
                id: rule.id.clone(),
                scope: rule.scope,
                enabled: rule.enabled,
            })
            .collect();
        Self {
            executor: Executor::new(rules, handler),
            map_ctx,
            accepting: AtomicBool::new(true),
            emitted: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            exec_counters,
            rules_summary,
        }
    }

    /// 分发一个总线事件。
    ///
    /// `Hook` 事件直接转为 [`DispatchInput`]（总线 `seq` 回填信封）；`Frame` 事件经
    /// [`map_frame`] 映射，未映射的帧忽略。`drain` 之后不再接收新派发（计为丢弃）。
    pub fn dispatch_event(&self, event: BusEvent, seq: u64) {
        if !self.accepting.load(Ordering::Acquire) {
            self.record_dropped(1);
            return;
        }
        let hook_event = match event {
            BusEvent::Hook(event) => event,
            BusEvent::Frame(frame) => match map_frame(&frame, &self.map_ctx) {
                Some(event) => event,
                None => return,
            },
        };
        self.dispatch_hook(hook_event, seq);
    }

    /// 直接派发已映射的 [`HookEvent`]（**不做 `accepting` 检查**）。
    ///
    /// 供 [`HookDrainHost::emit_terminal`] 在关停期绕过门控直投终态事件；普通总线
    /// 分发路径经 [`HookRuntime::dispatch_event`] 复用之。
    fn dispatch_hook(&self, hook_event: HookEvent, seq: u64) {
        self.emitted.fetch_add(1, Ordering::Relaxed);
        self.executor.dispatch(dispatch_input(hook_event, seq));
    }

    /// 记录 `count` 个被运行时丢弃的事件（总线消费者滞后等）。
    pub fn record_dropped(&self, count: u64) {
        self.dropped.fetch_add(count, Ordering::Relaxed);
    }

    /// 停止接收新派发，并在 `budget` 内尽力排空执行队列；返回是否在 budget 内排空。
    pub async fn drain(&self, budget: Duration) -> bool {
        self.accepting.store(false, Ordering::Release);
        self.executor.drain(budget).await
    }
}

impl HookStatsSource for HookRuntime {
    fn hook_stats(&self) -> HookStatsSnapshot {
        HookStatsSnapshot {
            emitted: self.emitted.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            executed: self.exec_counters.executed(),
            failed: self.exec_counters.failed(),
            timed_out: self.exec_counters.timed_out(),
            rules: self.rules_summary.clone(),
        }
    }
}

#[async_trait]
impl HookDrainHost for HookRuntime {
    async fn drain(&self, budget: Duration) {
        let _ = HookRuntime::drain(self, budget).await;
    }

    async fn emit_terminal(&self, event: HookEvent) {
        // 绕过 `accepting` 门控：drain 之后（accepting=false）仍能入队。
        self.dispatch_hook(event, 0);
    }
}

/// 规则收集 + 信任门控：全局全部，项目仅在受信任时并入。
fn select_rules<'a>(
    global_rules: &'a [HookRule],
    project_rules: &'a [HookRule],
    trusted: bool,
) -> Vec<&'a HookRule> {
    let mut selected: Vec<&HookRule> = global_rules.iter().collect();
    if trusted {
        selected.extend(project_rules.iter());
    } else if !project_rules.is_empty() {
        warn_untrusted_project_rules(project_rules);
    }
    selected
}

/// [`HookEvent`] → [`DispatchInput`]：回填总线 `seq`，提取 `matcher` 维度与 JSON 载荷。
fn dispatch_input(mut event: HookEvent, seq: u64) -> DispatchInput {
    event.context.seq = Some(seq);
    let mut input =
        DispatchInput::new(event.event_name()).with_source(source_str(event.context.source));
    if let Some(tool_name) = payload_tool_name(&event.payload) {
        input = input.with_tool_name(tool_name);
    }
    if let Some(kind) = payload_kind(&event.payload) {
        input = input.with_kind(kind);
    }
    input.with_payload(event.to_value().to_string())
}

/// 会话来源的 `matcher` 视图（与序列化拼写一致）。
fn source_str(source: SessionSource) -> &'static str {
    match source {
        SessionSource::Startup => "startup",
        SessionSource::Resume => "resume",
    }
}

/// 工具类事件携带的 `tool_name`。
fn payload_tool_name(payload: &HookPayload) -> Option<&str> {
    match payload {
        HookPayload::ToolCallRequested(p) => Some(p.tool_name.as_str()),
        HookPayload::PreToolUse(p) => Some(p.tool_name.as_str()),
        HookPayload::PostToolUse(p) => Some(p.tool_name.as_str()),
        HookPayload::PostToolUseFailure(p) => Some(p.tool_name.as_str()),
        _ => None,
    }
}

/// `PermissionRequest.kind` 的 `matcher` 视图。
fn payload_kind(payload: &HookPayload) -> Option<&'static str> {
    match payload {
        HookPayload::PermissionRequest(p) => Some(match p.kind {
            PermissionKind::Approval => "approval",
            PermissionKind::Question => "question",
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;
    use std::time::Instant;

    use crate::bus::EventBus;
    use visp_config::hooks::{DEFAULT_HOOK_TIMEOUT_MS, HookScope, HooksConfig};
    use visp_config::trust::HookTrustStore;
    use visp_core::agent::{AgentEvent, AgentEventFrame};
    use visp_hooks::{
        HookContext, HookEventName, RecordingHandler, StopPayload, StopStatus, VISP_HOOK_SCHEMA,
    };

    const PROJECT: &str = "/tmp/visp-project";

    fn rule(id: &str) -> HookRule {
        HookRule {
            id: id.to_string(),
            order: None,
            event: vec![HookEventName::Stop],
            matcher: None,
            command: "/bin/true".to_string(),
            args: Vec::new(),
            env: Map::new(),
            cwd: None,
            timeout_ms: DEFAULT_HOOK_TIMEOUT_MS,
            enabled: true,
            on_full: OnFull::DropNew,
            parallel: false,
            cooldown_ms: 0,
            include: Vec::new(),
            scope: HookScope::Global,
        }
    }

    fn frame(event: AgentEvent) -> AgentEventFrame {
        AgentEventFrame {
            event,
            session_id: "s1".to_string(),
            agent_name: "agent".to_string(),
            parent_session_id: None,
            parent_session_name: None,
        }
    }

    fn stop_hook_event() -> HookEvent {
        HookEvent {
            context: HookContext {
                schema: VISP_HOOK_SCHEMA,
                hook_event_name: HookEventName::Stop,
                session_id: "s1".to_string(),
                cwd: PROJECT.to_string(),
                source: SessionSource::Startup,
                origin: Origin::Tui,
                seq: None,
            },
            payload: HookPayload::Stop(StopPayload {
                status: StopStatus::Completed,
                input_tokens: 3,
                output_tokens: 5,
                tool_calls: 1,
            }),
        }
    }

    fn stop_event() -> BusEvent {
        BusEvent::Hook(stop_hook_event())
    }

    fn tool_frame() -> BusEvent {
        BusEvent::Frame(frame(AgentEvent::ToolCallRequest {
            call_id: "c1".to_string(),
            tool_name: "Bash".to_string(),
            arguments: "{}".to_string(),
        }))
    }

    fn runtime_with(
        global: &[HookRule],
        project: &[HookRule],
        trusted: bool,
        handler: Arc<RecordingHandler>,
    ) -> HookRuntime {
        HookRuntime::build_with_handler(global, project, trusted, session_ctx(PROJECT), handler)
    }

    /// 1. `HookRule` → `DispatchRule` 字段逐一映射。
    #[test]
    fn dispatch_rule_maps_every_field() {
        let mut rule = rule("r1");
        rule.order = Some(7);
        rule.event = vec![HookEventName::Stop, HookEventName::SessionEnd];
        rule.matcher = Some("^Bash$".to_string());
        rule.enabled = false;
        rule.on_full = OnFull::DropOld;
        rule.parallel = true;
        rule.cooldown_ms = 250;

        let dispatch = dispatch_rule(&rule);
        assert_eq!(dispatch.id, "r1");
        assert_eq!(dispatch.order, Some(7));
        assert_eq!(
            dispatch.event,
            vec![HookEventName::Stop, HookEventName::SessionEnd]
        );
        assert_eq!(dispatch.matcher.as_deref(), Some("^Bash$"));
        assert!(!dispatch.enabled);
        assert_eq!(dispatch.on_full, QueuePolicy::DropOld);
        assert!(dispatch.parallel);
        assert_eq!(dispatch.cooldown_ms, 250);
    }

    /// 2. `OnFull` ↔ `QueuePolicy` 逐一往返（含缺省 `DropNew`）。
    #[test]
    fn on_full_round_trips_to_queue_policy() {
        for (on_full, policy) in [
            (OnFull::DropNew, QueuePolicy::DropNew),
            (OnFull::DropOld, QueuePolicy::DropOld),
            (OnFull::CoalesceLatest, QueuePolicy::CoalesceLatest),
        ] {
            assert_eq!(queue_policy(on_full), policy);
            let mut rule = rule("r");
            rule.on_full = on_full;
            assert_eq!(dispatch_rule(&rule).on_full, policy);
        }
    }

    /// 3. `HookRule` → `ProcessSpec` 字段映射与 `cwd` 解析。
    #[test]
    fn process_spec_maps_fields_and_resolves_cwd() {
        let mut rule = rule("r");
        rule.command = "/bin/echo".to_string();
        rule.args = vec!["hi".to_string()];
        rule.env = Map::from([
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "1".to_string()),
        ]);
        rule.cwd = Some("project".to_string());
        rule.timeout_ms = 1234;

        let spec = process_spec(&rule, PROJECT);
        assert_eq!(spec.command, "/bin/echo");
        assert_eq!(spec.args, vec!["hi".to_string()]);
        assert_eq!(
            spec.env,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string())
            ]
        );
        assert_eq!(spec.cwd, Some(PathBuf::from(PROJECT)));
        assert_eq!(spec.timeout_ms, 1234);

        // 缺省 cwd → None（执行器回退 project_path）。
        rule.cwd = None;
        assert_eq!(process_spec(&rule, PROJECT).cwd, None);

        // 显式路径原样透传。
        rule.cwd = Some("/elsewhere".to_string());
        assert_eq!(
            process_spec(&rule, PROJECT).cwd,
            Some(PathBuf::from("/elsewhere"))
        );
    }

    /// 4a. 项目规则未受信任 → 不进入执行集（只有全局规则被执行）。
    #[tokio::test]
    async fn untrusted_project_rules_are_excluded() {
        let handler = Arc::new(RecordingHandler::new());
        let global = vec![rule("global")];
        let project = vec![rule("project-rule")];
        let runtime = runtime_with(&global, &project, false, handler.clone());

        runtime.dispatch_event(stop_event(), 1);
        tokio::time::timeout(Duration::from_secs(1), handler.wait_for(1))
            .await
            .expect("全局规则应被执行");

        let records = handler.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].rule_id, "global");
    }

    /// 4b. 项目规则受信任 → 与全局规则一并进入执行集。
    #[tokio::test]
    async fn trusted_project_rules_are_included() {
        let handler = Arc::new(RecordingHandler::new());
        let global = vec![rule("global")];
        let project = vec![rule("project-rule")];
        let runtime = runtime_with(&global, &project, true, handler.clone());

        runtime.dispatch_event(stop_event(), 1);
        tokio::time::timeout(Duration::from_secs(1), handler.wait_for(2))
            .await
            .expect("全局与项目规则均应被执行");

        let ids: Vec<String> = handler
            .records()
            .into_iter()
            .map(|record| record.rule_id)
            .collect();
        assert_eq!(ids, vec!["global".to_string(), "project-rule".to_string()]);
    }

    /// 4c. `project_rules_trusted` 走 `trust::verify`：未信任 false、信任后 true。
    #[test]
    fn project_rules_trusted_follows_verify() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        std::fs::create_dir_all(project.join(".visp/hooks")).unwrap();
        std::fs::write(project.join(".visp/hooks/a.sh"), "#!/bin/sh\n").unwrap();

        let mut rule = rule("p");
        rule.scope = HookScope::Project;
        rule.command = ".visp/hooks/a.sh".to_string();
        let rules = vec![rule];

        let mut store = HookTrustStore::default();
        assert!(!project_rules_trusted(project, &rules, &store));
        store.trust(project, &rules).unwrap();
        assert!(project_rules_trusted(project, &rules, &store));
    }

    /// 5. `Hook` 事件 → Handler 被调用一次，事件/信号与 seq 回填正确。
    #[tokio::test]
    async fn hook_event_dispatches_once_with_payload_and_seq() {
        let handler = Arc::new(RecordingHandler::new());
        let runtime = runtime_with(&[rule("r")], &[], false, handler.clone());

        runtime.dispatch_event(stop_event(), 42);
        tokio::time::timeout(Duration::from_secs(1), handler.wait_for(1))
            .await
            .expect("handler 应被调用一次");

        let records = handler.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].rule_id, "r");
        assert_eq!(records[0].event.event, HookEventName::Stop);
        assert_eq!(records[0].event.source.as_deref(), Some("startup"));

        let payload: serde_json::Value =
            serde_json::from_str(records[0].event.payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["hook_event_name"], "Stop");
        assert_eq!(payload["seq"], 42);
    }

    /// 6. `Frame` 事件经 `hook_map` 映射后派发（tool_name 参与 matcher）。
    #[tokio::test]
    async fn mapped_frame_dispatches_tool_event() {
        let mut rule = rule("r");
        rule.event = vec![HookEventName::ToolCallRequested];
        rule.matcher = Some("^Bash$".to_string());
        let handler = Arc::new(RecordingHandler::new());
        let runtime = runtime_with(&[rule], &[], false, handler.clone());

        runtime.dispatch_event(tool_frame(), 3);
        tokio::time::timeout(Duration::from_secs(1), handler.wait_for(1))
            .await
            .expect("映射后的事件应被派发");

        let records = handler.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event.event, HookEventName::ToolCallRequested);
        assert_eq!(records[0].event.tool_name.as_deref(), Some("Bash"));
    }

    /// 7. `Frame` 映射为 `None` → 不调用（规则命中全部事件仍不触发）。
    #[tokio::test]
    async fn unmapped_frame_is_ignored() {
        let mut rule = rule("r");
        rule.event = HookEventName::ALL.to_vec();
        let handler = Arc::new(RecordingHandler::new());
        let runtime = runtime_with(&[rule], &[], false, handler.clone());

        runtime.dispatch_event(
            BusEvent::Frame(frame(AgentEvent::TextDelta("hi".to_string()))),
            1,
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(handler.records().is_empty());
    }

    /// 8. 零规则 → 无派发（返回空运行时，不返回 None）。
    #[tokio::test]
    async fn zero_rules_dispatch_nothing() {
        let handler = Arc::new(RecordingHandler::new());
        let runtime = runtime_with(&[], &[], false, handler.clone());

        runtime.dispatch_event(stop_event(), 1);
        runtime.dispatch_event(tool_frame(), 2);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(handler.records().is_empty());
    }

    /// 9a. 忙碌时 `drain` 在 budget 内返回（有界，不悬挂），并停止接收新派发。
    #[tokio::test]
    async fn drain_is_bounded_when_busy() {
        let handler = Arc::new(RecordingHandler::blocking());
        let runtime = runtime_with(&[rule("r")], &[], false, handler.clone());

        runtime.dispatch_event(stop_event(), 1);
        tokio::time::timeout(Duration::from_secs(1), handler.wait_for(1))
            .await
            .expect("worker 应已进入执行");

        let budget = Duration::from_millis(80);
        let started = Instant::now();
        let drained = runtime.drain(budget).await;
        let elapsed = started.elapsed();

        assert!(!drained, "阻塞执行时不应判为排空");
        assert!(elapsed >= budget, "应在 budget 用尽后返回");
        assert!(
            elapsed < budget + Duration::from_secs(1),
            "应尽快返回，不悬挂"
        );

        runtime.dispatch_event(stop_event(), 2);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(handler.records().len(), 1, "drain 后不应再派发");

        handler.release();
    }

    /// 9b. 空闲时 `drain` 立即判为排空。
    #[tokio::test]
    async fn drain_returns_true_when_idle() {
        let handler = Arc::new(RecordingHandler::new());
        let runtime = runtime_with(&[rule("r")], &[], false, handler.clone());

        runtime.dispatch_event(stop_event(), 1);
        tokio::time::timeout(Duration::from_secs(1), handler.wait_for(1))
            .await
            .expect("worker 应已执行");
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(runtime.drain(Duration::from_millis(500)).await);
    }

    /// 9c. `HookDrainHost` 可经 `Arc<dyn>` 注入（关停管道契约）。
    #[tokio::test]
    async fn drain_host_trait_object_works() {
        let handler = Arc::new(RecordingHandler::new());
        let runtime = runtime_with(&[], &[], false, handler);
        let host: Arc<dyn HookDrainHost> = Arc::new(runtime);
        host.drain(Duration::from_millis(10)).await;
    }

    /// 9d. 回归（SessionEnd 竞态修复）：`emit_terminal` 绕过 `accepting` 门控——
    ///     `drain`（accepting=false）之后仍能把事件入队并被 handler 观察到；
    ///     对照 `dispatch_event` 此时被丢弃。
    #[tokio::test]
    async fn emit_terminal_delivers_after_drain() {
        let handler = Arc::new(RecordingHandler::new());
        let runtime = Arc::new(runtime_with(&[rule("r")], &[], false, handler.clone()));
        let host: Arc<dyn HookDrainHost> = runtime.clone();

        // 先 drain → accepting=false。
        host.drain(Duration::from_millis(200)).await;

        // drain 之后经总线派发 → 丢弃，不触发 handler。
        runtime.dispatch_event(stop_event(), 1);
        assert_eq!(runtime.hook_stats().dropped, 1);
        assert!(handler.records().is_empty());

        // drain 之后直投终态 → 不被丢弃，handler 观察到。
        host.emit_terminal(stop_hook_event()).await;
        tokio::time::timeout(Duration::from_secs(1), handler.wait_for(1))
            .await
            .expect("emit_terminal 应在 drain 之后仍被派发");
        assert_eq!(handler.records()[0].rule_id, "r");
        assert_eq!(runtime.hook_stats().emitted, 1);
        assert_eq!(runtime.hook_stats().dropped, 1, "直投不计入丢弃");
    }

    /// 9e. 回归（关停顺序）：先 `emit_terminal` 再 `drain`，终态事件不会丢失
    ///     （与 daemon `shutdown` 的顺序一致）。
    #[tokio::test]
    async fn emit_terminal_then_drain_keeps_event() {
        let handler = Arc::new(RecordingHandler::new());
        let runtime = Arc::new(runtime_with(&[rule("r")], &[], false, handler.clone()));
        let host: Arc<dyn HookDrainHost> = runtime.clone();

        host.emit_terminal(stop_hook_event()).await;
        host.drain(Duration::from_millis(500)).await;

        tokio::time::timeout(Duration::from_secs(1), handler.wait_for(1))
            .await
            .expect("先 emit_terminal 再 drain，终态事件不应丢失");
        assert_eq!(handler.records()[0].rule_id, "r");
    }

    // ── 计数与只读快照（步骤 1b-3a：设计 §15） ──

    /// 可注入的执行计数来源（测试用）。
    #[derive(Default)]
    struct FakeExecCounters {
        executed: AtomicU64,
        failed: AtomicU64,
        timed_out: AtomicU64,
    }

    impl ExecCounters for FakeExecCounters {
        fn executed(&self) -> u64 {
            self.executed.load(Ordering::SeqCst)
        }
        fn failed(&self) -> u64 {
            self.failed.load(Ordering::SeqCst)
        }
        fn timed_out(&self) -> u64 {
            self.timed_out.load(Ordering::SeqCst)
        }
    }

    /// 轮询等待条件成立（有上限，避免悬挂）。
    async fn wait_until(mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cond() {
            assert!(Instant::now() < deadline, "条件未在超时内满足");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// 11a. 快照初值为零，且携带生效规则摘要（id/scope/enabled）。
    #[test]
    fn stats_snapshot_lists_effective_rules() {
        let mut project_rule = rule("project");
        project_rule.scope = HookScope::Project;
        project_rule.enabled = false;
        let handler = Arc::new(RecordingHandler::new());
        let runtime = runtime_with(&[rule("global")], &[project_rule], true, handler);

        let snapshot = runtime.hook_stats();
        assert_eq!(
            (
                snapshot.emitted,
                snapshot.dropped,
                snapshot.executed,
                snapshot.failed,
                snapshot.timed_out,
            ),
            (0, 0, 0, 0, 0)
        );
        assert_eq!(snapshot.rules.len(), 2);
        assert_eq!(snapshot.rules[0].id, "global");
        assert_eq!(snapshot.rules[0].scope_str(), "global");
        assert!(snapshot.rules[0].enabled);
        assert_eq!(snapshot.rules[1].id, "project");
        assert_eq!(snapshot.rules[1].scope_str(), "project");
        assert!(!snapshot.rules[1].enabled);
    }

    /// 11b. `emitted` 随进入运行时的 hook 事件增长；未映射帧不计。
    #[tokio::test]
    async fn stats_emitted_grows_with_dispatched_events() {
        let handler = Arc::new(RecordingHandler::new());
        let runtime = runtime_with(&[rule("r")], &[], false, handler);
        assert_eq!(runtime.hook_stats().emitted, 0);

        runtime.dispatch_event(stop_event(), 1);
        // 未映射帧（规则命中全部事件也不派发）不计入 emitted。
        runtime.dispatch_event(BusEvent::Frame(frame(AgentEvent::TextDelta("x".into()))), 2);

        assert_eq!(runtime.hook_stats().emitted, 1);
        assert_eq!(runtime.hook_stats().dropped, 0);
    }

    /// 11c. `dropped` 记录关停（drain）后到达的派发与总线滞后丢弃。
    #[tokio::test]
    async fn stats_dropped_records_post_drain_and_lag() {
        let handler = Arc::new(RecordingHandler::new());
        let runtime = runtime_with(&[rule("r")], &[], false, handler.clone());

        runtime.dispatch_event(stop_event(), 1);
        tokio::time::timeout(Duration::from_secs(1), handler.wait_for(1))
            .await
            .expect("首条事件应被派发");
        assert!(runtime.drain(Duration::from_millis(200)).await);

        // drain 之后再派发 → 计为 dropped，不增加 emitted。
        runtime.dispatch_event(stop_event(), 2);
        assert_eq!(runtime.hook_stats().emitted, 1);
        assert_eq!(runtime.hook_stats().dropped, 1);

        // 总线滞后（broadcast Lagged）跳过的条数计入 dropped。
        runtime.record_dropped(3);
        assert_eq!(runtime.hook_stats().dropped, 4);
    }

    /// 11d. `executed`/`failed`/`timed_out` 从注入的执行计数来源读取。
    #[tokio::test]
    async fn stats_reads_injected_exec_counters() {
        let handler = Arc::new(RecordingHandler::new());
        let counters = Arc::new(FakeExecCounters::default());
        let runtime = HookRuntime::build_with_handler_and_counters(
            &[rule("r")],
            &[],
            false,
            session_ctx(PROJECT),
            handler,
            counters.clone(),
        );

        counters.executed.fetch_add(2, Ordering::SeqCst);
        counters.failed.fetch_add(1, Ordering::SeqCst);
        counters.timed_out.fetch_add(4, Ordering::SeqCst);

        let snapshot = runtime.hook_stats();
        assert_eq!(
            (snapshot.executed, snapshot.failed, snapshot.timed_out),
            (2, 1, 4)
        );
    }

    /// 11e. 默认 `SpawnHandler` 路径：真实进程的执行结果反映到快照。
    #[tokio::test]
    async fn stats_counts_real_spawn_handler_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().to_string_lossy().into_owned();

        let mut ok_rule = rule("ok");
        ok_rule.command = "sh".to_string();
        ok_rule.args = vec!["-c".to_string(), "exit 0".to_string()];
        let mut fail_rule = rule("fail");
        fail_rule.command = "sh".to_string();
        fail_rule.args = vec!["-c".to_string(), "exit 3".to_string()];
        let mut timeout_rule = rule("timeout");
        timeout_rule.command = "sh".to_string();
        timeout_rule.args = vec!["-c".to_string(), "sleep 5".to_string()];
        timeout_rule.timeout_ms = 50;

        let runtime = HookRuntime::build(
            &[ok_rule, fail_rule, timeout_rule],
            &[],
            false,
            session_ctx(project),
        );

        runtime.dispatch_event(stop_event(), 1);
        wait_until(|| {
            let stats = runtime.hook_stats();
            stats.executed == 1 && stats.failed == 1 && stats.timed_out == 1
        })
        .await;

        let snapshot = runtime.hook_stats();
        assert_eq!(snapshot.executed, 1);
        assert_eq!(snapshot.failed, 1);
        assert_eq!(snapshot.timed_out, 1);
    }

    /// 11f. `NoopHookStats` 返回全零 + 空规则（无执行器时的只读兜底）。
    #[test]
    fn noop_hook_stats_is_all_zero() {
        assert_eq!(NoopHookStats.hook_stats(), HookStatsSnapshot::default());
    }

    // ── 接线 helper（bus → dispatch；任务 1b-2c-b） ──

    /// 10a. 零规则 → 不构建、不订阅（零开销）。
    #[tokio::test]
    async fn setup_without_rules_returns_none_and_does_not_subscribe() {
        let bus = Arc::new(EventBus::new());
        let before = bus.receiver_count();

        assert!(
            setup_hook_runtime(
                &HooksConfig::default(),
                Path::new(PROJECT),
                &HookTrustStore::default(),
                &bus,
            )
            .is_none()
        );
        assert_eq!(bus.receiver_count(), before, "零规则不应订阅总线");
    }

    /// 10b. 有全局规则 → `Some`；总线发布可映射 `Frame` 后 handler 被调用；
    ///      返回的运行时可用作 `Arc<dyn HookDrainHost>`。
    #[tokio::test]
    async fn setup_with_global_rule_wires_bus_frame_to_handler() {
        let bus = Arc::new(EventBus::new());
        let handler = Arc::new(RecordingHandler::new());
        let mut rule = rule("global");
        rule.event = vec![HookEventName::ToolCallRequested];
        rule.matcher = Some("^Bash$".to_string());
        let config = HooksConfig { rules: vec![rule] };

        let runtime = setup_hook_runtime_with_handler(
            &config,
            Path::new(PROJECT),
            &HookTrustStore::default(),
            &bus,
            handler.clone(),
        )
        .expect("有规则应构建运行时");

        bus.publish(tool_frame());
        tokio::time::timeout(Duration::from_secs(1), handler.wait_for(1))
            .await
            .expect("Frame 应经总线映射后被派发");
        let records = handler.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].rule_id, "global");

        let host: Arc<dyn HookDrainHost> = runtime;
        host.drain(Duration::from_millis(10)).await;
    }

    /// 10c. 未注入 handler 时走默认 `SpawnHandler` 构建路径。
    #[tokio::test]
    async fn setup_with_default_handler_returns_runtime() {
        let bus = Arc::new(EventBus::new());
        let mut rule = rule("global");
        rule.command = "/bin/true".to_string();
        let config = HooksConfig { rules: vec![rule] };

        let runtime = setup_hook_runtime(
            &config,
            Path::new(PROJECT),
            &HookTrustStore::default(),
            &bus,
        )
        .expect("有全局规则应构建运行时");

        let host: Arc<dyn HookDrainHost> = runtime;
        host.drain(Duration::from_millis(10)).await;
    }

    /// 10d. 项目级规则未受信任 → 被排除（不派发）；全局规则不受影响。
    #[tokio::test]
    async fn setup_excludes_untrusted_project_rules() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        std::fs::create_dir_all(project.join(".visp/hooks")).unwrap();
        std::fs::write(project.join(".visp/hooks/a.sh"), "#!/bin/sh\n").unwrap();

        let mut project_rule = rule("project-rule");
        project_rule.scope = HookScope::Project;
        project_rule.command = ".visp/hooks/a.sh".to_string();
        project_rule.event = vec![HookEventName::ToolCallRequested];

        let mut global_rule = rule("global");
        global_rule.event = vec![HookEventName::ToolCallRequested];

        let config = HooksConfig {
            rules: vec![global_rule, project_rule],
        };
        let bus = Arc::new(EventBus::new());
        let handler = Arc::new(RecordingHandler::new());
        setup_hook_runtime_with_handler(
            &config,
            project,
            &HookTrustStore::default(),
            &bus,
            handler.clone(),
        )
        .expect("有全局规则应构建运行时");

        bus.publish(tool_frame());
        tokio::time::timeout(Duration::from_secs(1), handler.wait_for(1))
            .await
            .expect("全局规则应被派发");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let ids: Vec<String> = handler
            .records()
            .into_iter()
            .map(|record| record.rule_id)
            .collect();
        assert_eq!(
            ids,
            vec!["global".to_string()],
            "未受信任的项目规则不得派发"
        );
    }

    /// 10e. 仅有未受信任的项目规则 → 生效规则为零 → 返回 `None`（不订阅）。
    #[tokio::test]
    async fn setup_with_only_untrusted_project_rules_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        std::fs::create_dir_all(project.join(".visp/hooks")).unwrap();
        std::fs::write(project.join(".visp/hooks/a.sh"), "#!/bin/sh\n").unwrap();

        let mut project_rule = rule("project-rule");
        project_rule.scope = HookScope::Project;
        project_rule.command = ".visp/hooks/a.sh".to_string();
        let config = HooksConfig {
            rules: vec![project_rule],
        };
        let bus = Arc::new(EventBus::new());
        let before = bus.receiver_count();

        assert!(setup_hook_runtime(&config, project, &HookTrustStore::default(), &bus).is_none());
        assert_eq!(bus.receiver_count(), before, "未信任项目规则不应订阅总线");
    }
}
