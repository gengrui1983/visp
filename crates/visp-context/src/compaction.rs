//! Structured compaction: turn the middle of a long transcript into durable
//! notes + a compact summary instead of silently dropping it.

use std::collections::HashMap;

use visp_core::memory::{MemoryEntry, MemoryKind};
use visp_core::message::{Message, Role, estimate_tokens};

/// One extracted note (not yet persisted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryNote {
    pub kind: MemoryKind,
    pub content: String,
}

impl MemoryNote {
    fn new(kind: MemoryKind, content: impl Into<String>) -> Self {
        Self {
            kind,
            content: content.into(),
        }
    }
}

/// Truncate text to an approximate token budget, keeping the head.
pub(crate) fn truncate_text_to_tokens(text: &str, max_tokens: u32) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    let mut current = text.to_string();
    loop {
        let tokens = estimate_tokens(&current);
        if tokens <= max_tokens || current.is_empty() {
            return current;
        }
        let chars = current.chars().count();
        let ratio = (max_tokens as f64 / tokens as f64) * 0.9;
        let keep = (chars as f64 * ratio).max(1.0) as usize;
        current = current.chars().take(keep).collect();
    }
}

fn group_lines(merged: &[(MemoryKind, String)], kind: MemoryKind) -> Vec<&String> {
    merged
        .iter()
        .filter(|(k, _)| *k == kind)
        .map(|(_, c)| c)
        .collect()
}

/// Append one group, honoring both a hard line limit and a token budget.
///
/// The summary has a fixed budget; without per-group budgets a long work log
/// could push out the confirmed facts that are supposed to survive compaction.
fn push_group_budget(
    out: &mut String,
    merged: &[(MemoryKind, String)],
    kind: MemoryKind,
    limit: usize,
    token_budget: u32,
) {
    let lines = group_lines(merged, kind);
    if lines.is_empty() || token_budget == 0 {
        return;
    }
    let header = format!("\n## {}\n", kind.label());
    let mut section = header.clone();
    let mut used = estimate_tokens(&header);
    if used > token_budget {
        return;
    }
    let mut count = 0usize;
    for line in lines {
        let text = format!("- {}\n", shorten(line, 240));
        let tokens = estimate_tokens(&text);
        if count >= limit || used.saturating_add(tokens) > token_budget {
            break;
        }
        section.push_str(&text);
        used += tokens;
        count += 1;
    }
    if count > 0 {
        out.push_str(&section);
    }
}

