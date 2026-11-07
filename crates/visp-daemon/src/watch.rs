//! daemon 文件监听模块（设计 §5.6；已迁移至共享模块 `visp-fs`，设计 §4.7）。
//!
//! 分两层：
//! - [`WatchPlan`]：启动时一次性构建的**领域监听计划**（纯构建 + 纯匹配）；
//! - 运行时：把计划翻译为 [`WatchTarget`] 交 `visp-fs` 完成挂载、事件归一化、
//!   缺目录补挂与去重；daemon 侧只保留**领域分类**与**按领域 debounce 聚合**。
//!
//! 计划构建与事件匹配不触碰任何全局状态，测试用 tempdir 构造存在性组合即可覆盖。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;

use visp_core::agent::{AgentEvent, AgentEventFrame};
use visp_core::bus::BusEvent;
use visp_fs::runtime::{DegradeReason, FsWatcher, WatchMessage, start as start_fs};
use visp_fs::target::{FilterRules, Include, WatchMode, WatchTarget};

use crate::bus::EventBus;
use crate::reload::{ReloadCore, ReloadDomain, ReloadItem};

/// 自动重载的默认 debounce 窗口（设计 §7 决策 9）。
///
/// 足以合并编辑器原子写的多事件与「Save All」多文件风暴，同时低于人类的
/// 感知阈值。测试注入更短的窗口，不睡真实 200ms 的整数倍。
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// 监听计划（设计 §5.6 九行表）：路径全集 + 每路径监听模式 + 过滤规则 + 领域映射。
///
/// `entries` 是全部**希望**监听的目录，含启动时尚不存在、运行中补挂的目标；
/// 实际挂载由 [`WatchPlan::fs_targets`] 翻译为 `visp-fs` 目标后交给它执行，
/// daemon 侧只依据 [`WatchPlan::classify`] 做领域映射。
#[derive(Debug, Clone, Default)]
pub struct WatchPlan {
    pub entries: Vec<WatchEntry>,
}

/// 单个监听条目。
#[derive(Debug, Clone)]
pub struct WatchEntry {
    /// 被监听目录（绝对路径）。
    pub dir: PathBuf,
    /// 是否递归监听。
    pub recursive: bool,
    /// 该目录下事件 → 领域的过滤规则。
    pub rules: Vec<WatchRule>,
}

/// 路径过滤规则（设计 §5.6 表格「路径过滤」列）。
#[derive(Debug, Clone)]
pub struct WatchRule {
    pub matcher: Matcher,
    /// 命中时归属的领域；[`Matcher::VispSubtree`] 由子目录名动态决定，此处恒空。
    pub domains: Vec<ReloadDomain>,
}

/// 匹配器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    /// 直接子项文件名/目录名精确等于给定串（非递归条目）。
    Child(String),
    /// 递归子树内扩展名等于给定串（不含点）。
    Extension(String),
    /// 递归子树内任意事件（skills 目标形态 `skills/<name>/SKILL.md`）。
    Any,
    /// project 根下的 `.visp` 子树，按第二层子目录名分流 rules/agents/skills。
    VispSubtree,
}

/// 一次事件分类结果。`domains` 为空表示「事件相关但暂不直接命中领域」，
/// 例如 `.visp` 目录本身创建——补挂由 `visp-fs` 承担，daemon 侧不因此发起重载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub domains: Vec<ReloadDomain>,
}

