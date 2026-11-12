//! Adaptive context management for visp.
//!
//! Responsibilities:
//! 1. Estimate prompt size conservatively (CJK / code / JSON aware).
//! 2. Learn a per-session calibration factor from provider-reported usage.
//! 3. Compact (not merely drop) history near the limit into a structured
//!    summary that preserves goal, facts, decisions, dead ends, work log and
//!    next step.
//! 4. Persist durable memory entries so details survive compaction and can be
//!    retrieved later through the `memory` tool.
//! 5. React to server-side "context too long" errors by increasing pressure
//!    and letting the agent loop retry.

mod compaction;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use visp_core::context::ContextTrimmer;
use visp_core::memory::{MemoryEntry, MemoryStore, NewMemoryEntry};
use visp_core::message::{Message, Role, estimate_message_tokens};

use compaction::{
    build_summary, build_work_log, extract_notes, repair_tool_pairs, truncate_observation,
    truncate_text_to_tokens,
};

/// Configuration for [`DefaultContextTrimmer`].
#[derive(Debug, Clone, PartialEq)]
pub struct ContextConfig {
    pub head_turns: usize,
    pub tail_turns: usize,
    pub tool_output_max_chars: usize,
    /// Trigger compaction when the calibrated estimate reaches this percentage
    /// of the available budget.
    pub compact_trigger_pct: u8,
    /// Target percentage after compaction (hysteresis: avoid compacting every
    /// single iteration).
    pub compact_target_pct: u8,
    /// Extra headroom kept for estimator error.
    pub safety_margin_pct: u8,
    /// Minimum space reserved for the model's output.
    pub min_output_tokens: u32,
    /// Maximum tokens for the generated compaction summary.
    pub max_summary_tokens: u32,
    /// Maximum memory entries injected into the summary.
    pub max_memory_entries: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            head_turns: 2,
            tail_turns: 6,
            tool_output_max_chars: 8_000,
            compact_trigger_pct: 80,
            compact_target_pct: 45,
            safety_margin_pct: 12,
            min_output_tokens: 2_000,
            max_summary_tokens: 1_200,
            max_memory_entries: 80,
        }
    }
}

impl From<&visp_config::config::ContextSection> for ContextConfig {
    fn from(section: &visp_config::config::ContextSection) -> Self {
        Self {
            head_turns: section.head_turns,
            tail_turns: section.tail_turns,
            tool_output_max_chars: section.tool_output_max_chars,
            compact_trigger_pct: section.compact_trigger_pct,
            compact_target_pct: section.compact_target_pct,
            safety_margin_pct: section.safety_margin_pct,
            min_output_tokens: section.min_output_tokens,
            max_summary_tokens: section.max_summary_tokens,
            max_memory_entries: section.max_memory_entries,
        }
    }
}

/// Snapshot of the prompt produced by the last compaction.
///
/// The agent loop passes the **full** history on every iteration, so
/// re-compacting every time would regenerate the summary and slide the
/// retained tail — invalidating the provider's prompt cache on every turn.
/// Remembering the compacted result lets us append new messages unchanged and
/// only re-compact once the logical (compacted + appended) size reaches the
/// trigger again.
#[derive(Debug, Clone)]
struct CompactionCheckpoint {
    /// Length of the history prefix that produced `output`.
    input_len: usize,
    /// Base estimate of that prefix, used to detect a non-append-only history.
    input_estimate: u32,
    /// The compacted prompt (retained head + summary + retained tail).
    output: Arc<Vec<Message>>,
    /// Base estimate of `output` (without calibration).
    output_estimate: u32,
}

#[derive(Debug, Clone)]
struct SessionState {
    /// Multiplier applied to base estimates (actual / estimated).
    ///
    /// Deliberately never below 1.0: `UsageInfo.input_tokens` is not a
    /// normalized prompt total (Anthropic excludes cache read/write tokens), so
    /// `actual < estimated` can mean "most of the prompt was cached", not "the
    /// estimator over-counted". Lowering the multiplier on that signal would
    /// inflate the budget and trade a harmless waste of window for a possible
    /// provider 400.
    calibration: f64,
    samples: u32,
    /// Multiplier applied to the available budget after a server overflow.
    pressure: f64,
    overflow_level: u32,
    /// Set after a server overflow: the next trim must compact even if the
    /// (possibly underestimated) history appears to fit.
    force_compact: bool,
    compactions: u32,
    /// Result of the last compaction, reused while the transcript stays under
    /// the trigger (hysteresis).
    compaction: Option<CompactionCheckpoint>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            calibration: 1.0,
            samples: 0,
            pressure: 1.0,
            overflow_level: 0,
            force_compact: false,
            compactions: 0,
            compaction: None,
        }
    }
}

