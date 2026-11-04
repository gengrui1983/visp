//! watcher 运行时：notify 接线、目录/文件双监听、动态补挂与适配层
//! （设计 §4.3 实现策略、§4.5 接口职责、§4.6 归一化）。
//!
//! 分层：本模块封装对 `notify` 的使用，对消费者只暴露「路径 + 事件类型」的
//! [`WatchMessage`]。纯逻辑（划定范围、归一化、降级链）仍由 sibling 模块提供，
//! 本模块只做「运行时编排 + 后端适配」。
//!
//! 事件在 notify 回调线程上仅做非阻塞投递，真正的挂载/卸载/补挂发生在后台
//! 任务中（从 notify 事件循环之外调用 `watch`/`unwatch`，避免自锁）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use notify::event::{ModifyKind, RenameMode};
use notify::{Event as NotifyEvent, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::degrade::{MountTarget, resolve_degradation};
use crate::normalize::{EventType, RawEvent, normalize};
use crate::target::{WatchMode, WatchTarget};

/// 根 / 最近祖先级挂载失败的有界重试次数（设计 §4.3 细则 5）。
pub(crate) const MAX_MOUNT_ATTEMPTS: usize = 3;

/// 投递给消费者的文件事件（路径 + 类型）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEvent {
    /// 事件路径。
    pub path: PathBuf,
    /// 事件类型。
    pub kind: EventType,
}

/// 运行时投递给消费者的消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchMessage {
    /// 文件事件。
    Event(FileEvent),
    /// 缺目录补挂后的重扫信号（G5）；消费者幂等处理。
    Rescan,
    /// 根 / 最近祖先级挂载失败或不可挂载的**显式降级上报**（细则 5，不静默）。
    Degraded(DegradeReason),
}

/// 降级原因（设计 §4.3 细则 5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DegradeReason {
    /// 目标根 / 最近存在祖先挂载失败（资源耗尽 / 权限），已做有界重试。
    MountFailed {
        /// 挂载失败路径。
        path: PathBuf,
        /// 已尝试次数。
        attempts: usize,
    },
    /// 无任何可挂载祖先：自动监听对该目标不可用。
    Unmountable {
        /// 目标根。
        root: PathBuf,
    },
}

/// 对外可见的监听状态快照（生命周期收敛 / 无句柄泄漏的可观测面）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatcherStatus {
    /// 当前已直接挂载的目录。
    pub dirs: Vec<PathBuf>,
    /// 当前已挂载的文件级路径。
    pub files: Vec<PathBuf>,
    /// 成功发生的目录挂载次数。
    pub dir_mounts: usize,
    /// 成功发生的目录卸载次数。
    pub dir_unmounts: usize,
    /// 成功发生的文件挂载次数。
    pub file_mounts: usize,
    /// 成功发生的文件卸载次数。
    pub file_unmounts: usize,
}

/// 挂载类型：目录递归 / 目录非递归 / 文件级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountKind {
    /// 递归目录。
    DirRecursive,
    /// 非递归目录（直接子级目标的根、缺目录降级挂载点）。
    DirShallow,
    /// 文件级监听（直接子级目标下的匹配文件）。
    File,
}

impl MountKind {
    /// 映射为 notify 的递归模式。
    fn recursive_mode(self) -> RecursiveMode {
        match self {
            MountKind::DirRecursive => RecursiveMode::Recursive,
            _ => RecursiveMode::NonRecursive,
        }
    }
}

/// 挂载后端抽象：生产为 notify，测试可注入假体。
pub(crate) trait MountBackend {
    /// 挂载（目录按 [`MountKind`] 决定递归与否；文件一律非递归）。
    fn watch(&mut self, path: &Path, kind: MountKind) -> Result<(), notify::Error>;
    /// 卸载；`watch_not_found` 由调用方按细则 1 忽略。
    fn unwatch(&mut self, path: &Path, kind: MountKind) -> Result<(), notify::Error>;
}

/// 事件路径别名：将后端报告的 canonical 路径还原为调用方使用的原始拼写。
///
/// macOS fsevent 通过 realpath 报告事件（例如 `/var/folders/...` 会变成
/// `/private/var/folders/...`），而目标与已索引路径使用调用方传入的拼写；
/// 若不还原，`WatchTarget::matches` 与 `strip_prefix` 会全部失配。
#[derive(Debug, Default)]
struct PathAliases {
    /// (canonical 前缀, 原始前缀)，按 canonical 前缀长度降序。
    entries: Vec<(PathBuf, PathBuf)>,
}

