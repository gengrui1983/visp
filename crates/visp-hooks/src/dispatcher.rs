//! hook 执行器的**决策内核**（设计 D4 / §9 执行语义；实施计划步骤 1b-2 测试 1–6）。
//!
//! 本模块只做纯逻辑决策，**不 spawn 进程、不做任何 IO**：
//!
//! - **匹配**：按事件名命中；`matcher` 正则匹配 `tool_name`/`source`/`kind`（缺省 = 全匹配）。
//! - **排序**：同事件多规则按 `(order, id)` 升序（`order` 缺省为 0，故纯 id 字典序；`order` 覆盖）。
//! - **同规则串行**：默认 per-rule 串行（同规则事件按到达顺序执行，不并发交错）；
//!   `parallel = true` 放开同规则并发。
//! - **`on_full`**：`drop_new`（默认）/ `drop_old` / `coalesce_latest`。
//! - **`cooldown_ms`**：窗口内同规则事件被抑制。
//! - **执行抽象**：通过 [`Handler`] 注入，由接线层（1b-2b）实现进程 spawn 等副作用。
//!
//! ## 输入类型为何是模块自己的轻量类型
//!
//! `visp-config` **依赖** `visp-hooks`（复用 [`crate::HookEventName`]），因此本 crate
//! **不能**反向依赖 `visp-config::hooks::HookRule`，否则形成循环依赖。故决策内核以
//! [`DispatchInput`]（总线事件视图）与 [`DispatchRule`]（规则视图）为输入；接线层从
//! `BusEvent` / `HookRule` 适配。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use regex::Regex;
use tokio::sync::watch;

use crate::event::HookEventName;

/// 规则队列默认容量（每条规则独立队列）。
pub const DEFAULT_QUEUE_CAPACITY: usize = 1024;

/// [`Executor::drain`] 的轮询间隔。
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// 规则队列溢出策略（设计 §7.2 `on_full`）。
///
/// 与 `visp-config::hooks::OnFull` 同义；因 crate 边界不引入反向依赖，
/// 由接线层在适配 [`DispatchRule`] 时做一次映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueuePolicy {
    /// 丢新（默认）。
    #[default]
    DropNew,
    /// 丢旧。
    DropOld,
    /// 合并为最新（状态型规则用）。
    CoalesceLatest,
}

/// 决策内核的事件视图：由接线层从 `BusEvent` / `HookEvent` 适配而来。
///
/// 三个可选字段是 `matcher` 的匹配目标；缺省表示该事件不携带此维度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchInput {
    /// 事件名。
    pub event: HookEventName,
    /// 工具名（工具类事件携带）。
    pub tool_name: Option<String>,
    /// 会话来源 `{startup, resume}`（`SessionStart` 等携带）。
    pub source: Option<String>,
    /// `PermissionRequest.kind`（`{approval, question}`）。
    pub kind: Option<String>,
    /// 预序列化的**完整事件 JSON**（设计 D5：作为子进程 stdin 载荷）。
    ///
    /// 由接线层从 [`crate::event::HookEvent`] 序列化后填入；决策内核只透传，
    /// 不解析。为 `None` 时进程 Handler 退化为「由事件视图生成的精简 JSON」。
    pub payload: Option<String>,
}

impl DispatchInput {
    /// 以事件名构造，其余匹配维度为空。
    pub fn new(event: HookEventName) -> Self {
        Self {
            event,
            tool_name: None,
            source: None,
            kind: None,
            payload: None,
        }
    }

    /// 设置 `tool_name`。
    pub fn with_tool_name(mut self, tool_name: impl Into<String>) -> Self {
        self.tool_name = Some(tool_name.into());
        self
    }

    /// 设置 `source`。
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// 设置 `kind`。
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    /// 设置完整事件 JSON 载荷（写入子进程 stdin）。
    pub fn with_payload(mut self, payload: impl Into<String>) -> Self {
        self.payload = Some(payload.into());
        self
    }

    /// 当前事件实际携带的 `matcher` 匹配目标（`tool_name`/`source`/`kind`）。
    pub(crate) fn match_targets(&self) -> impl Iterator<Item = &str> {
        [
            self.tool_name.as_deref(),
            self.source.as_deref(),
            self.kind.as_deref(),
        ]
        .into_iter()
        .flatten()
    }
}