#[derive(Debug)]
struct SessionStates {
    map: HashMap<String, SessionState>,
    order: VecDeque<String>,
    max_sessions: usize,
}

impl Default for SessionStates {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            max_sessions: 256,
        }
    }
}

impl SessionStates {
    fn get(&mut self, key: &str) -> SessionState {
        if !self.map.contains_key(key) {
            if self.map.len() >= self.max_sessions
                && let Some(old) = self.order.pop_front()
            {
                self.map.remove(&old);
            }
            self.map.insert(key.to_string(), SessionState::default());
            self.order.push_back(key.to_string());
        }
        self.map.get(key).cloned().unwrap_or_default()
    }

    fn update(&mut self, key: &str, f: impl FnOnce(&mut SessionState)) {
        let _ = self.get(key);
        if let Some(state) = self.map.get_mut(key) {
            f(state);
        }
    }
}

/// Default context trimmer.
pub struct DefaultContextTrimmer {
    pub config: ContextConfig,
    memory: Option<Arc<dyn MemoryStore>>,
    states: Mutex<SessionStates>,
}

impl Default for DefaultContextTrimmer {
    fn default() -> Self {
        Self::new(ContextConfig::default(), None)
    }
}

impl DefaultContextTrimmer {
    pub fn new(config: ContextConfig, memory: Option<Arc<dyn MemoryStore>>) -> Self {
        Self {
            config,
            memory,
            states: Mutex::new(SessionStates::default()),
        }
    }

    pub fn with_memory(config: ContextConfig, memory: Arc<dyn MemoryStore>) -> Self {
        Self::new(config, Some(memory))
    }

    fn key(session_id: Option<&str>) -> String {
        session_id.unwrap_or("__default__").to_string()
    }

    fn state(&self, session_id: Option<&str>) -> SessionState {
        self.states
            .lock()
            .map(|mut states| states.get(&Self::key(session_id)))
            .unwrap_or_default()
    }

    fn update_state(&self, session_id: Option<&str>, f: impl FnOnce(&mut SessionState)) {
        if let Ok(mut states) = self.states.lock() {
            states.update(&Self::key(session_id), f);
        }
    }

    /// Available budget in estimated-token units.
    fn effective_budget(
        &self,
        max_context_tokens: u32,
        system_overhead: u32,
        output_tokens: u32,
        state: &SessionState,
    ) -> u32 {
        if max_context_tokens == 0 {
            return 0;
        }
        let capped_output = output_tokens.min(max_context_tokens * 3 / 4);
        let reserve = capped_output.max(self.config.min_output_tokens);
        let available = max_context_tokens
            .saturating_sub(reserve)
            .saturating_sub(system_overhead);
        let margin = self.config.safety_margin_pct.min(90) as u32;
        let after_margin = available.saturating_mul(100 - margin) / 100;
        let calibrated = after_margin as f64 / state.calibration.max(1.0);
        (calibrated * state.pressure).max(1.0) as u32
    }

    fn trim_inner(
        &self,
        session_id: Option<&str>,
        history: &[Message],
        max_context_tokens: u32,
        system_overhead: u32,
        output_tokens: u32,
    ) -> Vec<Message> {
        if history.is_empty() {
            return Vec::new();
        }

        let state = self.state(session_id);
        let budget =
            self.effective_budget(max_context_tokens, system_overhead, output_tokens, &state);
        let trigger = if state.force_compact {
            0
        } else {
            budget.saturating_mul(self.config.compact_trigger_pct as u32) / 100
        };
        let target = budget.saturating_mul(self.config.compact_target_pct as u32) / 100;

        let total = estimate_messages(history);

        // Hysteresis: reuse the last compacted prompt while the transcript
        // (compacted output + messages appended since) stays below the
        // trigger. This keeps the prompt prefix stable across iterations.
        if !state.force_compact
            && let Some(checkpoint) = state.compaction.as_ref()
        {
            let prefix_unchanged = history.len() >= checkpoint.input_len
                && estimate_messages(&history[..checkpoint.input_len]) == checkpoint.input_estimate;
            if prefix_unchanged {
                let appended = &history[checkpoint.input_len..];
                let logical_total = checkpoint
                    .output_estimate
                    .saturating_add(estimate_messages(appended));
                if logical_total <= trigger {
                    let mut result = (*checkpoint.output).clone();
                    result.extend_from_slice(appended);
                    repair_tool_pairs(&mut result);
                    let output_estimate = estimate_messages(&result);
                    self.update_state(session_id, |s| {
                        s.compaction = Some(CompactionCheckpoint {
                            input_len: history.len(),
                            input_estimate: total,
                            output: Arc::new(result.clone()),
                            output_estimate,
                        });
                    });
                    return result;
                }
            } else {
                // History is no longer an append-only extension of the
                // checkpoint; discard it before it can return stale content.
                self.update_state(session_id, |s| s.compaction = None);
            }
        }

        if total <= trigger {
            if state.compaction.is_some() {
                self.update_state(session_id, |s| s.compaction = None);
            }
            return history.to_vec();
        }

        if state.force_compact {
            // One forced compaction per overflow report; if it is still too
            // long the next provider 400 will set the flag again.
            self.update_state(session_id, |s| {
                s.force_compact = false;
                s.compaction = None;
            });
        }

        let result = self.compact(session_id, history, target, &state);
        // Remember which prefix produced this result so later iterations can
        // append to it instead of re-summarizing the whole transcript.
        let output_estimate = estimate_messages(&result);
        self.update_state(session_id, |s| {
            s.compaction = Some(CompactionCheckpoint {
                input_len: history.len(),
                input_estimate: total,
                output: Arc::new(result.clone()),
                output_estimate,
            });
        });
        result
    }