impl WatchPlan {
    /// 构建监听计划。
    ///
    /// `global_config` 为解析后的全局配置根（`~/.config/visp` 或 `VISP_CONFIG_DIR`）；
    /// 为 `None` 或不存在时跳过全部全局条目。
    pub fn build(project: &Path, global_config: Option<&Path>) -> Self {
        let mut plan = WatchPlan::default();

        // #1 project 向上每个祖先目录（非递归）：命中 AGENTS.md；
        // project 根这一层另加 `.visp` 子树规则，服务 #6 的降级补挂链。
        // 祖先链与加载器共用 `visp_config::agents_md_ancestors`（设计 §5.3），
        // 边界（git 根 / $HOME / 不向上）单点解析，避免两处漂移。
        for ancestor in visp_config::agents_md_ancestors(project) {
            let mut rules = vec![child_rule("AGENTS.md", ReloadDomain::Rules)];
            if ancestor == project {
                rules.push(WatchRule {
                    matcher: Matcher::VispSubtree,
                    domains: Vec::new(),
                });
            }
            plan.push_entry(ancestor, false, rules);
        }

        // #2 全局配置根（非递归）：顶层 AGENTS.md + rules/agents/skills 子目录事件分流。
        if let Some(global) = global_config.filter(|dir| dir.exists()) {
            let rules = vec![
                child_rule("AGENTS.md", ReloadDomain::Rules),
                child_rule("rules", ReloadDomain::Rules),
                child_rule("agents", ReloadDomain::Agents),
                child_rule("skills", ReloadDomain::Skills),
            ];
            plan.push_entry(global.to_path_buf(), false, rules);

            // #3/#4/#5 全局 rules/agents/skills（递归）。
            plan.push_recursive(global.join("rules"), ReloadDomain::Rules, false);
            plan.push_recursive(global.join("agents"), ReloadDomain::Agents, false);
            plan.push_recursive(global.join("skills"), ReloadDomain::Skills, true);
        }

        // #6 项目 `.visp`（非递归）：rules/agents/skills 子目录事件分流。
        let visp = project.join(".visp");
        let visp_rules = vec![
            child_rule("rules", ReloadDomain::Rules),
            child_rule("agents", ReloadDomain::Agents),
            child_rule("skills", ReloadDomain::Skills),
        ];
        plan.push_entry(visp.clone(), false, visp_rules);

        // #7/#8/#9 项目 rules/agents/skills（递归）。
        plan.push_recursive(visp.join("rules"), ReloadDomain::Rules, false);
        plan.push_recursive(visp.join("agents"), ReloadDomain::Agents, false);
        plan.push_recursive(visp.join("skills"), ReloadDomain::Skills, true);

        plan
    }

    /// 将单个事件路径分类为领域集合（纯函数，不做任何 I/O）。
    ///
    /// 返回 `None` 表示事件与计划完全无关（如排除清单内的文件），应当丢弃；
    /// 返回 `Some` 且 `domains` 为空表示事件相关但仅需触发补挂校准。
    pub fn classify(&self, path: &Path) -> Option<Classification> {
        let mut domains: Vec<ReloadDomain> = Vec::new();
        let mut matched = false;
        for entry in &self.entries {
            let Ok(rel) = path.strip_prefix(&entry.dir) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                continue;
            }
            for rule in &entry.rules {
                if let Some(rule_domains) = rule.match_domains(rel) {
                    matched = true;
                    for domain in rule_domains {
                        if !domains.contains(&domain) {
                            domains.push(domain);
                        }
                    }
                }
            }
        }
        matched.then_some(Classification { domains })
    }

    /// 翻译为 `visp-fs` 的目标声明（设计 §4.7：监听计划构建下沉 `visp-fs`）。
    ///
    /// 模式由条目 `recursive` 决定；包含规则由匹配器翻译（[`Matcher::Any`] 表达为
    /// 「空包含集 = 全部包含」）；排除集为空——daemon 的排除语义由「不命中任何
    /// 包含规则」表达。
    pub fn fs_targets(&self) -> Vec<WatchTarget> {
        self.entries
            .iter()
            .map(|entry| WatchTarget {
                root: entry.dir.clone(),
                mode: if entry.recursive {
                    WatchMode::Recursive
                } else {
                    WatchMode::DirectChildren
                },
                filter: FilterRules {
                    includes: includes_for(&entry.rules),
                    excludes: Vec::new(),
                },
            })
            .collect()
    }

    /// 合并写入一个监听条目（同目录去重，递归模式向上取或）。
    fn push_entry(&mut self, dir: PathBuf, recursive: bool, rules: Vec<WatchRule>) {
        if let Some(existing) = self.entries.iter_mut().find(|entry| entry.dir == dir) {
            existing.recursive |= recursive;
            existing.rules.extend(rules);
        } else {
            self.entries.push(WatchEntry {
                dir,
                recursive,
                rules,
            });
        }
    }

    /// 加入一个递归监听的领域目录（rules/agents 用 `.md` 过滤，skills 用任意事件）。
    fn push_recursive(&mut self, dir: PathBuf, domain: ReloadDomain, any: bool) {
        let matcher = if any {
            Matcher::Any
        } else {
            Matcher::Extension("md".to_string())
        };
        self.push_entry(
            dir,
            true,
            vec![WatchRule {
                matcher,
                domains: vec![domain],
            }],
        );
    }
}

