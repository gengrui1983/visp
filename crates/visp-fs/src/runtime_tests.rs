//! watcher 运行时测试（计划 2a，7 例 + 适配层单测）。
//!
//! 集成用例**后端无关**：只断言「有限时间轮询的最终收敛」，不含 kqueue 特化断言，
//! 可在 Linux（inotify）与 macOS（kqueue）双方言跑。
//! 关键前提：验收文件的**在监听启动之前已存在**（否则 kqueue 目录 diff 会自动补挂，
//! 让缺陷实现也通过——虚假覆盖）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::tempdir;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{Instant, timeout};

use crate::normalize::RawEvent;
use crate::target::{FilterRules, Include, WatchTarget};

use super::*;

/// 构造直接子级目标。
fn direct(root: &Path, includes: Vec<Include>, excludes: &[&str]) -> WatchTarget {
    WatchTarget {
        root: root.to_path_buf(),
        mode: WatchMode::DirectChildren,
        filter: FilterRules {
            includes,
            excludes: excludes.iter().map(|s| (*s).to_string()).collect(),
        },
    }
}

/// 构造递归目标。
fn recursive(root: &Path, includes: Vec<Include>, excludes: &[&str]) -> WatchTarget {
    WatchTarget {
        root: root.to_path_buf(),
        mode: WatchMode::Recursive,
        filter: FilterRules {
            includes,
            excludes: excludes.iter().map(|s| (*s).to_string()).collect(),
        },
    }
}

/// 事件谓词。
fn is_event(message: &WatchMessage, path: &Path, kind: EventType) -> bool {
    matches!(
        message,
        WatchMessage::Event(FileEvent { path: p, kind: k }) if p == path && *k == kind
    )
}

/// 在给定时限内轮询，直到出现满足谓词的消息。
async fn wait_event<F>(rx: &mut UnboundedReceiver<WatchMessage>, dur: Duration, pred: F) -> bool
where
    F: Fn(&WatchMessage) -> bool,
{
    let deadline = Instant::now() + dur;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match timeout(remaining, rx.recv()).await {
            Ok(Some(message)) => {
                if pred(&message) {
                    return true;
                }
            }
            Ok(None) | Err(_) => return false,
        }
    }
}

// ── 集成用例（后端无关） ──────────────────────────────────────────────

/// 2a#1 核心修复项：启动前已存在文件的原地覆写 → 「修改」。
#[tokio::test]
async fn existing_file_overwrite_emits_modified() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("AGENTS.md");
    std::fs::write(&file, "before").unwrap(); // 启动前已存在

    let target = direct(dir.path(), vec![Include::FileName("AGENTS.md".into())], &[]);
    let (watcher, mut rx) = start(vec![target]).await.unwrap();

    std::fs::write(&file, "after").unwrap(); // 原地覆写

    assert!(
        wait_event(&mut rx, Duration::from_secs(5), |m| is_event(
            m,
            &file,
            EventType::Modified
        ))
        .await,
        "启动前已存在文件的原地覆写必须产生修改事件"
    );
    watcher.stop();
}

/// 2a#2 新建文件 → 「创建」。
#[tokio::test]
async fn new_file_emits_created() {
    let dir = tempdir().unwrap();
    let target = direct(dir.path(), vec![Include::Extension("md".into())], &[]);
    let (watcher, mut rx) = start(vec![target]).await.unwrap();

    let file = dir.path().join("new.md");
    std::fs::write(&file, "hello").unwrap();

    assert!(
        wait_event(&mut rx, Duration::from_secs(5), |m| is_event(
            m,
            &file,
            EventType::Created
        ))
        .await,
        "新建文件应产生创建事件"
    );
    watcher.stop();
}

/// 2a#3 删除文件 → 「删除」。
#[tokio::test]
async fn removed_file_emits_removed() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("gone.md");
    std::fs::write(&file, "x").unwrap();

    let target = direct(dir.path(), vec![Include::Extension("md".into())], &[]);
    let (watcher, mut rx) = start(vec![target]).await.unwrap();

    std::fs::remove_file(&file).unwrap();

    assert!(
        wait_event(&mut rx, Duration::from_secs(5), |m| is_event(
            m,
            &file,
            EventType::Removed
        ))
        .await,
        "删除文件应产生删除事件"
    );
    watcher.stop();
}