impl PathAliases {
    /// 为每个目标根（或其最近存在祖先）登记 canonical → 原始映射。
    fn for_targets(targets: &[WatchTarget]) -> Self {
        let mut aliases = Self::default();
        for target in targets {
            aliases.add(&target.root);
        }
        aliases
            .entries
            .sort_by(|a, b| b.0.components().count().cmp(&a.0.components().count()));
        aliases
    }

    /// 为根或最近存在祖先登记映射；根尚未创建时仍可覆盖后续出现的事件。
    fn add(&mut self, path: &Path) {
        let mut current = Some(path);
        while let Some(candidate) = current {
            if let Ok(canonical) = candidate.canonicalize() {
                if canonical != candidate
                    && !self
                        .entries
                        .iter()
                        .any(|(existing, _)| existing == &canonical)
                {
                    self.entries.push((canonical, candidate.to_path_buf()));
                }
                return;
            }
            current = candidate.parent();
        }
    }

    /// 若 `path` 位于某 canonical 前缀下，则按对应原始前缀重建路径。
    fn restore(&self, path: &Path) -> PathBuf {
        for (canonical, original) in &self.entries {
            if let Ok(rest) = path.strip_prefix(canonical) {
                return original.join(rest);
            }
        }
        path.to_path_buf()
    }
}

impl RawEvent {
    /// 把事件中所有路径还原为调用方拼写（fsevent canonical 路径兼容）。
    fn restore(self, aliases: &PathAliases) -> Self {
        match self {
            RawEvent::Create(path) => RawEvent::Create(aliases.restore(&path)),
            RawEvent::Modify(path) => RawEvent::Modify(aliases.restore(&path)),
            RawEvent::Remove(path) => RawEvent::Remove(aliases.restore(&path)),
            RawEvent::RenameFrom(path) => RawEvent::RenameFrom(aliases.restore(&path)),
            RawEvent::RenameTo(path) => RawEvent::RenameTo(aliases.restore(&path)),
            RawEvent::RenameBoth { from, to } => RawEvent::RenameBoth {
                from: aliases.restore(&from),
                to: aliases.restore(&to),
            },
        }
    }
}

/// `notify::Event` → 自定义 [`RawEvent`] 的适配（备注 13）。
///
/// 后端差异集中于此：
/// - kqueue `RenameMode::Any`（单路径旧）→ [`RawEvent::RenameFrom`]；
/// - inotify `From` / `To` / **`Both`**（单事件旧 + 新）→ 对应三种形态；
/// - Windows 仅 `From` / `To`（**无 `Both`**）。
pub fn adapt(event: &NotifyEvent) -> Vec<RawEvent> {
    let mut out = Vec::new();
    match event.kind {
        EventKind::Create(_) => out.extend(event.paths.iter().cloned().map(RawEvent::Create)),
        EventKind::Remove(_) => out.extend(event.paths.iter().cloned().map(RawEvent::Remove)),
        EventKind::Modify(ModifyKind::Name(mode)) => match mode {
            // kqueue `Any`（单路径旧）/ inotify·Windows `From` / 未知 `Other`：旧路径。
            RenameMode::From | RenameMode::Any | RenameMode::Other => {
                if let Some(path) = event.paths.first() {
                    out.push(RawEvent::RenameFrom(path.clone()));
                }
            }
            RenameMode::To => {
                if let Some(path) = event.paths.first() {
                    out.push(RawEvent::RenameTo(path.clone()));
                }
            }
            RenameMode::Both => {
                if let (Some(from), Some(to)) = (event.paths.first(), event.paths.get(1)) {
                    out.push(RawEvent::RenameBoth {
                        from: from.clone(),
                        to: to.clone(),
                    });
                }
            }
        },
        // 内容写入 / 元数据变更 / `ModifyKind::Any`（kqueue `Vnode::Link`）：保守「修改」。
        EventKind::Modify(_) => out.extend(event.paths.iter().cloned().map(RawEvent::Modify)),
        // `EventKind::Any` 为不精确后端的 catch-all，保守按「修改」处理。
        EventKind::Any => out.extend(event.paths.iter().cloned().map(RawEvent::Modify)),
        // Access（开闭/读取）不在「创建/修改/删除」契约内；Other 由 `need_rescan` 另行处理。
        EventKind::Access(_) | EventKind::Other => {}
    }
    out
}