/// 汇总一组领域规则的包含条件（去重；空集表示「全部包含」）。
///
/// [`Matcher::VispSubtree`] 对应 `.visp` 子树前缀（仅用于结构性事件命中，
/// 领域分流仍由 daemon 侧 `classify` 决定）。
fn includes_for(rules: &[WatchRule]) -> Vec<Include> {
    // `Any` 语义是「全部包含」，空包含集恰好表达该语义。
    if rules
        .iter()
        .any(|rule| matches!(rule.matcher, Matcher::Any))
    {
        return Vec::new();
    }
    let mut includes: Vec<Include> = Vec::new();
    for rule in rules {
        let include = match &rule.matcher {
            Matcher::Child(name) => Include::FileName(name.clone()),
            Matcher::Extension(ext) => Include::Extension(ext.clone()),
            Matcher::Any => continue,
            Matcher::VispSubtree => Include::Prefix(PathBuf::from(".visp")),
        };
        if !includes.contains(&include) {
            includes.push(include);
        }
    }
    includes
}

impl WatchRule {
    /// 判断相对路径 `rel`（相对所属监听目录）是否命中本规则，命中则返回领域集合。
    fn match_domains(&self, rel: &Path) -> Option<Vec<ReloadDomain>> {
        match &self.matcher {
            Matcher::Child(name) => (rel == Path::new(name)).then(|| self.domains.clone()),
            Matcher::Extension(ext) => (rel.extension().and_then(|value| value.to_str())
                == Some(ext.as_str()))
            .then(|| self.domains.clone()),
            Matcher::Any => Some(self.domains.clone()),
            Matcher::VispSubtree => visp_subtree_domains(rel),
        }
    }
}

/// `.visp` 子树分流：第二层子目录名决定领域；`.visp` 本身无领域（仅补挂）。
///
/// 排除清单（system-prompt.md、daemon.toml、webfetch.toml、logs、codegraph.db
/// 及其余 `.visp` 内容）因不在 rules/agents/skills 之内，自然零命中。
///
/// `hooks` 目录**显式**返回 `None`（不映射领域）：设计 D11 规定 `[hooks]` 配置与
/// `.visp/hooks/` 信任/脚本文件一期**不热重载**，改动需重启 daemon。此处固化该契约，
/// 防止未来 watch 计划扩展时误纳入 hooks。与其它排除项不同，hooks 是预期未来可能
/// 支持热重载的目录，故不依赖「未列入白名单」的隐式排除。
fn visp_subtree_domains(rel: &Path) -> Option<Vec<ReloadDomain>> {
    let mut components = rel.components();
    match components.next() {
        Some(std::path::Component::Normal(name)) if name == ".visp" => {}
        _ => return None,
    }
    match components.next() {
        None => Some(Vec::new()),
        Some(std::path::Component::Normal(name)) => match name.to_str()? {
            "rules" => Some(vec![ReloadDomain::Rules]),
            "agents" => Some(vec![ReloadDomain::Agents]),
            "skills" => Some(vec![ReloadDomain::Skills]),
            // D11：hook 配置/信任文件改动不热重载，显式不映射（需重启）。
            "hooks" => None,
            _ => None,
        },
        Some(_) => None,
    }
}