/// 2a#4 直接子级模式下的子目录创建须上报（G5 / daemon #6 依赖）。
#[tokio::test]
async fn direct_child_subdir_creation_is_reported() {
    let dir = tempdir().unwrap();
    let target = direct(dir.path(), vec![], &[]);
    let (watcher, mut rx) = start(vec![target]).await.unwrap();

    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();

    assert!(
        wait_event(&mut rx, Duration::from_secs(5), |m| is_event(
            m,
            &sub,
            EventType::Created
        ))
        .await,
        "直接子级模式必须上报子目录创建"
    );
    watcher.stop();
}

/// 2a#5 动态补挂：目标目录不存在 → 运行中创建 → 重扫信号 + 事件。
#[tokio::test]
async fn missing_target_dir_is_dynamically_attached() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("watched");
    let target = direct(&missing, vec![], &[]);
    let (watcher, mut rx) = start(vec![target]).await.unwrap();

    std::fs::create_dir(&missing).unwrap();
    assert!(
        wait_event(&mut rx, Duration::from_secs(5), |m| matches!(
            m,
            WatchMessage::Rescan
        ))
        .await,
        "缺目录补挂应发出一次重扫信号"
    );

    let file = missing.join("hello.md");
    std::fs::write(&file, "x").unwrap();
    assert!(
        wait_event(&mut rx, Duration::from_secs(5), |m| is_event(
            m,
            &file,
            EventType::Created
        ))
        .await,
        "补挂后写入应产生事件"
    );
    watcher.stop();
}

/// 2a#6 递归目标下子树既有文件覆写 → 「修改」。
#[tokio::test]
async fn recursive_target_overwrite_in_subtree_emits_modified() {
    let dir = tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let file = sub.join("a.md");
    std::fs::write(&file, "before").unwrap();

    let target = recursive(dir.path(), vec![Include::Extension("md".into())], &[]);
    let (watcher, mut rx) = start(vec![target]).await.unwrap();

    std::fs::write(&file, "after").unwrap();

    assert!(
        wait_event(&mut rx, Duration::from_secs(5), |m| is_event(
            m,
            &file,
            EventType::Modified
        ))
        .await,
        "递归目标下子树既有文件覆写应产生修改事件"
    );
    watcher.stop();
}

/// 2a#7 过滤生效：未命中包含规则的事件不下发。
#[tokio::test]
async fn non_matching_events_are_filtered() {
    let dir = tempdir().unwrap();
    let target = direct(
        dir.path(),
        vec![Include::Extension("md".into())],
        &["node_modules"],
    );
    let (watcher, mut rx) = start(vec![target]).await.unwrap();

    // 无关扩展名：不命中包含规则。
    std::fs::write(dir.path().join("ignore.txt"), "x").unwrap();

    let got = timeout(Duration::from_millis(600), rx.recv()).await;
    assert!(got.is_err(), "未命中过滤规则的事件不应下发，却收到 {got:?}");
    watcher.stop();
}

// ── 适配层单测（备注 13：notify::Event → RawEvent） ──────────────────

fn notify_event(kind: EventKind, paths: &[&str]) -> NotifyEvent {
    let mut event = NotifyEvent::new(kind);
    for path in paths {
        event = event.add_path(PathBuf::from(path));
    }
    event
}

#[test]
fn adapt_create_and_remove() {
    use notify::event::{CreateKind, RemoveKind};
    assert_eq!(
        adapt(&notify_event(
            EventKind::Create(CreateKind::File),
            &["/p/a.md"]
        )),
        vec![RawEvent::Create("/p/a.md".into())]
    );
    assert_eq!(
        adapt(&notify_event(
            EventKind::Remove(RemoveKind::File),
            &["/p/a.md"]
        )),
        vec![RawEvent::Remove("/p/a.md".into())]
    );
}

#[test]
fn adapt_data_and_metadata_modify() {
    use notify::event::{DataChange, MetadataKind};
    assert_eq!(
        adapt(&notify_event(
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            &["/p/a.md"]
        )),
        vec![RawEvent::Modify("/p/a.md".into())]
    );
    assert_eq!(
        adapt(&notify_event(
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
            &["/p/a.md"]
        )),
        vec![RawEvent::Modify("/p/a.md".into())]
    );
}

#[test]
fn adapt_kqueue_rename_any_uses_old_path() {
    assert_eq!(
        adapt(&notify_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            &["/p/old.md"]
        )),
        vec![RawEvent::RenameFrom("/p/old.md".into())]
    );
}