/// `watch_not_found` 判定（细则 1：notify 在文件被删时已自动移除 watch，二次卸载须忽略）。
pub(crate) fn is_watch_not_found(error: &notify::Error) -> bool {
    matches!(error.kind, notify::ErrorKind::WatchNotFound)
}

/// 生产挂载后端：包装共享的 notify watcher，并累计挂载/卸载次数。
struct NotifyBackend {
    watcher: Arc<StdMutex<RecommendedWatcher>>,
    status: Arc<StdMutex<WatcherStatus>>,
}

impl MountBackend for NotifyBackend {
    fn watch(&mut self, path: &Path, kind: MountKind) -> Result<(), notify::Error> {
        let result = self
            .watcher
            .lock()
            .unwrap()
            .watch(path, kind.recursive_mode());
        if result.is_ok() {
            let mut status = self.status.lock().unwrap();
            match kind {
                MountKind::File => status.file_mounts += 1,
                _ => status.dir_mounts += 1,
            }
        }
        result
    }

    fn unwatch(&mut self, path: &Path, kind: MountKind) -> Result<(), notify::Error> {
        let result = self.watcher.lock().unwrap().unwatch(path);
        if result.is_ok() {
            let mut status = self.status.lock().unwrap();
            match kind {
                MountKind::File => status.file_unmounts += 1,
                _ => status.dir_unmounts += 1,
            }
        }
        result
    }
}

/// 运行时核心：维护监听集合并把后端事件归一化投递。
///
/// 泛型后端与存在性谓词便于以假体注入做确定性的细则单测（计划 2b）。
struct Core<B, E> {
    targets: Vec<WatchTarget>,
    backend: B,
    exists: E,
    /// 对外可见状态快照（监听集合 + 挂载计数）。
    status: Arc<StdMutex<WatcherStatus>>,
    /// 已直接挂载的目录 → 挂载类型（用于 Modify 抑制与卸载）。
    dirs: HashMap<PathBuf, MountKind>,
    /// 已挂载的文件级路径。
    files: HashSet<PathBuf>,
    /// 已上报降级的路径（去重；成功恢复后移除，允许再次上报）。
    degraded: HashSet<PathBuf>,
    /// 已知「根已直接挂载」的目标根，用于 G5 重扫信号去重。
    known_direct: HashSet<PathBuf>,
    /// 首次 reconcile（启动挂载）不计重扫信号。
    initialized: bool,
}

impl<B: MountBackend, E: Fn(&Path) -> bool> Core<B, E> {
    fn new(
        targets: Vec<WatchTarget>,
        backend: B,
        exists: E,
        status: Arc<StdMutex<WatcherStatus>>,
    ) -> Self {
        Self {
            targets,
            backend,
            exists,
            status,
            dirs: HashMap::new(),
            files: HashSet::new(),
            degraded: HashSet::new(),
            known_direct: HashSet::new(),
            initialized: false,
        }
    }

    /// 已直接挂载目录集合（Modify 抑制的判定依据）。
    fn dir_path_set(&self) -> HashSet<PathBuf> {
        self.dirs.keys().cloned().collect()
    }

    /// 路径是否命中任一目标。
    fn matches_any(&self, path: &Path) -> bool {
        self.targets.iter().any(|target| target.matches(path))
    }

    /// 路径是否属于某直接子级目标的直接子项范围。
    fn is_direct_child_scope(&self, path: &Path) -> bool {
        self.targets
            .iter()
            .any(|target| target.mode == WatchMode::DirectChildren && target.matches(path))
    }

    /// 是否为文件级重挂目标（Create / Rename 新名路径的幂等重挂范围）：
    /// 已监听文件，或直接子级范围内的现存文件。递归目标的普通文件不由此处补挂
    /// （交由 notify 递归能力），仅细则 2a 的恢复路径会为其补文件级监听。
    fn is_remount_target(&self, path: &Path) -> bool {
        self.files.contains(path) || (self.is_direct_child_scope(path) && path.is_file())
    }

    /// 细则 2a 的恢复判定：Remove 命中**仍存在**的路径 ⇒ 覆盖同名已监听路径的
    /// 原子替换（tmp+rename）⇒ 须重挂该路径并补发「创建」。
    ///
    /// **不限目标模式**：递归目标下 notify 的文件级监听随旧 inode 失效，且 kqueue
    /// 目录 diff 不会为仍在 watch 集合内的同名路径补发 Create，故递归目标同样
    /// 必须由本探测恢复（否则随后该路径的原地编辑全部漏检）。
    fn should_recover(&self, path: &Path) -> bool {
        self.files.contains(path) || (self.matches_any(path) && path.is_file())
    }