/// 构造「直接子项名精确匹配」规则。
fn child_rule(name: &str, domain: ReloadDomain) -> WatchRule {
    WatchRule {
        matcher: Matcher::Child(name.to_string()),
        domains: vec![domain],
    }
}

/// 领域重载执行器抽象（设计 §5.6 事件处理管线第 3 步）。
///
/// 真实实现 [`CoreReloadExecutor`] 委托 [`ReloadCore::reload_domains`]；
/// 测试注入假体记录调用，避免依赖真实资产。
#[async_trait::async_trait]
#[allow(clippy::double_must_use)] // async_trait 展开的 Pin<Box<dyn Future>> 触发误报
pub trait ReloadExecutor: Send + Sync + 'static {
    /// 重载给定领域子集，返回逐项结果。
    async fn reload_domains(&self, domains: &[ReloadDomain]) -> Vec<ReloadItem>;
}

/// 委托共享 reload 核心的真实执行器。
pub struct CoreReloadExecutor {
    core: Arc<ReloadCore>,
}

impl CoreReloadExecutor {
    pub fn new(core: Arc<ReloadCore>) -> Self {
        Self { core }
    }
}

#[async_trait::async_trait]
impl ReloadExecutor for CoreReloadExecutor {
    async fn reload_domains(&self, domains: &[ReloadDomain]) -> Vec<ReloadItem> {
        self.core.reload_domains(domains).await
    }
}

/// 文件监听运行时（设计 §5.6；迁移后 §4.7）。
///
/// 挂载、归一化、缺目录补挂、去重由 `visp-fs` 承担；本层消费其消息做领域分类，
/// 并按领域窗口重置式 debounce 聚合后调用 [`ReloadExecutor`]。
pub struct FileWatcher {
    watcher: FsWatcher,
    task: tokio::task::JoinHandle<()>,
}

impl FileWatcher {
    /// 启动监听。
    ///
    /// 把领域监听计划翻译为 `visp-fs` 目标：目标根不存在时由 `visp-fs` 降级监听
    /// 最近存在祖先并在根出现后补挂（设计 G5）；根 / 最近祖先级挂载失败由
    /// `visp-fs` 显式上报降级，本层仅记录，不阻断启动。
    pub async fn start(
        plan: Arc<WatchPlan>,
        executor: Arc<dyn ReloadExecutor>,
        debounce: Duration,
        bus: Option<Arc<EventBus>>,
    ) -> Result<Self, String> {
        let (watcher, messages) = start_fs(plan.fs_targets())
            .await
            .map_err(|error| error.to_string())?;

        let task_plan = plan.clone();
        let task = tokio::spawn(async move {
            run_loop(messages, task_plan, executor, debounce, bus).await;
        });

        Ok(Self { watcher, task })
    }

    /// 停止监听：中止后台任务并释放全部监听。
    pub fn stop(self) {
        self.task.abort();
        self.watcher.stop();
    }
}

/// 文件监听工厂入口（步骤 6d）。
///
/// 开关关闭时返回 `None`——**不构造 watcher、不起后台任务**，自动热重载整体失效、
/// 退回纯显式 `/reload` 形态（设计 §7 决策 12、§9 唯一回滚入口）。
///
/// 开启时按 [`WatchPlan::build`] 构建监听计划并创建 [`FileWatcher`]，执行器由调用方
/// 注入（生产路径传 [`CoreReloadExecutor`]，其委托共享 [`ReloadCore`]，通知句柄已含
/// 于核心的总线通道）。`bus` 为显示面事件总线，用于把细则 5 的降级通知投递到
/// TUI；未注入时静默容忍，不影响启动。watcher 构造失败降级为 `None` + warn，不阻断 daemon 启动。
pub async fn start_file_watcher(
    enabled: bool,
    project: &Path,
    global_config: Option<&Path>,
    executor: Arc<dyn ReloadExecutor>,
    bus: Option<Arc<EventBus>>,
) -> Option<FileWatcher> {
    if !enabled {
        tracing::info!("自动文件监听已关闭（daemon.toml [daemon].filewatcher = false）");
        return None;
    }

    let plan = Arc::new(WatchPlan::build(project, global_config));
    match FileWatcher::start(plan, executor, DEBOUNCE, bus).await {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            tracing::warn!(%error, "文件监听启动失败，自动热重载不可用");
            None
        }
    }
}