    fn compact(
        &self,
        session_id: Option<&str>,
        history: &[Message],
        target: u32,
        state: &SessionState,
    ) -> Vec<Message> {
        // Stage 1: keep both ends of oversized observations.
        let work: Vec<Message> = history
            .iter()
            .cloned()
            .map(|mut msg| {
                if msg.role == Role::Tool {
                    let truncated =
                        truncate_observation(&msg.content, self.config.tool_output_max_chars);
                    if truncated != msg.content {
                        msg.content = truncated;
                    }
                }
                msg
            })
            .collect();
        if estimate_messages(&work) <= target {
            return work;
        }

        let starts = turn_starts(&work);
        if starts.is_empty() {
            return keep_tail(&work, target);
        }

        let mut head_turns = self.config.head_turns.max(1).min(starts.len());
        let mut tail_turns = self
            .config
            .tail_turns
            .max(1)
            .min(starts.len().saturating_sub(head_turns).max(1));

        let goal = work
            .iter()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone());
        let next_step = work
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && !m.content.trim().is_empty())
            .map(|m| m.content.clone());
        let stored = self.load_memory(session_id);

        loop {
            let head_end = if starts.len() <= head_turns {
                work.len()
            } else {
                starts[head_turns]
            };
            let tail_start = if starts.len() <= tail_turns {
                0
            } else {
                starts[starts.len() - tail_turns]
            };

            if head_end < tail_start {
                let middle = &work[head_end..tail_start];
                let notes = extract_notes(middle);
                self.persist_notes(session_id, &notes, head_end, tail_start);
                let work_log = build_work_log(middle);
                let summary = build_summary(
                    middle.len(),
                    goal.as_deref(),
                    &notes,
                    &work_log,
                    next_step.as_deref(),
                    &stored,
                    self.config.max_summary_tokens,
                );

                let mut result = Vec::new();
                result.extend_from_slice(&work[..head_end]);
                result.push(Message::system(summary));
                result.extend_from_slice(&work[tail_start..]);
                repair_tool_pairs(&mut result);

                if estimate_messages(&result) <= target || target == 0 {
                    if state.pressure < 1.0 || state.overflow_level > 0 {
                        self.update_state(session_id, |s| {
                            s.compactions += 1;
                        });
                    }
                    return result;
                }

                if tail_turns > 1 {
                    tail_turns -= 1;
                    continue;
                }
                if head_turns > 1 {
                    head_turns -= 1;
                    continue;
                }

                // Stage 2: hard-truncate retained messages (oldest first) while
                // keeping at least the last user turn intact.
                return self.shrink_to_fit(result, target);
            }

            if tail_turns > 1 {
                tail_turns -= 1;
            } else if head_turns > 1 {
                head_turns -= 1;
            } else {
                break;
            }
        }

        self.compact_fallback(session_id, &work, goal, next_step, &stored, target)
    }

    /// Last-resort compaction when there are too few turns for a middle
    /// section (for example one user message followed by a single huge
    /// assistant message). Keeps the task anchor, summarizes everything after
    /// it, and appends as much of the newest tail as fits.
    fn compact_fallback(
        &self,
        session_id: Option<&str>,
        work: &[Message],
        goal: Option<String>,
        next_step: Option<String>,
        stored: &[MemoryEntry],
        target: u32,
    ) -> Vec<Message> {
        let anchor_end = work
            .iter()
            .position(|m| m.role == Role::User)
            .map(|i| i + 1)
            .unwrap_or(0);
        let rest = &work[anchor_end..];
        let notes = extract_notes(rest);
        self.persist_notes(session_id, &notes, anchor_end, work.len());
        let work_log = build_work_log(rest);
        let summary = build_summary(
            rest.len(),
            goal.as_deref(),
            &notes,
            &work_log,
            next_step.as_deref(),
            stored,
            self.config.max_summary_tokens.max(200),
        );

        let mut result: Vec<Message> = work[..anchor_end].to_vec();
        result.push(Message::system(summary));

        // Keep the newest messages that still fit without splitting the tail.
        let base = estimate_messages(&result);
        let mut tail: Vec<Message> = Vec::new();
        let mut tail_tokens = 0u32;
        for msg in rest.iter().rev() {
            let t = estimate_message_tokens(msg);
            if base.saturating_add(tail_tokens).saturating_add(t) > target {
                break;
            }
            tail_tokens += t;
            tail.push(msg.clone());
        }
        tail.reverse();
        result.extend(tail);
        repair_tool_pairs(&mut result);

        // A zero-token budget is degenerate (the window is entirely consumed
        // by output reserve / system overhead). Returning the goal anchor is
        // more useful than returning nothing.
        if target > 0 && estimate_messages(&result) > target {
            return self.shrink_to_fit(result, target);
        }
        result
    }

    fn shrink_to_fit(&self, mut messages: Vec<Message>, target: u32) -> Vec<Message> {
        // Keep summary (index 1 usually) and the final user turn; compress the
        // rest from oldest to newest so recent context stays intact.
        let hard_cap = 3_000usize;
        let len = messages.len();
        for i in 0..len {
            if estimate_messages(&messages) <= target {
                break;
            }
            let is_summary = messages[i].role == Role::System;
            let is_last_user = messages[i].role == Role::User
                && messages[i + 1..].iter().all(|m| m.role != Role::User);
            if is_summary || is_last_user {
                continue;
            }
            if messages[i].content.chars().count() > hard_cap
                || estimate_message_tokens(&messages[i]) > hard_cap as u32 / 3
            {
                messages[i].content = truncate_observation(&messages[i].content, hard_cap);
            }
        }
        // Last resort: drop non-essential messages from the front (never the
        // summary or the final user message).
        while estimate_messages(&messages) > target && messages.len() > 2 {
            let idx = messages
                .iter()
                .position(|m| m.role != Role::System && m.role != Role::User)
                .or_else(|| {
                    messages
                        .iter()
                        .enumerate()
                        .find(|(i, m)| *i > 0 && m.role == Role::User)
                        .map(|(i, _)| i)
                });
            match idx {
                Some(i) if i + 1 < messages.len() => {
                    messages.remove(i);
                }
                _ => break,
            }
        }

        // Absolute fallback: anchor (goal) + summary + latest user message.
        // `target` can be smaller than any real conversation turn, especially
        // right after a server overflow; never return something over budget.
        if estimate_messages(&messages) > target {
            let goal = messages.iter().find(|m| m.role == Role::User).cloned();
            let summary = messages.iter().find(|m| m.role == Role::System).cloned();
            let last_user = messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .cloned();

            let mut minimal: Vec<Message> = Vec::new();
            if let Some(mut m) = goal {
                m.content = truncate_text_to_tokens(&m.content, target / 4);
                minimal.push(m);
            }
            if let Some(mut m) = summary {
                m.content = truncate_text_to_tokens(&m.content, target / 2);
                minimal.push(m);
            }
            if let Some(mut m) = last_user {
                m.content = truncate_text_to_tokens(&m.content, target / 4);
                if !minimal.iter().any(|other| other.content == m.content) {
                    minimal.push(m);
                }
            }
            if !minimal.is_empty() {
                messages = minimal;
            }
        }

        repair_tool_pairs(&mut messages);
        messages
    }

    fn load_memory(&self, session_id: Option<&str>) -> Vec<visp_core::memory::MemoryEntry> {
        let (Some(session_id), Some(store)) = (session_id, self.memory.as_ref()) else {
            return Vec::new();
        };
        store
            .list(session_id, self.config.max_memory_entries)
            .unwrap_or_else(|e| {
                tracing::warn!(session_id, error = %e, "failed to load task memory");
                Vec::new()
            })
    }

    fn persist_notes(
        &self,
        session_id: Option<&str>,
        notes: &[compaction::MemoryNote],
        start: usize,
        end: usize,
    ) {
        let (Some(session_id), Some(store)) = (session_id, self.memory.as_ref()) else {
            return;
        };
        let source = format!("messages[{start}..{end}]");
        for note in notes {
            let entry = NewMemoryEntry::new(session_id, note.kind, note.content.clone())
                .with_source(source.clone());
            if let Err(e) = store.append(entry) {
                tracing::warn!(session_id, error = %e, "failed to persist task memory");
                break;
            }
        }
    }
}