#[test]
fn adapt_inotify_from_to() {
    assert_eq!(
        adapt(&notify_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            &["/p/old.md"]
        )),
        vec![RawEvent::RenameFrom("/p/old.md".into())]
    );
    assert_eq!(
        adapt(&notify_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            &["/p/new.md"]
        )),
        vec![RawEvent::RenameTo("/p/new.md".into())]
    );
}

#[test]
fn adapt_inotify_both_splits_from_and_to() {
    assert_eq!(
        adapt(&notify_event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &["/p/old.md", "/p/new.md"]
        )),
        vec![RawEvent::RenameBoth {
            from: "/p/old.md".into(),
            to: "/p/new.md".into(),
        }]
    );
}

#[test]
fn adapt_access_and_other_are_ignored() {
    use notify::event::{AccessKind, AccessMode};
    assert!(
        adapt(&notify_event(
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            &["/p/a.md"]
        ))
        .is_empty()
    );
    assert!(adapt(&notify_event(EventKind::Other, &["/p/a.md"])).is_empty());
}

// ── 细则单测（计划 2b；假体注入，后端无关） ──────────────────────────

/// 假体挂载后端：记录调用并可按路径注入失败，避免依赖真实 notify 与 OS 权限。
#[derive(Default)]
struct FakeBackend {
    /// 注入 watch 失败的路径集合。
    fail_watch: HashSet<PathBuf>,
    /// unwatch 是否一律报 `watch_not_found`（细则 1）。
    watch_not_found_on_unwatch: bool,
    /// 已发生的 watch 调用。
    watch_calls: Vec<(PathBuf, MountKind)>,
    /// 已发生的 unwatch 调用。
    unwatch_calls: Vec<PathBuf>,
}

impl MountBackend for FakeBackend {
    fn watch(&mut self, path: &Path, kind: MountKind) -> Result<(), notify::Error> {
        self.watch_calls.push((path.to_path_buf(), kind));
        if self.fail_watch.contains(path) {
            Err(notify::Error::generic("injected watch failure"))
        } else {
            Ok(())
        }
    }

    fn unwatch(&mut self, path: &Path, _kind: MountKind) -> Result<(), notify::Error> {
        self.unwatch_calls.push(path.to_path_buf());
        if self.watch_not_found_on_unwatch {
            Err(notify::Error::watch_not_found())
        } else {
            Ok(())
        }
    }
}

/// 以假体后端构造 Core，存在性由内存集合注入。
fn fake_core(
    targets: Vec<WatchTarget>,
    exists: HashSet<PathBuf>,
) -> Core<FakeBackend, impl Fn(&Path) -> bool> {
    Core::new(
        targets,
        FakeBackend::default(),
        move |path: &Path| exists.contains(path),
        Arc::new(StdMutex::new(WatcherStatus::default())),
    )
}

/// 2b#1 原子替换后仍可感知（细则 2a 的直接验收）。
#[tokio::test]
async fn atomic_replace_then_in_place_edit_is_still_observed() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("AGENTS.md");
    std::fs::write(&file, "v1").unwrap(); // 启动前已存在且已监听

    let target = direct(dir.path(), vec![Include::FileName("AGENTS.md".into())], &[]);
    let (watcher, mut rx) = start(vec![target]).await.unwrap();

    // 原子替换：tmp + rename 覆盖同名已监听路径。
    let tmp = dir.path().join("AGENTS.md.tmp");
    std::fs::write(&tmp, "v2").unwrap();
    std::fs::rename(&tmp, &file).unwrap();

    // kqueue 目录 diff 不会补发 Create，须由细则 2a 的存在性探测恢复。
    assert!(
        wait_event(&mut rx, Duration::from_secs(5), |m| is_event(
            m,
            &file,
            EventType::Created
        ))
        .await,
        "原子替换应由细则 2a 恢复并投递创建事件"
    );

    // 再原地覆写 → 仍收到「修改」。
    std::fs::write(&file, "v3").unwrap();
    assert!(
        wait_event(&mut rx, Duration::from_secs(5), |m| is_event(
            m,
            &file,
            EventType::Modified
        ))
        .await,
        "原子替换后原地覆写仍须可感知"
    );
    watcher.stop();
}