/// Truncate a long observation while keeping both the beginning and the end.
///
/// Logs and compiler output usually put the actionable error at the end, so
/// head-only truncation (the previous behavior) could hide the root cause.
pub(crate) fn truncate_observation(content: &str, max_chars: usize) -> String {
    let total = content.chars().count();
    if total <= max_chars {
        return content.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let head_chars = max_chars * 3 / 5;
    let tail_chars = max_chars.saturating_sub(head_chars);
    let head: String = content.chars().take(head_chars).collect();
    let tail: String = content
        .chars()
        .skip(total.saturating_sub(tail_chars))
        .collect();
    format!(
        "{head}\n... [{removed} chars truncated] ...\n{tail}",
        removed = total.saturating_sub(head_chars + tail_chars)
    )
}

/// Split assistant text into candidate notes and classify each line/sentence.
pub(crate) fn extract_notes(messages: &[Message]) -> Vec<MemoryNote> {
    let mut notes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |kind: MemoryKind, content: String| {
        let content = content.trim().to_string();
        if content.is_empty() {
            return;
        }
        let key = (kind.as_str().to_string(), content.clone());
        if seen.insert(key) {
            notes.push(MemoryNote::new(kind, content));
        }
    };

    // Map tool_call id -> signature for associating results.
    let mut call_sig: HashMap<String, String> = HashMap::new();

    for msg in messages {
        match msg.role {
            Role::User => {
                let content = shorten(&msg.content, 500);
                if !content.is_empty() {
                    push(MemoryKind::Fact, format!("User: {content}"));
                }
            }
            Role::Assistant => {
                if let Some(calls) = &msg.tool_calls {
                    for call in calls {
                        let sig = tool_signature(&call.name, &call.arguments);
                        call_sig.insert(call.id.clone(), sig.clone());
                        push(MemoryKind::Work, format!("{sig} -> requested"));
                    }
                }
                if !msg.content.trim().is_empty() {
                    let mut matched = false;
                    for sentence in split_sentences(&msg.content) {
                        if let Some(kind) = classify(&sentence) {
                            push(kind, sentence);
                            matched = true;
                        }
                    }
                    if !matched {
                        // No explicit marker: keep a short progress note.
                        let content = shorten(&msg.content, 300);
                        if content.chars().count() > 20 {
                            push(MemoryKind::Fact, format!("Assistant: {content}"));
                        }
                    }
                }
            }
            Role::Tool => {
                let short = shorten(&msg.content, 300);
                let sig = msg
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| call_sig.get(id))
                    .cloned();
                let is_error =
                    msg.tool_result_is_error.unwrap_or(false) || looks_like_error(&msg.content);
                if let Some(sig) = sig {
                    if is_error {
                        push(MemoryKind::DeadEnd, format!("{sig} failed: {short}"));
                    } else {
                        push(MemoryKind::Work, format!("{sig} -> ok: {short}"));
                    }
                } else if is_error {
                    push(MemoryKind::DeadEnd, format!("Tool failed: {short}"));
                }
            }
            Role::System => {}
        }
    }

    if notes.len() > 80 {
        // Work notes are cheap to regenerate from the work log; facts,
        // decisions, dead ends and pivots are not. Protect all non-work notes
        // and fill the remaining room with the most recent work notes.
        let mut important = Vec::new();
        let mut work = Vec::new();
        for note in notes {
            if note.kind == MemoryKind::Work {
                work.push(note);
            } else {
                important.push(note);
            }
        }
        important.truncate(80);
        let room = 80usize.saturating_sub(important.len());
        if work.len() > room {
            let drop = work.len() - room;
            work.drain(..drop);
        }
        important.extend(work);
        notes = important;
    }
    notes
}

/// Build a deduplicated log of tool calls already executed, with counts and
/// final status. This is what stops the model from re-running the same query.
pub(crate) fn build_work_log(messages: &[Message]) -> Vec<String> {
    struct Entry {
        signature: String,
        count: u32,
        status: String,
    }

    let mut call_sig: HashMap<String, String> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut entries: Vec<Entry> = Vec::new();

    let ensure = |sig: &str,
                  order: &mut Vec<String>,
                  index: &mut HashMap<String, usize>,
                  entries: &mut Vec<Entry>| {
        if !index.contains_key(sig) {
            index.insert(sig.to_string(), entries.len());
            order.push(sig.to_string());
            entries.push(Entry {
                signature: sig.to_string(),
                count: 0,
                status: "pending".to_string(),
            });
        }
        index[sig]
    };

    for msg in messages {
        if msg.role == Role::Assistant
            && let Some(calls) = &msg.tool_calls
        {
            for call in calls {
                let sig = tool_signature(&call.name, &call.arguments);
                call_sig.insert(call.id.clone(), sig.clone());
                let i = ensure(&sig, &mut order, &mut index, &mut entries);
                entries[i].count += 1;
            }
        }
        if msg.role == Role::Tool {
            let Some(id) = &msg.tool_call_id else {
                continue;
            };
            let Some(sig) = call_sig.get(id) else {
                continue;
            };
            let i = ensure(sig, &mut order, &mut index, &mut entries);
            let is_error =
                msg.tool_result_is_error.unwrap_or(false) || looks_like_error(&msg.content);
            entries[i].status = if is_error {
                format!("error: {}", shorten(&msg.content, 120))
            } else {
                "ok".to_string()
            };
        }
    }

    order
        .into_iter()
        .take(25)
        .filter_map(|sig| {
            let e = index.get(&sig).map(|&i| &entries[i])?;
            Some(format!(
                "- {} (x{}) -> {}",
                e.signature,
                e.count,
                shorten(&e.status, 160)
            ))
        })
        .collect()
}