    /// 处理单个后端原始事件：细则 2a 探测 + 归一化 + 必要重挂 + 过滤投递。
    fn process(&mut self, raw: RawEvent) -> Vec<WatchMessage> {
        let mut messages = Vec::new();

        // macOS fsevent 的 `RenameMode::Any` 无法区分旧/新名：重命名会先后报告
        // 两个单路径事件（旧名、新名），且都归为 `RenameFrom`。旧名此时已不存在，
        // 而新名仍存在；按存在性把后者还原为 `RenameTo`，否则新文件会被误判为删除。
        let raw = match raw {
            RawEvent::RenameFrom(path) if (self.exists)(&path) => RawEvent::RenameTo(path),
            other => other,
        };

        // 细则 2a（必需主路径）：Remove 命中**仍存在**的路径 ⇒ 覆盖同名已监听路径的
        // 原子替换（tmp+rename）⇒ 立即重挂 + 投递「创建」。kqueue 的目录 diff 不会
        // 补发 Create（该路径仍在 watch 集合内），故这是 kqueue 原子替换的必需恢复路径。
        if let RawEvent::Remove(path) = &raw
            && (self.exists)(path)
            && self.should_recover(path)
        {
            self.remount_file(path);
            if self.matches_any(path) {
                messages.push(WatchMessage::Event(FileEvent {
                    path: path.clone(),
                    kind: EventType::Created,
                }));
            }
            return messages;
        }

        for (path, kind) in normalize(&raw, &self.dir_path_set()) {
            // Create / Rename（新名）涉及的直接子文件：无条件「先卸后挂」（幂等；细则 1/3）。
            if kind == EventType::Created && self.is_remount_target(&path) {
                self.remount_file(&path);
            }
            if self.matches_any(&path) {
                messages.push(WatchMessage::Event(FileEvent { path, kind }));
            }
        }
        messages
    }

    /// 对单个文件无条件「先卸后挂」（幂等；幂等目标是不产生重复状态而非跳过重挂）。
    /// 对单个文件无条件「先卸后挂」：幂等目标是**不产生重复状态**，而非跳过必要重挂。
    ///
    /// 细则 1：卸载报 `watch_not_found` 忽略（notify 已自动移除）；
    /// 细则 4：单文件级挂载失败 → 忽略 + 不入集合 + warn（待后续 Create 恢复）。
    fn remount_file(&mut self, path: &Path) {
        if let Err(error) = self.backend.unwatch(path, MountKind::File) {
            // 细则 1：`watch_not_found` 为常态（notify 已自动移除），静默忽略。
            if !is_watch_not_found(&error) {
                tracing::debug!(path = %path.display(), %error, "卸载文件监听失败（忽略）");
            }
        }
        match self.backend.watch(path, MountKind::File) {
            Ok(()) => {
                self.files.insert(path.to_path_buf());
            }
            Err(error) => {
                self.files.remove(path);
                tracing::warn!(path = %path.display(), %error, "文件级监听重挂失败，忽略");
            }
        }
    }

    /// 收敛目录/文件监听集合、执行缺目录补挂，并产出重扫信号。
    fn reconcile(&mut self) -> Vec<WatchMessage> {
        let mut messages = Vec::new();

        // 期望的目录挂载：降级解析 + 模式映射。
        let mut desired: HashMap<PathBuf, MountKind> = HashMap::new();
        for target in &self.targets {
            match resolve_degradation(&target.root, &self.exists).mount {
                MountTarget::Direct(root) => {
                    let kind = match target.mode {
                        WatchMode::Recursive => MountKind::DirRecursive,
                        WatchMode::DirectChildren => MountKind::DirShallow,
                    };
                    merge_dir(&mut desired, root, kind);
                }
                MountTarget::Fallback { ancestor, .. } => {
                    merge_dir(&mut desired, ancestor, MountKind::DirShallow);
                }
                // 细则 5：无任何可挂载祖先 ⇒ 终止并显式上报「自动监听不可用」。
                MountTarget::Unmountable => {
                    if self.degraded.insert(target.root.clone()) {
                        messages.push(WatchMessage::Degraded(DegradeReason::Unmountable {
                            root: target.root.clone(),
                        }));
                    }
                }
            }
        }

        // 卸载不再需要的目录。
        let stale: Vec<PathBuf> = self
            .dirs
            .keys()
            .filter(|path| !desired.contains_key(*path))
            .cloned()
            .collect();
        for path in stale {
            let kind = self.dirs[&path];
            let _ = self.backend.unwatch(&path, kind);
            self.dirs.remove(&path);
        }

        // 挂载 / 重挂期望目录（目录重挂仅在模式变化或新出现时发生）。
        let mut entries: Vec<(PathBuf, MountKind)> = desired.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, kind) in entries {
            if self.dirs.get(&path) == Some(&kind) {
                continue;
            }
            if self.dirs.contains_key(&path) {
                let old = self.dirs[&path];
                let _ = self.backend.unwatch(&path, old);
                self.dirs.remove(&path);
            }
            match self.mount_dir_with_retry(&path, kind) {
                Ok(()) => {
                    self.dirs.insert(path.clone(), kind);
                    // 挂载成功即视为恢复，允许未来再次上报降级。
                    self.degraded.remove(&path);
                }
                Err(attempts) => {
                    // 细则 5：根 / 最近祖先级失败 ⇒ 显式上报降级（不静默）+ 有界重试。
                    if self.degraded.insert(path.clone()) {
                        messages.push(WatchMessage::Degraded(DegradeReason::MountFailed {
                            path,
                            attempts,
                        }));
                    }
                }
            }
        }