/// 修正：细则 2a 的存在性探测须覆盖**递归目标**。
///
/// 递归子树内、启动前已存在且已监听的文件被 tmp+rename 原子替换后，须补发
/// 「创建」并把该路径重挂为文件级监听，使随后的原地覆写仍可感知。旧实现对
/// 该探测仅门控文件级监听，递归目标下新 inode 不被监听 → 后续原地覆写漏检。
#[tokio::test]
async fn recursive_target_atomic_replace_then_in_place_edit_is_still_observed() {
    let dir = tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let file = sub.join("a.md");
    std::fs::write(&file, "v1").unwrap(); // 启动前已存在（递归监听覆盖）

    let target = recursive(dir.path(), vec![Include::Extension("md".into())], &[]);
    let (watcher, mut rx) = start(vec![target]).await.unwrap();

    // tmp + rename 覆盖同名已监听路径（原子替换）。
    let tmp = sub.join("a.md.tmp");
    std::fs::write(&tmp, "v2").unwrap();
    std::fs::rename(&tmp, &file).unwrap();

    assert!(
        wait_event(&mut rx, Duration::from_secs(5), |m| is_event(
            m,
            &file,
            EventType::Created
        ))
        .await,
        "递归目标下原子替换须由细则 2a 补发创建事件"
    );

    // 随后原地覆写 → 仍收到「修改」（新 inode 已被细则 2a 重挂）。
    std::fs::write(&file, "v3").unwrap();
    assert!(
        wait_event(&mut rx, Duration::from_secs(5), |m| is_event(
            m,
            &file,
            EventType::Modified
        ))
        .await,
        "递归目标下原子替换后原地覆写仍须可感知"
    );
    watcher.stop();
}

/// 2b#2 细则 1：`watch_not_found` 忽略。
#[test]
fn watch_not_found_is_recognized_and_ignored() {
    assert!(is_watch_not_found(&notify::Error::watch_not_found()));
    assert!(!is_watch_not_found(&notify::Error::generic("other")));

    let dir = tempdir().unwrap();
    let file = dir.path().join("a.md");
    std::fs::write(&file, "x").unwrap();
    let target = direct(dir.path(), vec![Include::Extension("md".into())], &[]);
    let mut core = fake_core(
        vec![target],
        [dir.path().to_path_buf()].into_iter().collect(),
    );
    // 二次卸载报 watch_not_found：不得中断重挂。
    core.backend.watch_not_found_on_unwatch = true;
    core.files.insert(file.clone());
    core.process(RawEvent::Create(file.clone()));

    assert!(
        core.files.contains(&file),
        "watch_not_found 应被忽略且重挂成功"
    );
}

/// 2b#3 幂等重挂：已挂路径的 Create 无条件「先卸后挂」，状态不重复、不丢失。
#[test]
fn remount_of_mounted_path_is_idempotent() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("a.md");
    std::fs::write(&file, "x").unwrap();
    let target = direct(dir.path(), vec![Include::Extension("md".into())], &[]);
    let mut core = fake_core(
        vec![target],
        [dir.path().to_path_buf()].into_iter().collect(),
    );

    core.process(RawEvent::Create(file.clone()));
    core.process(RawEvent::Create(file.clone()));

    assert_eq!(core.files.len(), 1, "监听集合不得重复");
    assert!(core.files.contains(&file));
    assert_eq!(
        core.backend.watch_calls.len(),
        2,
        "每次 Create 均无条件重挂（不得因已在集合而跳过）"
    );
    assert!(
        core.backend.unwatch_calls.contains(&file),
        "重挂须先卸载（Windows 上重复 watch 会泄漏句柄）"
    );
}

/// 2b#4 细则 4：单文件级失败不污染集合，且可被后续 Create 恢复。
#[test]
fn single_file_mount_failure_does_not_pollute_and_recovers() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("a.md");
    std::fs::write(&file, "x").unwrap();
    let target = direct(dir.path(), vec![Include::Extension("md".into())], &[]);
    let mut core = fake_core(
        vec![target],
        [dir.path().to_path_buf()].into_iter().collect(),
    );

    core.backend.fail_watch.insert(file.clone());
    core.process(RawEvent::Create(file.clone()));
    assert!(!core.files.contains(&file), "单文件级失败不得入集合");

    // 后续 Create 恢复。
    core.backend.fail_watch.remove(&file);
    core.process(RawEvent::Create(file.clone()));
    assert!(core.files.contains(&file), "后续 Create 应恢复监听");
}