/// 决策内核的规则视图：只含匹配/排序/队列所需的字段。
///
/// 由接线层从 `visp_config::hooks::HookRule` 适配（`command`/`args`/`env` 等执行字段
/// 不属于决策内核，由 [`Handler`] 自行持有）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchRule {
    /// 规则唯一名。
    pub id: String,
    /// 显式顺序覆盖；缺省 0，故纯 `id` 字典序。
    pub order: Option<i64>,
    /// 命中事件名（可多选）。
    pub event: Vec<HookEventName>,
    /// 正则匹配 `tool_name`/`source`/`kind`；`None` = 全匹配。
    pub matcher: Option<String>,
    /// 是否启用。
    pub enabled: bool,
    /// 队列溢出策略。
    pub on_full: QueuePolicy,
    /// 放开同规则并发。
    pub parallel: bool,
    /// 最小触发间隔（毫秒）。
    pub cooldown_ms: u64,
}

impl Default for DispatchRule {
    fn default() -> Self {
        Self {
            id: String::new(),
            order: None,
            event: Vec::new(),
            matcher: None,
            enabled: true,
            on_full: QueuePolicy::DropNew,
            parallel: false,
            cooldown_ms: 0,
        }
    }
}

/// 规则排序键：`(order.unwrap_or(0), id)`。
fn order_key(rule: &DispatchRule) -> (i64, &str) {
    (rule.order.unwrap_or(0), rule.id.as_str())
}

/// 判断单条规则是否命中事件（纯函数，`matcher` 每次重新编译）。
///
/// 语义：`enabled` 且事件名命中；`matcher` 为 `None` 时全匹配，否则要求事件至少携带
/// `tool_name`/`source`/`kind` 之一且其中任一匹配正则。非法正则可视为不命中。
pub fn matches(rule: &DispatchRule, input: &DispatchInput) -> bool {
    if !rule.enabled || !rule.event.contains(&input.event) {
        return false;
    }
    match &rule.matcher {
        None => true,
        Some(pattern) => match Regex::new(pattern) {
            Ok(re) => input.match_targets().any(|target| re.is_match(target)),
            Err(_) => false,
        },
    }
}

/// 命中并排序：返回同一事件下应分发的规则，按 `(order, id)` 升序。
pub fn select_matches<'a>(
    rules: &'a [DispatchRule],
    input: &DispatchInput,
) -> Vec<&'a DispatchRule> {
    let mut selected: Vec<&DispatchRule> =
        rules.iter().filter(|rule| matches(rule, input)).collect();
    selected.sort_by(|a, b| order_key(a).cmp(&order_key(b)));
    selected
}

/// 执行抽象：由上层注入，决策内核只负责「何时调用、以何顺序调用」。
///
/// 本任务不提供进程 spawn 实现；测试用 [`RecordingHandler`] 记录调用。
#[async_trait]
#[allow(clippy::double_must_use)] // async_trait 展开的 Pin<Box<dyn Future>> 触发误报
pub trait Handler: Send + Sync {
    /// 执行一次规则；内核保证：同规则串行（除非 `parallel`），跨规则按 [`select_matches`] 顺序。
    async fn run(&self, rule: &DispatchRule, event: &DispatchInput);
}

/// 编译后的 `matcher` 状态（避免每次分发重新编译正则）。
enum MatcherState {
    All,
    Pattern(Regex),
    Invalid,
}

/// 单条规则的运行时状态（队列 + 串行 worker 标记 + cooldown 时钟）。
struct RuleRuntime {
    spec: DispatchRule,
    matcher: MatcherState,
    queue: Mutex<VecDeque<DispatchInput>>,
    worker_active: AtomicBool,
    last_fired: Mutex<Option<Instant>>,
}

