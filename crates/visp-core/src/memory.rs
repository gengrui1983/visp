//! 持久化任务记忆（Task Memory）。
//!
//! 目的：上下文压缩后，任务中的关键事实、决定、死路和方向变化不能丢失。
//! `MemoryStore` 定义存储抽象；`visp-db` 提供 SQLite 实现，
//! `InMemoryMemoryStore` 供测试和无持久化场景使用。
//!
//! 设计取舍见 `docs/design/2026-11-12-context-management.md`。

use std::sync::{
    Mutex,
    atomic::{AtomicI64, Ordering},
};

use serde::{Deserialize, Serialize};

/// 记忆条目类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// 任务目标 / 用户约束
    Goal,
    /// 已确认的事实
    Fact,
    /// 做出的决定及理由
    Decision,
    /// 走不通的路 / 失败尝试
    DeadEnd,
    /// 方向改变
    Pivot,
    /// 已执行的关键操作（工具调用）
    Work,
    /// 下一步计划
    Next,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Goal => "goal",
            MemoryKind::Fact => "fact",
            MemoryKind::Decision => "decision",
            MemoryKind::DeadEnd => "dead_end",
            MemoryKind::Pivot => "pivot",
            MemoryKind::Work => "work",
            MemoryKind::Next => "next",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "goal" => Some(MemoryKind::Goal),
            "fact" => Some(MemoryKind::Fact),
            "decision" => Some(MemoryKind::Decision),
            "dead_end" | "deadend" => Some(MemoryKind::DeadEnd),
            "pivot" => Some(MemoryKind::Pivot),
            "work" => Some(MemoryKind::Work),
            "next" => Some(MemoryKind::Next),
            _ => None,
        }
    }

    /// 在供 LLM 阅读的记忆块中的分组标题。
    pub fn label(self) -> &'static str {
        match self {
            MemoryKind::Goal => "Goal",
            MemoryKind::Fact => "Confirmed facts",
            MemoryKind::Decision => "Decisions",
            MemoryKind::DeadEnd => "Dead ends (do not retry)",
            MemoryKind::Pivot => "Direction changes",
            MemoryKind::Work => "Work already done",
            MemoryKind::Next => "Next step",
        }
    }
}

/// 新记忆条目（尚未落库，无 id）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewMemoryEntry {
    pub session_id: String,
    pub kind: MemoryKind,
    pub content: String,
    pub source: Option<String>,
}

impl NewMemoryEntry {
    pub fn new(
        session_id: impl Into<String>,
        kind: MemoryKind,
        content: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            kind,
            content: content.into(),
            source: None,
        }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }
}

/// 已持久化的记忆条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: i64,
    pub session_id: String,
    pub kind: MemoryKind,
    pub content: String,
    pub source: Option<String>,
    /// Unix 毫秒时间戳。
    pub created_at: i64,
}

impl MemoryEntry {
    pub fn as_new(&self) -> NewMemoryEntry {
        NewMemoryEntry {
            session_id: self.session_id.clone(),
            kind: self.kind,
            content: self.content.clone(),
            source: self.source.clone(),
        }
    }
}

/// 任务记忆存储抽象。
///
/// 同步接口：SQLite 操作是微秒级，调用点在构建 prompt 的同步路径上，
/// 引入 async trait 只会增加复杂度。
pub trait MemoryStore: Send + Sync {
    /// 追加一条记忆；相同 session + kind + content 已存在时应幂等返回已有条目。
    fn append(&self, entry: NewMemoryEntry) -> Result<MemoryEntry, String>;