/// 2b#5 细则 5：根 / 最近祖先级失败 → 显式上报降级 + 有界重试。
#[test]
fn root_level_mount_failure_reports_degradation_with_bounded_retry() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let target = direct(&root, vec![], &[]);
    let mut core = fake_core(vec![target], [root.clone()].into_iter().collect());
    core.backend.fail_watch.insert(root.clone());

    let messages = core.reconcile();
    assert_eq!(
        messages,
        vec![WatchMessage::Degraded(DegradeReason::MountFailed {
            path: root.clone(),
            attempts: MAX_MOUNT_ATTEMPTS,
        })],
        "根级失败必须显式上报，不得静默"
    );
    assert_eq!(
        core.backend.watch_calls.len(),
        MAX_MOUNT_ATTEMPTS,
        "重试须有界"
    );
    assert!(core.dirs.is_empty());

    // 去重：状态未变化时不重复上报。
    assert!(core.reconcile().is_empty());
}

/// fd 泄漏兜底：根存在但初始挂载失败 ⇒ 启动阶段须识别并放弃该 watcher。
#[test]
fn existing_root_with_failed_initial_mount_is_reported() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let target = recursive(&root, vec![], &[]);
    let mut core = fake_core(vec![target], [root.clone()].into_iter().collect());
    core.backend.fail_watch.insert(root.clone());

    let _ = core.reconcile();
    assert_eq!(core.failed_initial_roots(), vec![root]);
}

/// G5 延迟挂载设计不受影响：根不存在时不算初始挂载失败。
#[test]
fn missing_root_is_not_an_initial_mount_failure() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("watched");
    let target = recursive(&missing, vec![], &[]);
    let mut core = fake_core(
        vec![target],
        [dir.path().to_path_buf()].into_iter().collect(),
    );

    let _ = core.reconcile();
    assert!(core.failed_initial_roots().is_empty());
}

/// 2b#5 细则 5：无任何可挂载祖先 → 终止并上报「不可挂载」。
#[test]
fn unmountable_target_reports_once_and_does_not_mount() {
    let target = direct(Path::new("/nowhere/watched"), vec![], &[]);
    let mut core = fake_core(vec![target], HashSet::new());

    let messages = core.reconcile();
    assert_eq!(
        messages,
        vec![WatchMessage::Degraded(DegradeReason::Unmountable {
            root: PathBuf::from("/nowhere/watched"),
        })]
    );
    assert!(core.backend.watch_calls.is_empty(), "不可挂载不应尝试挂载");
    assert!(core.reconcile().is_empty(), "去重后不再重复上报");
}

/// 2b#6 细则 2b（防御性）：Create 先 / Remove 后不得拆掉刚挂的 watch。
#[test]
fn out_of_order_create_then_remove_keeps_watch() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("a.md");
    std::fs::write(&file, "x").unwrap();
    let target = direct(dir.path(), vec![Include::Extension("md".into())], &[]);
    let exists: HashSet<PathBuf> = [dir.path().to_path_buf(), file.clone()]
        .into_iter()
        .collect();
    let mut core = fake_core(vec![target], exists);

    // 「Create 先到」：挂上。
    core.files.insert(file.clone());
    core.process(RawEvent::Create(file.clone()));
    assert!(core.files.contains(&file));

    // 「Remove 后到」但路径仍存在（原子替换语义）：视为重挂而非拆除。
    core.process(RawEvent::Remove(file.clone()));
    assert!(
        core.files.contains(&file),
        "乱序 Remove 不得拆掉刚挂的 watch（防御性）"
    );
}

/// 2b#7 细则 3：不依赖 notify `Vnode::Link` 内部重挂。
#[test]
fn kqueue_link_modify_does_not_remount_dirs() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let target = direct(&root, vec![], &[]);
    let mut core = fake_core(vec![target], [root.clone()].into_iter().collect());
    core.dirs.insert(root.clone(), MountKind::DirShallow);

    // kqueue Link 被适配为 Modify(Any) 且路径为已直接挂载目录 → 抑制且不触发目录重挂。
    let before = core.backend.watch_calls.len();
    let messages = core.process(RawEvent::Modify(root.clone()));
    assert!(messages.is_empty(), "已挂载目录的条目级 Modify 应被抑制");
    assert_eq!(core.backend.watch_calls.len(), before, "不得触发目录重挂");

    // 文件 Modify 同样不触发任何目录重挂。
    let file = dir.path().join("a.md");
    core.process(RawEvent::Modify(file));
    assert_eq!(core.backend.watch_calls.len(), before);
}