impl RuleRuntime {
    fn new(spec: DispatchRule) -> Self {
        let matcher = match &spec.matcher {
            None => MatcherState::All,
            Some(pattern) => match Regex::new(pattern) {
                Ok(re) => MatcherState::Pattern(re),
                Err(_) => MatcherState::Invalid,
            },
        };
        Self {
            spec,
            matcher,
            queue: Mutex::new(VecDeque::new()),
            worker_active: AtomicBool::new(false),
            last_fired: Mutex::new(None),
        }
    }

    fn matched(&self, input: &DispatchInput) -> bool {
        if !self.spec.enabled || !self.spec.event.contains(&input.event) {
            return false;
        }
        match &self.matcher {
            MatcherState::All => true,
            MatcherState::Invalid => false,
            MatcherState::Pattern(re) => input.match_targets().any(|target| re.is_match(target)),
        }
    }

    /// cooldown 闸门：窗口内返回 `false`（丢弃）；放行时刷新时间戳。
    fn cooldown_allows(&self) -> bool {
        if self.spec.cooldown_ms == 0 {
            return true;
        }
        let mut last_fired = self.last_fired.lock().expect("cooldown 锁");
        let now = Instant::now();
        match *last_fired {
            Some(prev)
                if now.duration_since(prev) < Duration::from_millis(self.spec.cooldown_ms) =>
            {
                false
            }
            _ => {
                *last_fired = Some(now);
                true
            }
        }
    }

    /// 入队；返回是否入队成功（`on_full = drop_new` 且队满时为 `false`）。
    fn enqueue(&self, event: DispatchInput, capacity: usize) -> bool {
        let mut queue = self.queue.lock().expect("队列锁");
        if queue.len() < capacity {
            queue.push_back(event);
            return true;
        }
        match self.spec.on_full {
            QueuePolicy::DropNew => false,
            QueuePolicy::DropOld => {
                queue.pop_front();
                queue.push_back(event);
                true
            }
            QueuePolicy::CoalesceLatest => {
                queue.clear();
                queue.push_back(event);
                true
            }
        }
    }
}

/// hook 决策内核：规则匹配 → 排序 → cooldown → per-rule 队列/并发 → [`Handler`]。
pub struct Executor {
    handler: Arc<dyn Handler>,
    capacity: usize,
    rules: Vec<Arc<RuleRuntime>>,
}

impl Executor {
    /// 以默认队列容量构造。
    pub fn new(rules: Vec<DispatchRule>, handler: Arc<dyn Handler>) -> Self {
        Self::with_capacity(rules, handler, DEFAULT_QUEUE_CAPACITY)
    }

    /// 以指定 per-rule 队列容量构造。
    pub fn with_capacity(
        mut rules: Vec<DispatchRule>,
        handler: Arc<dyn Handler>,
        capacity: usize,
    ) -> Self {
        rules.sort_by(|a, b| order_key(a).cmp(&order_key(b)));
        let rules = rules
            .into_iter()
            .map(|spec| Arc::new(RuleRuntime::new(spec)))
            .collect();
        Self {
            handler,
            capacity,
            rules,
        }
    }

    /// 分发一个事件（非阻塞：匹配/入队后立即返回，执行在后台任务中完成）。
    pub fn dispatch(&self, input: DispatchInput) {
        for rule in &self.rules {
            if !rule.matched(&input) || !rule.cooldown_allows() {
                continue;
            }
            if rule.spec.parallel {
                let handler = self.handler.clone();
                let spec = rule.spec.clone();
                let event = input.clone();
                tokio::spawn(async move {
                    handler.run(&spec, &event).await;
                });
            } else if rule.enqueue(input.clone(), self.capacity) {
                Self::ensure_worker(rule.clone(), self.handler.clone());
            }
        }
    }

    /// 有界等待规则队列排空（设计 D13 关停 drain）。
    ///
    /// 轮询各规则：**队列为空**且**串行 worker 不在执行**时视为排空，立即返回 `true`；
    /// 超过 `budget` 仍未排空则返回 `false`（调用方另有硬超时兜底）。
    ///
    /// `parallel = true` 的规则执行在游离任务中，无队列水位可观测，故本方法对其
    /// **尽力而为**（可能在其完成前返回）。
    pub async fn drain(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            if self.idle() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(DRAIN_POLL_INTERVAL).await;
        }
    }

    /// 所有规则队列为空且无串行 worker 在执行。
    fn idle(&self) -> bool {
        self.rules.iter().all(|rule| {
            rule.queue.lock().expect("队列锁").is_empty()
                && !rule.worker_active.load(Ordering::Acquire)
        })
    }

    /// 确保串行 worker 在运行（幂等）。
    fn ensure_worker(rule: Arc<RuleRuntime>, handler: Arc<dyn Handler>) {
        if rule.worker_active.swap(true, Ordering::AcqRel) {
            return;
        }
        tokio::spawn(async move { run_worker(rule, handler).await });
    }
}