        // 直接子级目标下的文件级监听收敛。
        let desired_files = self.desired_files();
        let stale_files: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|path| !desired_files.contains(*path))
            .cloned()
            .collect();
        for path in stale_files {
            let _ = self.backend.unwatch(&path, MountKind::File);
            self.files.remove(&path);
        }
        let mut new_files: Vec<PathBuf> = desired_files
            .into_iter()
            .filter(|path| !self.files.contains(path))
            .collect();
        new_files.sort();
        for path in new_files {
            match self.backend.watch(&path, MountKind::File) {
                Ok(()) => {
                    self.files.insert(path);
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "文件级监听挂载失败，忽略");
                }
            }
        }

        // G5：目标根从「未挂载」变为「已直接挂载」⇒ 一次重扫信号（启动首挂不计）。
        let now_direct: HashSet<PathBuf> = self
            .targets
            .iter()
            .filter(|target| (self.exists)(&target.root) && self.dirs.contains_key(&target.root))
            .map(|target| target.root.clone())
            .collect();
        if self.initialized {
            let mut appeared: Vec<PathBuf> = now_direct
                .iter()
                .filter(|root| !self.known_direct.contains(*root))
                .cloned()
                .collect();
            appeared.sort();
            messages.extend(appeared.into_iter().map(|_| WatchMessage::Rescan));
        }
        self.known_direct = now_direct;

        // 更新对外可见的监听集合快照。
        {
            let mut status = self.status.lock().unwrap();
            status.dirs = self.dirs.keys().cloned().collect();
            status.dirs.sort();
            status.files = self.files.iter().cloned().collect();
            status.files.sort();
        }

        messages
    }

    /// 初始 reconcile 之后仍「根存在但未挂载」的目标根。
    ///
    /// kqueue 递归挂载中途失败（EMFILE / 权限）时，notify 会把已打开的 fd 留在
    /// watcher 内部；若静默返回半成品 watcher，这些 fd 将永久泄漏。启动阶段据此
    /// 判定是否需要放弃并销毁 watcher。
    fn failed_initial_roots(&self) -> Vec<PathBuf> {
        self.targets
            .iter()
            .filter(|target| (self.exists)(&target.root) && !self.dirs.contains_key(&target.root))
            .map(|target| target.root.clone())
            .collect()
    }

    /// 有界重试挂载目录（细则 5）；返回最终尝试次数作为 `Err`。
    fn mount_dir_with_retry(&mut self, path: &Path, kind: MountKind) -> Result<(), usize> {
        for attempt in 0..MAX_MOUNT_ATTEMPTS {
            if self.backend.watch(path, kind).is_ok() {
                return Ok(());
            }
            tracing::warn!(path = %path.display(), attempt = attempt + 1, "目录监听挂载失败，重试");
        }
        Err(MAX_MOUNT_ATTEMPTS)
    }

    /// 直接子级目标下当前存在的匹配文件集合，并保留细则 2a 恢复的递归目标文件。
    ///
    /// 保留项判定为「仍存在且命中任一目标」：使 2a 为重挂单个文件（而非整棵递归根）
    /// 补上的文件级监听不会被随后的收敛误判为 stale 而卸载。
    fn desired_files(&self) -> HashSet<PathBuf> {
        let mut files = HashSet::new();
        for target in &self.targets {
            if target.mode != WatchMode::DirectChildren || !(self.exists)(&target.root) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&target.root) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let is_file = entry
                    .file_type()
                    .map(|kind| kind.is_file())
                    .unwrap_or(false);
                if is_file && target.matches(&path) {
                    files.insert(path);
                }
            }
        }
        for path in &self.files {
            if (self.exists)(path) && self.matches_any(path) {
                files.insert(path.clone());
            }
        }
        files
    }
}