// ── 生命周期与监听集合收敛（计划 2c） ────────────────────────────────

/// 轮询直到监听状态满足谓词。
async fn wait_status<F>(watcher: &FsWatcher, pred: F) -> bool
where
    F: Fn(&WatcherStatus) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if pred(&watcher.status()) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    pred(&watcher.status())
}

/// 轮询直到接收端关闭（`recv` 返回 `None`），途中丢弃残留消息。
async fn wait_closed(rx: &mut UnboundedReceiver<WatchMessage>, dur: Duration) -> bool {
    let deadline = Instant::now() + dur;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match timeout(remaining, rx.recv()).await {
            Ok(None) => return true,
            Ok(Some(_)) => continue,
            Err(_) => return false,
        }
    }
}

/// 2c#1 `stop` 后不再投递、监听释放。
#[tokio::test]
async fn stop_halts_delivery_and_releases_watches() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("a.md");
    std::fs::write(&file, "x").unwrap();
    let target = direct(dir.path(), vec![Include::Extension("md".into())], &[]);
    let (watcher, mut rx) = start(vec![target]).await.unwrap();

    assert!(
        wait_status(&watcher, |status| status.files.contains(&file)).await,
        "启动后应已挂载既有文件"
    );

    watcher.stop();

    assert!(
        wait_closed(&mut rx, Duration::from_secs(5)).await,
        "stop 后接收端应关闭，不再投递"
    );
    // 停止后写入不再产生事件（通道已关闭，recv 立即返回 None）。
    std::fs::write(dir.path().join("b.md"), "y").unwrap();
    assert!(
        matches!(
            timeout(Duration::from_millis(300), rx.recv()).await,
            Ok(None)
        ),
        "stop 后不得再投递"
    );
}

/// 2c#2 重复 create/delete 风暴下监听集合收敛、无句柄泄漏。
#[tokio::test]
async fn create_delete_storm_converges_without_leak() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("data.md");
    std::fs::write(&file, "seed").unwrap();
    let target = direct(dir.path(), vec![Include::Extension("md".into())], &[]);
    let (watcher, _rx) = start(vec![target]).await.unwrap();

    for _ in 0..25 {
        std::fs::write(&file, "x").unwrap();
        let _ = std::fs::remove_file(&file);
    }
    std::fs::write(&file, "final").unwrap();

    assert!(
        wait_status(&watcher, |status| status.dirs.len() == 1
            && status.files.len() == 1
            && status.files.contains(&file))
        .await,
        "风暴后监听集合应收敛为「1 目录 + 1 文件」：{:?}",
        watcher.status()
    );
    watcher.stop();
}

/// 2c#3 接收端被 drop 后模块不 panic（守护后台任务的容错路径）。
#[tokio::test]
async fn dropping_receiver_does_not_panic() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("a.md");
    std::fs::write(&file, "x").unwrap();
    let target = direct(dir.path(), vec![Include::Extension("md".into())], &[]);
    let (watcher, rx) = start(vec![target]).await.unwrap();

    drop(rx);
    // 触发事件；若投递路径 panic，任务会异常终止。
    std::fs::write(&file, "y").unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 句柄仍可正常停止。
    watcher.stop();
}

/// 2c#4 目录级重挂仅在目录事件时发生（不因文件事件触发目录重挂）。
#[tokio::test]
async fn directory_remount_only_on_directory_events() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("a.md");
    std::fs::write(&file, "x").unwrap();
    let target = direct(dir.path(), vec![Include::Extension("md".into())], &[]);
    let (watcher, mut rx) = start(vec![target]).await.unwrap();

    assert!(
        wait_status(&watcher, |status| status.dirs.len() == 1
            && status.files.len() == 1)
        .await
    );
    let before = watcher.status();

    for _ in 0..10 {
        std::fs::write(&file, "y").unwrap();
    }
    assert!(
        wait_event(&mut rx, Duration::from_secs(5), |m| is_event(
            m,
            &file,
            EventType::Modified
        ))
        .await
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    let after = watcher.status();
    assert_eq!(
        before.dir_mounts, after.dir_mounts,
        "文件事件不得触发目录重挂"
    );
    assert_eq!(before.dir_unmounts, after.dir_unmounts);
    watcher.stop();
}