/// Build the summary message injected in place of dropped turns.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_summary(
    compacted_count: usize,
    goal: Option<&str>,
    notes: &[MemoryNote],
    work_log: &[String],
    next_step: Option<&str>,
    stored: &[MemoryEntry],
    max_tokens: u32,
) -> String {
    let mut merged: Vec<(MemoryKind, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for note in notes {
        if seen.insert((note.kind, note.content.clone())) {
            merged.push((note.kind, note.content.clone()));
        }
    }
    for entry in stored {
        if seen.insert((entry.kind, entry.content.clone())) {
            merged.push((entry.kind, entry.content.clone()));
        }
    }

    // If the transcript no longer contains a goal/next-step (e.g. it was
    // compacted in an earlier round), fall back to agent-recorded memory.
    let goal = goal.map(str::to_string).or_else(|| {
        merged
            .iter()
            .find(|(k, _)| *k == MemoryKind::Goal)
            .map(|(_, c)| c.clone())
    });
    let next_step = next_step.map(str::to_string).or_else(|| {
        merged
            .iter()
            .rev()
            .find(|(k, _)| *k == MemoryKind::Next)
            .map(|(_, c)| c.clone())
    });

    let mut out = format!(
        "[CONTEXT COMPACTED]\n{compacted_count} earlier messages were summarized to fit the context window.\n\
The full transcript is still stored. Use the `memory` tool (action=\"search\") to retrieve details.\n"
    );

    if let Some(goal) = goal.as_deref() {
        out.push_str("\n## Goal\n");
        out.push_str(&shorten(goal, 600));
        out.push('\n');
    }

    if let Some(next) = next_step.as_deref() {
        out.push_str("\n## Next step\n");
        out.push_str(&shorten(next, 500));
        out.push('\n');
    }

    // Order matters: the summary may be truncated to fit the budget.
    // Decisions / pivots / dead ends are the most expensive information to
    // lose, so they get their own budget ahead of the (often verbose) facts
    // and work log. Every present section gets a share so a long work log
    // cannot push the confirmed facts out of the summary.
    let critical: Vec<MemoryKind> = [MemoryKind::Decision, MemoryKind::Pivot, MemoryKind::DeadEnd]
        .into_iter()
        .filter(|kind| !group_lines(&merged, *kind).is_empty())
        .collect();
    let has_facts = !group_lines(&merged, MemoryKind::Fact).is_empty();

    let work_lines: Vec<String> = if !work_log.is_empty() {
        work_log.to_vec()
    } else {
        group_lines(&merged, MemoryKind::Work)
            .into_iter()
            .map(|line| format!("- {}", shorten(line, 300)))
            .collect()
    };
    let has_work = !work_lines.is_empty();

    let remaining = max_tokens.saturating_sub(estimate_tokens(&out));
    let section_count =
        usize::from(!critical.is_empty()) + usize::from(has_facts) + usize::from(has_work);
    if section_count > 0 {
        let per_section = remaining / section_count as u32;
        if !critical.is_empty() {
            let per_kind = per_section / critical.len() as u32;
            for kind in critical {
                push_group_budget(&mut out, &merged, kind, 12, per_kind);
            }
        }
        if has_facts {
            push_group_budget(&mut out, &merged, MemoryKind::Fact, 12, per_section);
        }
        if has_work {
            push_work_log_budget(&mut out, &work_lines, per_section);
        }
    }

    truncate_text_to_tokens(&out, max_tokens)
}

/// Append the deduplicated work log within a token budget.
fn push_work_log_budget(out: &mut String, lines: &[String], token_budget: u32) {
    let header = "\n## Work already done (do not repeat unless files changed)\n";
    let mut section = header.to_string();
    let mut used = estimate_tokens(header);
    if used > token_budget {
        return;
    }
    let mut count = 0usize;
    for line in lines.iter().take(20) {
        let text = format!("{}\n", shorten(line, 220));
        let tokens = estimate_tokens(&text);
        if used.saturating_add(tokens) > token_budget {
            break;
        }
        section.push_str(&text);
        used += tokens;
        count += 1;
    }
    if count > 0 {
        out.push_str(&section);
    }
}

/// Remove orphan tool results / tool calls after cutting at turn boundaries.
pub(crate) fn repair_tool_pairs(messages: &mut Vec<Message>) {
    let result_ids: std::collections::HashSet<String> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    for msg in messages.iter_mut() {
        if msg.role == Role::Assistant
            && let Some(calls) = msg.tool_calls.clone()
        {
            let valid: Vec<_> = calls
                .iter()
                .filter(|c| result_ids.contains(&c.id))
                .cloned()
                .collect();
            if valid.is_empty() {
                msg.tool_calls = None;
            } else if valid.len() < calls.len() {
                msg.tool_calls = Some(valid);
            }
        }
    }

    let call_ids: std::collections::HashSet<String> = messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|c| c.id.clone())
        .collect();
    messages.retain(|m| {
        m.role != Role::Tool
            || m.tool_call_id
                .as_ref()
                .map(|id| call_ids.contains(id))
                .unwrap_or(false)
    });
}