/// 合并目录挂载需求；同路径递归优先。
fn merge_dir(map: &mut HashMap<PathBuf, MountKind>, path: PathBuf, kind: MountKind) {
    map.entry(path)
        .and_modify(|existing| {
            if kind == MountKind::DirRecursive {
                *existing = kind;
            }
        })
        .or_insert(kind);
}

/// 文件监听句柄（设计 §4.5）。
pub struct FsWatcher {
    watcher: Arc<StdMutex<RecommendedWatcher>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    status: Arc<StdMutex<WatcherStatus>>,
}

impl FsWatcher {
    /// 当前监听状态快照（监听集合 + 挂载计数）。
    pub fn status(&self) -> WatcherStatus {
        self.status.lock().unwrap().clone()
    }

    /// 停止监听：通知后台任务退出，并主动卸载全部已跟踪监听后释放 watcher。
    ///
    /// 显式卸载使 OS 句柄确定性回收（不依赖后台任务被调度析构的时机）。
    pub fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let snapshot = self.status();
        if let Ok(mut watcher) = self.watcher.lock() {
            for path in snapshot.dirs.iter().chain(snapshot.files.iter()) {
                let _ = watcher.unwatch(path);
            }
        }
    }
}

/// 启动监听：返回句柄与消息接收端（设计 §4.5）。
///
/// 目标根不存在时按 G5 自动挂载最近存在祖先；根出现后补挂并发重扫信号。
/// 需在 tokio 运行时内调用（后台任务由 `tokio::spawn` 驱动）。
pub async fn start(
    targets: Vec<WatchTarget>,
) -> Result<(FsWatcher, UnboundedReceiver<WatchMessage>), notify::Error> {
    // fsevent 以 realpath 报告事件；提前记录 canonical → 原始拼写映射。
    let aliases = PathAliases::for_targets(&targets);
    let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel::<NotifyEvent>();
    let watcher = notify::recommended_watcher(
        move |result: Result<NotifyEvent, notify::Error>| match result {
            Ok(event) => {
                let _ = raw_tx.send(event);
            }
            Err(error) => tracing::warn!(%error, "文件监听回调错误"),
        },
    )?;
    let watcher = Arc::new(StdMutex::new(watcher));
    let status = Arc::new(StdMutex::new(WatcherStatus::default()));

    let backend = NotifyBackend {
        watcher: watcher.clone(),
        status: status.clone(),
    };
    let mut core = Core::new(
        targets,
        backend,
        |path: &Path| path.exists(),
        status.clone(),
    );

    let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel::<WatchMessage>();
    // 启动时一次性挂载（同步完成，先于任何事件）。
    let initial_messages = core.reconcile();
    if let Some(root) = core.failed_initial_roots().into_iter().next() {
        // 根存在却挂载失败（典型：EMFILE）。notify 可能已在 watcher 内留下部分 fd；
        // 立即销毁核心与 watcher（释放全部 fd），绝不返回半成品监听器。
        drop(core);
        drop(watcher);
        return Err(notify::Error::new(notify::ErrorKind::Generic(format!(
            "initial mount failed for {} (resource exhausted?); watcher released to avoid fd leak",
            root.display()
        ))));
    }
    for message in initial_messages {
        let _ = out_tx.send(message);
    }
    core.initialized = true;

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => break,
                event = raw_rx.recv() => {
                    let Some(event) = event else { break };
                    if event.need_rescan() && out_tx.send(WatchMessage::Rescan).is_err() {
                        return;
                    }
                    let mut messages: Vec<WatchMessage> = Vec::new();
                    for raw in adapt(&event) {
                        messages.extend(core.process(raw.restore(&aliases)));
                    }
                    messages.extend(core.reconcile());
                    for message in messages {
                        if out_tx.send(message).is_err() {
                            // 接收端被 drop：静默停止投递（设计 §4.5）。
                            return;
                        }
                    }
                }
            }
        }
    });

    Ok((
        FsWatcher {
            watcher,
            shutdown: Some(shutdown_tx),
            task: Some(task),
            status,
        },
        out_rx,
    ))
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