    /// 按关键词检索（大小写不敏感的子串匹配），按时间倒序。
    /// `query` 为空时等价于列出最近的条目。
    fn search(
        &self,
        session_id: &str,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>, String>;

    /// 列出某会话最近的记忆，按时间正序（便于拼进 prompt）。
    fn list(&self, session_id: &str, limit: usize) -> Result<Vec<MemoryEntry>, String>;

    /// 条目总数。
    fn count(&self, session_id: &str) -> Result<usize, String> {
        Ok(self.list(session_id, usize::MAX)?.len())
    }
}

/// 进程内实现：用于测试、`storage.driver = "memory"` 或持久化失败时的降级。
#[derive(Debug, Default)]
pub struct InMemoryMemoryStore {
    entries: Mutex<Vec<MemoryEntry>>,
    next_id: AtomicI64,
}

impl InMemoryMemoryStore {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            next_id: AtomicI64::new(1),
        }
    }
}

impl MemoryStore for InMemoryMemoryStore {
    fn append(&self, entry: NewMemoryEntry) -> Result<MemoryEntry, String> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|e| format!("memory store lock poisoned: {e}"))?;

        if let Some(existing) = entries.iter().find(|e| {
            e.session_id == entry.session_id && e.kind == entry.kind && e.content == entry.content
        }) {
            return Ok(existing.clone());
        }

        let stored = MemoryEntry {
            id: self.next_id.fetch_add(1, Ordering::SeqCst),
            session_id: entry.session_id,
            kind: entry.kind,
            content: entry.content,
            source: entry.source,
            created_at: chrono::Utc::now().timestamp_millis(),
        };
        entries.push(stored.clone());
        Ok(stored)
    }

    fn search(
        &self,
        session_id: &str,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>, String> {
        let entries = self
            .entries
            .lock()
            .map_err(|e| format!("memory store lock poisoned: {e}"))?;
        let needle = query.map(|q| q.to_lowercase());
        let mut result: Vec<MemoryEntry> = entries
            .iter()
            .filter(|e| e.session_id == session_id)
            .filter(|e| match &needle {
                Some(n) if !n.is_empty() => e.content.to_lowercase().contains(n),
                _ => true,
            })
            .cloned()
            .collect();
        result.sort_by_key(|a| std::cmp::Reverse(a.id));
        result.truncate(limit);
        Ok(result)
    }

    fn list(&self, session_id: &str, limit: usize) -> Result<Vec<MemoryEntry>, String> {
        let entries = self
            .entries
            .lock()
            .map_err(|e| format!("memory store lock poisoned: {e}"))?;
        let mut result: Vec<MemoryEntry> = entries
            .iter()
            .filter(|e| e.session_id == session_id)
            .cloned()
            .collect();
        result.sort_by_key(|a| a.id);
        if result.len() > limit {
            result.drain(0..result.len() - limit);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_append_search_and_dedupe() {
        let store = InMemoryMemoryStore::new();
        let e = store
            .append(NewMemoryEntry::new("s1", MemoryKind::Fact, "pool leaks"))
            .unwrap();
        let dup = store
            .append(NewMemoryEntry::new("s1", MemoryKind::Fact, "pool leaks"))
            .unwrap();
        assert_eq!(e.id, dup.id, "identical memory must be idempotent");

        store
            .append(NewMemoryEntry::new("s1", MemoryKind::Decision, "add Drop"))
            .unwrap();
        store
            .append(NewMemoryEntry::new("s2", MemoryKind::Fact, "unrelated"))
            .unwrap();

        let found = store.search("s1", Some("POOL"), 10).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, MemoryKind::Fact);

        let all = store.list("s1", 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].kind, MemoryKind::Fact, "list is chronological");
        assert_eq!(store.count("s1").unwrap(), 2);
    }

    #[test]
    fn memory_kind_round_trips() {
        for kind in [
            MemoryKind::Goal,
            MemoryKind::Fact,
            MemoryKind::Decision,
            MemoryKind::DeadEnd,
            MemoryKind::Pivot,
            MemoryKind::Work,
            MemoryKind::Next,
        ] {
            assert_eq!(MemoryKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(MemoryKind::parse("bogus"), None);
    }
}