/// 串行 worker：按到达顺序逐条执行；队列空且确认无新项后退出。
async fn run_worker(rule: Arc<RuleRuntime>, handler: Arc<dyn Handler>) {
    loop {
        let next = rule.queue.lock().expect("队列锁").pop_front();
        match next {
            Some(event) => handler.run(&rule.spec, &event).await,
            None => {
                // 持锁完成「判空 + 置 inactive」：与 enqueue 的 push 互斥，
                // 保证不会漏掉并发入队（若已入队则继续循环，否则退出由入队方负责重启）。
                let queue = rule.queue.lock().expect("队列锁");
                if queue.is_empty() {
                    rule.worker_active.store(false, Ordering::Release);
                    break;
                }
            }
        }
    }
}

/// 测试/dry-run 用的内存处理器：记录调用、统计同规则并发峰值、可延迟/阻塞。
///
/// - [`RecordingHandler::new`]：立即返回。
/// - [`RecordingHandler::with_delay`]：每次调用前 `sleep`。
/// - [`RecordingHandler::blocking`]：每次调用阻塞至 [`RecordingHandler::release`]。
pub struct RecordingHandler {
    records: Mutex<Vec<Record>>,
    count_tx: watch::Sender<usize>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    delay: Duration,
    gate: Option<watch::Sender<bool>>,
}

/// 一次 [`Handler::run`] 调用的记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// 命中规则 id。
    pub rule_id: String,
    /// 触发事件。
    pub event: DispatchInput,
}

impl Default for RecordingHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingHandler {
    /// 立即返回的记录器。
    pub fn new() -> Self {
        Self::build(Duration::ZERO, false)
    }

    /// 每次调用前延迟 `delay` 的记录器。
    pub fn with_delay(delay: Duration) -> Self {
        Self::build(delay, false)
    }

    /// 每次调用阻塞至 [`RecordingHandler::release`] 的记录器。
    pub fn blocking() -> Self {
        Self::build(Duration::ZERO, true)
    }

    fn build(delay: Duration, blocking: bool) -> Self {
        let (count_tx, _) = watch::channel(0usize);
        let gate = blocking.then(|| watch::channel(false).0);
        Self {
            records: Mutex::new(Vec::new()),
            count_tx,
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            delay,
            gate,
        }
    }

    /// 放行所有 [`RecordingHandler::blocking`] 阻塞的调用。
    pub fn release(&self) {
        if let Some(gate) = &self.gate {
            let _ = gate.send(true);
        }
    }

    /// 已记录调用（按调用开始顺序）。
    pub fn records(&self) -> Vec<Record> {
        self.records.lock().expect("记录锁").clone()
    }

    /// 同规则并发峰值（用于 `parallel` 断言）。
    pub fn max_concurrency(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }

    /// 等待至少 `n` 次调用被记录（有超时由调用方自行包裹）。
    pub async fn wait_for(&self, n: usize) {
        let mut rx = self.count_tx.subscribe();
        while *rx.borrow() < n {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }

    async fn wait_gate(&self) {
        if let Some(gate) = &self.gate {
            let mut rx = gate.subscribe();
            while !*rx.borrow() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        }
    }
}

#[async_trait]
impl Handler for RecordingHandler {
    async fn run(&self, rule: &DispatchRule, event: &DispatchInput) {
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(current, Ordering::SeqCst);
        self.records.lock().expect("记录锁").push(Record {
            rule_id: rule.id.clone(),
            event: event.clone(),
        });
        self.count_tx.send_modify(|count| *count += 1);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.wait_gate().await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}