/// 重扫信号触发的全量重扫领域集合（设计 §4.2 P2-I：daemon 具备全量重扫能力）。
const FULL_RESCAN_DOMAINS: [ReloadDomain; 3] = [
    ReloadDomain::Rules,
    ReloadDomain::Skills,
    ReloadDomain::Agents,
];

/// 后台任务：窗口重置式 debounce + 领域聚合 + 重扫信号处理。
async fn run_loop(
    mut messages: UnboundedReceiver<WatchMessage>,
    plan: Arc<WatchPlan>,
    executor: Arc<dyn ReloadExecutor>,
    debounce: Duration,
    bus: Option<Arc<EventBus>>,
) {
    while let Some(first) = messages.recv().await {
        let mut domains: Vec<ReloadDomain> = Vec::new();
        let mut rescan_all = false;
        collect(first, &plan, &mut domains, &mut rescan_all, &bus);

        // 开窗：首个事件起算，窗口内每来消息重置计时、合并领域（按领域聚合）。
        let sleep = tokio::time::sleep(debounce);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                biased;
                Some(message) = messages.recv() => {
                    collect(message, &plan, &mut domains, &mut rescan_all, &bus);
                    sleep.as_mut().reset(tokio::time::Instant::now() + debounce);
                }
                _ = &mut sleep => break,
            }
        }

        // 重扫信号（G5）：补挂竞态窗口内的事件可能丢失，且信号不带领域——daemon 属
        // 「收到事件即全量读盘重建」的消费者，故对全部领域做一次全量重扫（幂等）。
        if rescan_all {
            for domain in FULL_RESCAN_DOMAINS {
                if !domains.contains(&domain) {
                    domains.push(domain);
                }
            }
        }

        if !domains.is_empty() {
            executor.reload_domains(&domains).await;
        }
    }
}

/// 把一条 `visp-fs` 消息并入当前 debounce 窗口的领域聚合。
fn collect(
    message: WatchMessage,
    plan: &WatchPlan,
    domains: &mut Vec<ReloadDomain>,
    rescan_all: &mut bool,
    bus: &Option<Arc<EventBus>>,
) {
    match message {
        WatchMessage::Event(event) => {
            if let Some(classification) = plan.classify(&event.path) {
                for domain in classification.domains {
                    if !domains.contains(&domain) {
                        domains.push(domain);
                    }
                }
            }
        }
        WatchMessage::Rescan => *rescan_all = true,
        // 细则 5：根 / 最近祖先级不可挂载——显式记录并 best-effort 通知 TUI，不静默。
        WatchMessage::Degraded(reason) => {
            tracing::warn!(?reason, "文件监听降级：目标目录不可挂载");
            notify_degraded(bus, &reason);
        }
    }
}

/// 细则 5 降级通知：best-effort 发布一条 `session_id` 为空的汇总 Status 帧。
///
/// 与 reload 通知同一来源：`session_id` 置空使 TUI 路由到主 tab 状态行，
/// 不携带任何未知会话；总线未注入 / 无订阅者均静默容忍，不影响启动与运行。
fn notify_degraded(bus: &Option<Arc<EventBus>>, reason: &DegradeReason) {
    let Some(bus) = bus else {
        return;
    };
    let message = format!("文件监听降级：{reason:?}");
    tracing::debug!(%message, "推送文件监听降级通知");
    let frame = AgentEventFrame {
        event: AgentEvent::StatusUpdate(message),
        session_id: String::new(),
        agent_name: String::new(),
        parent_session_id: None,
        parent_session_name: None,
    };
    bus.publish(BusEvent::Frame(frame));
}

#[cfg(test)]
#[path = "watch_tests.rs"]
mod tests;
