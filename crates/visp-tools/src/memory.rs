//! `memory` tool: durable task memory for the agent.
//!
//! Compaction writes key facts/decisions/dead ends automatically, but the
//! model can also record its own notes and query them later (especially after
//! a long compaction). Storage is abstracted by `MemoryStore`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use visp_core::memory::{MemoryKind, MemoryStore, NewMemoryEntry};
use visp_core::tool::{Tool, ToolContext, ToolResult};

pub struct MemoryTool {
    store: Arc<dyn MemoryStore>,
    default_limit: usize,
}

impl MemoryTool {
    pub fn new(store: Arc<dyn MemoryStore>) -> Self {
        Self {
            store,
            default_limit: 20,
        }
    }

    fn format_entries(entries: &[visp_core::memory::MemoryEntry]) -> String {
        let mut out = String::new();
        for e in entries {
            out.push_str(&format!("[#{} {}] {}\n", e.id, e.kind.as_str(), e.content));
        }
        out
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Durable task memory. Use action=\"record\" to save key confirmed facts, decisions and \
their reasons, dead ends, or direction changes that must survive context compaction. \
Use action=\"search\" or \"list\" to recall earlier notes after the transcript was compacted. \
Recording is idempotent per (kind, content)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["record", "search", "list"],
                    "description": "record a note, search notes, or list recent notes"
                },
                "kind": {
                    "type": "string",
                    "enum": ["goal", "fact", "decision", "dead_end", "pivot", "work", "next"],
                    "description": "memory kind for action=record (default: fact)"
                },
                "content": {
                    "type": "string",
                    "description": "note text for action=record"
                },
                "query": {
                    "type": "string",
                    "description": "substring to search for (action=search); omit to list"
                },
                "limit": {
                    "type": "integer",
                    "description": "maximum entries to return (default 20, max 200)"
                }
            },
            "required": ["action"]
        })
    }

    fn category(&self) -> &str {
        "context"
    }

    async fn execute(&self, arguments: Value, context: &ToolContext) -> ToolResult {
        let Some(session_id) = context.session_id.as_deref() else {
            return ToolResult::error("memory tool requires a session_id");
        };

        let action = arguments
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.default_limit as u64)
            .clamp(1, 200) as usize;

        match action.as_str() {
            "record" => {
                let content = arguments
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if content.is_empty() {
                    return ToolResult::error("memory record requires non-empty content");
                }
                let kind = arguments
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .and_then(MemoryKind::parse)
                    .unwrap_or(MemoryKind::Fact);
                match self
                    .store
                    .append(NewMemoryEntry::new(session_id, kind, content.to_string()))
                {
                    Ok(entry) => ToolResult::success(format!(
                        "Recorded {} memory #{}",
                        entry.kind.as_str(),
                        entry.id
                    )),
                    Err(e) => ToolResult::error(format!("failed to record memory: {e}")),
                }
            }
            "search" => {
                let query = arguments.get("query").and_then(|v| v.as_str());
                match self.store.search(session_id, query, limit) {
                    Ok(entries) if entries.is_empty() => {
                        ToolResult::success("No matching memory entries.")
                    }
                    Ok(entries) => ToolResult::success(Self::format_entries(&entries)),
                    Err(e) => ToolResult::error(format!("failed to search memory: {e}")),
                }
            }
            "list" => match self.store.list(session_id, limit) {
                Ok(entries) if entries.is_empty() => ToolResult::success("No memory entries yet."),
                Ok(entries) => ToolResult::success(Self::format_entries(&entries)),
                Err(e) => ToolResult::error(format!("failed to list memory: {e}")),
            },
            _ => ToolResult::error(format!(
                "unknown memory action {action:?}; expected record, search or list"
            )),
        }
    }

    fn requires_approval(&self) -> bool {
        false
    }
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