fn estimate_messages(messages: &[Message]) -> u32 {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Find the start index of each logical turn.
///
/// A user message always starts a turn and the first assistant reply belongs to
/// it. Subsequent assistant messages (the tool-call steps of an agent loop) are
/// separate turns even without an intervening user message. This is what makes
/// `head_turns` / `tail_turns` work for the common real-world transcript:
/// one user message followed by dozens of assistant/tool rounds.
fn turn_starts(messages: &[Message]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut turn_has_assistant = false;
    for (i, msg) in messages.iter().enumerate() {
        match msg.role {
            Role::User => {
                starts.push(i);
                turn_has_assistant = false;
            }
            Role::Assistant => {
                if starts.is_empty() || turn_has_assistant {
                    starts.push(i);
                }
                turn_has_assistant = true;
            }
            Role::System | Role::Tool => {}
        }
    }
    starts
}

/// Keep the newest suffix that fits the budget (used when there is no turn
/// boundary to compact around, e.g. a tool-only transcript).
fn keep_tail(messages: &[Message], target: u32) -> Vec<Message> {
    let mut result: Vec<Message> = Vec::new();
    let mut used = 0u32;
    for msg in messages.iter().rev() {
        let t = estimate_message_tokens(msg);
        if used.saturating_add(t) > target {
            break;
        }
        used += t;
        result.push(msg.clone());
    }
    result.reverse();
    repair_tool_pairs(&mut result);
    result
}

impl ContextTrimmer for DefaultContextTrimmer {
    fn trim(
        &self,
        history: &[Message],
        max_context_tokens: u32,
        system_overhead: u32,
        output_tokens: u32,
    ) -> Vec<Message> {
        self.trim_inner(
            None,
            history,
            max_context_tokens,
            system_overhead,
            output_tokens,
        )
    }

    fn trim_for_session(
        &self,
        session_id: Option<&str>,
        history: &[Message],
        max_context_tokens: u32,
        system_overhead: u32,
        output_tokens: u32,
    ) -> Vec<Message> {
        self.trim_inner(
            session_id,
            history,
            max_context_tokens,
            system_overhead,
            output_tokens,
        )
    }

    fn observe_usage(&self, session_id: Option<&str>, estimated_input: u32, actual_input: u32) {
        if estimated_input == 0 || actual_input == 0 {
            return;
        }
        let sample = actual_input as f64 / estimated_input as f64;
        if !(0.2..=8.0).contains(&sample) {
            return;
        }
        self.update_state(session_id, |state| {
            // Only calibrate upward. A sub-unit sample is ambiguous (see
            // `SessionState::calibration`): treating it as overestimation would
            // enlarge the budget exactly when cache accounting made the
            // reported input smaller than the real prompt.
            state.calibration = if state.samples == 0 {
                sample.clamp(1.0, 6.0)
            } else {
                state.calibration.max(sample).clamp(1.0, 6.0)
            };
            state.samples += 1;
            // A successful provider call with known usage means the pressure
            // is no longer needed; calibration carries the correction forward.
            state.pressure = 1.0;
            state.overflow_level = 0;
            state.force_compact = false;
        });
    }

    fn on_context_overflow(&self, session_id: Option<&str>) -> u32 {
        let mut level = 0;
        self.update_state(session_id, |state| {
            state.pressure = (state.pressure * 0.7).max(0.25);
            state.overflow_level += 1;
            state.force_compact = true;
            level = state.overflow_level;
        });
        level
    }

    fn reset_context_pressure(&self, session_id: Option<&str>) {
        self.update_state(session_id, |state| {
            state.pressure = (state.pressure * 1.15).min(1.0);
            if state.pressure >= 1.0 {
                state.overflow_level = 0;
            }
        });
    }

    fn estimate_tokens(&self, session_id: Option<&str>, messages: &[Message]) -> u32 {
        let base = estimate_messages(messages);
        let state = self.state(session_id);
        (base as f64 * state.calibration.max(1.0)) as u32
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