// ── helpers ──────────────────────────────────────────────────────────────

fn shorten(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let head = max_chars * 3 / 4;
    let tail = max_chars.saturating_sub(head);
    let prefix: String = text.chars().take(head).collect();
    let suffix: String = text.chars().skip(count - tail).collect();
    format!("{prefix} ... {suffix}")
}

fn tool_signature(name: &str, arguments: &str) -> String {
    let args = arguments.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{name}({})", shorten(&args, 160))
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    for line in text.lines() {
        let mut start = 0;
        let chars: Vec<char> = line.chars().collect();
        for (i, ch) in chars.iter().enumerate() {
            // A period is only a sentence boundary when followed by whitespace
            // or end-of-line. Otherwise `dots.ocr`, `server.rs:42` and version
            // numbers (`1.2.3`) would be split and the actionable part of a
            // confirmed fact would be lost.
            let boundary = match ch {
                '.' => chars.get(i + 1).is_none_or(|next| next.is_whitespace()),
                '!' | '?' | ';' | '。' | '！' | '？' | '；' | '\n' => true,
                _ => false,
            };
            if boundary {
                let piece: String = chars[start..=i].iter().collect();
                if !piece.trim().is_empty() {
                    result.push(piece.trim().to_string());
                }
                start = i + 1;
            }
        }
        if start < chars.len() {
            let piece: String = chars[start..].iter().collect();
            if !piece.trim().is_empty() {
                result.push(piece.trim().to_string());
            }
        }
    }
    result
}

fn classify(line: &str) -> Option<MemoryKind> {
    let line = line.trim();
    if line.chars().count() < 8 {
        return None;
    }
    let lower = line.to_lowercase();
    const DEAD_ENDS: &[&str] = &[
        "doesn't work",
        "does not work",
        "not work",
        "failed",
        "cannot",
        "can't",
        "unable",
        "dead end",
        "走不通",
        "失败",
        "无法",
        "不可行",
        "排除",
    ];
    const PIVOTS: &[&str] = &[
        "instead",
        "switch to",
        "changed approach",
        "change approach",
        "改为",
        "改成",
        "转向",
        "换一个",
        "调整方案",
    ];
    const DECISIONS: &[&str] = &[
        "decided",
        "decision",
        "we will",
        "i will",
        "let's",
        "plan to",
        "approach:",
        "choose",
        "决定",
        "选择",
        "采用",
        "方案是",
    ];
    const FACTS: &[&str] = &[
        "confirmed",
        "found",
        "discovered",
        "root cause",
        "because",
        "确认",
        "发现",
        "根因",
        "原因是",
        "证明",
    ];

    if DEAD_ENDS.iter().any(|k| lower.contains(k)) {
        Some(MemoryKind::DeadEnd)
    } else if PIVOTS.iter().any(|k| lower.contains(k)) {
        Some(MemoryKind::Pivot)
    } else if DECISIONS.iter().any(|k| lower.contains(k)) {
        Some(MemoryKind::Decision)
    } else if FACTS.iter().any(|k| lower.contains(k)) {
        Some(MemoryKind::Fact)
    } else {
        None
    }
}

fn looks_like_error(content: &str) -> bool {
    let lower = content.to_lowercase();
    [
        "error",
        "failed",
        "exception",
        "panic",
        "not found",
        "denied",
    ]
    .iter()
    .any(|k| lower.contains(k))
}
