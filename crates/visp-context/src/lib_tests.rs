use super::*;
use std::sync::Arc;
use visp_core::memory::{InMemoryMemoryStore, MemoryKind, MemoryStore, NewMemoryEntry};
use visp_core::message::ToolCallRequest;
use visp_core::message::estimate_tokens;

fn small_config() -> ContextConfig {
    ContextConfig {
        head_turns: 1,
        tail_turns: 1,
        tool_output_max_chars: 400,
        compact_trigger_pct: 80,
        compact_target_pct: 45,
        safety_margin_pct: 0,
        min_output_tokens: 100,
        max_summary_tokens: 2_000,
        max_memory_entries: 80,
    }
}

fn long_turn(i: usize, len: usize) -> Vec<Message> {
    let user = format!("user task {i}: {}", "u".repeat(len));
    let assistant = format!(
        "Confirmed fact {i}: root cause found in module {i}. Decided to fix it. {}",
        "a".repeat(len)
    );
    vec![Message::user(user), Message::assistant(assistant)]
}

fn tool_turn(i: usize, error: bool) -> Vec<Message> {
    let id = format!("call_{i}");
    let call = Message::tool_call(vec![ToolCallRequest {
        id: id.clone(),
        name: "grep".into(),
        arguments: format!(r#"{{"pattern":"needle_{i}","path":"src"}}"#),
    }]);
    let result = Message::tool(
        if error {
            format!("grep failed: permission denied for src/module_{i}.rs")
        } else {
            format!("src/module_{i}.rs:42: needle_{i}")
        },
        id,
    );
    vec![Message::user(format!("inspect module {i}")), call, result]
}

fn compacted_summary(result: &[Message]) -> Option<&str> {
    result
        .iter()
        .find(|m| m.role == Role::System && m.content.contains("[CONTEXT COMPACTED]"))
        .map(|m| m.content.as_str())
}

#[test]
fn default_config_is_reasonable() {
    let c = ContextConfig::default();
    assert_eq!(c.head_turns, 2);
    assert_eq!(c.tail_turns, 6);
    assert!(c.compact_target_pct < c.compact_trigger_pct);
    assert!(c.safety_margin_pct > 0);
}

#[test]
fn empty_history_returns_empty() {
    let trimmer = DefaultContextTrimmer::default();
    assert!(trimmer.trim(&[], 1000, 10, 100).is_empty());
}

#[test]
fn under_trigger_history_is_unchanged() {
    let trimmer = DefaultContextTrimmer::default();
    let history = vec![Message::user("short"), Message::assistant("short answer")];
    let result = trimmer.trim(&history, 128_000, 10, 4096);
    assert_eq!(result, history);
}

#[test]
fn cjk_and_punctuation_estimates_are_conservative() {
    let trimmer = DefaultContextTrimmer::default();
    let cjk = vec![Message::user("中".repeat(1000))];
    let cjk_est = trimmer.estimate_tokens(None, &cjk);
    assert!(
        cjk_est >= 1000,
        "CJK history must not be estimated at bytes/4, got {cjk_est}"
    );

    let json = vec![Message::user("{}[](),:;".repeat(200))];
    let json_est = trimmer.estimate_tokens(None, &json);
    assert!(
        json_est >= 700,
        "punctuation-heavy estimate too low: {json_est}"
    );
    assert!(estimate_tokens("hello") < 10);
}

#[test]
fn provider_usage_calibrates_estimates_per_session() {
    let trimmer = DefaultContextTrimmer::default();
    let history = vec![Message::user("a".repeat(4000))];
    let before = trimmer.estimate_tokens(Some("s1"), &history);
    assert!(before > 0);

    // Provider counted twice as many tokens as the heuristic (e.g. local
    // tokenizer with CJK/code). The next estimate must reflect it.
    trimmer.observe_usage(Some("s1"), before, before * 2);
    let after = trimmer.estimate_tokens(Some("s1"), &history);
    assert!(
        after >= before * 2,
        "calibration not applied: before={before}, after={after}"
    );

    // Other sessions are unaffected.
    let other = trimmer.estimate_tokens(Some("s2"), &history);
    assert_eq!(other, before);
}

#[test]
fn calibration_brings_estimate_close_to_actual_usage() {
    let trimmer = DefaultContextTrimmer::default();
    let history = vec![
        Message::user("排查一下 server.rs 里连接池不释放的问题，日志在 /tmp/server.log"),
        Message::assistant(r#"{"error":"connection pool exhausted","path":"/tmp/server.log"}"#),
    ];
    let base = trimmer.estimate_tokens(Some("s"), &history);
    // Simulate a provider tokenizer that counts 1.7x more than the heuristic.
    let actual = (base as f64 * 1.7) as u32;
    trimmer.observe_usage(Some("s"), base, actual);
    let calibrated = trimmer.estimate_tokens(Some("s"), &history);
    let error = (calibrated as f64 - actual as f64).abs() / actual as f64;
    assert!(
        error < 0.10,
        "calibrated estimate {calibrated} should be within 10% of actual {actual}"
    );
}

#[test]
fn server_overflow_increases_pressure_and_compacts() {
    let config = ContextConfig {
        min_output_tokens: 100,
        ..small_config()
    };
    let trimmer = DefaultContextTrimmer::new(config, None);
    let mut history = Vec::new();
    for i in 0..8 {
        history.extend(long_turn(i, 2_000));
    }

    // Under the original budget this fits without compaction.
    let normal = trimmer.trim_for_session(Some("s"), &history, 20_000, 100, 200);
    assert!(compacted_summary(&normal).is_none());

    // A single server overflow must force compaction on the very next build,
    // even when the heuristic still thinks the prompt fits.
    assert!(trimmer.on_context_overflow(Some("s")) > 0);
    let pressured = trimmer.trim_for_session(Some("s"), &history, 20_000, 100, 200);
    assert!(
        compacted_summary(&pressured).is_some(),
        "overflow pressure must force compaction"
    );
}

#[test]
fn compaction_keeps_goal_facts_work_and_next_step() {
    let trimmer = DefaultContextTrimmer::new(small_config(), None);
    let mut history = Vec::new();
    history.push(Message::user(
        "Investigate the connection pool leak in server.rs",
    ));
    history.push(Message::assistant(
        "Confirmed fact: the pool never releases connections. Decided to add a Drop impl.",
    ));
    history.extend(tool_turn(1, false));
    history.extend(tool_turn(2, true));
    for i in 0..5 {
        history.extend(long_turn(i + 3, 2_000));
    }
    history.push(Message::assistant(
        "Next I will patch src/pool.rs and run cargo test.",
    ));

    let result = trimmer.trim_for_session(Some("s"), &history, 4_000, 100, 200);
    let summary = compacted_summary(&result).expect("expected compaction summary");

    assert!(summary.contains("## Goal"), "{summary}");
    assert!(summary.contains("connection pool leak"), "{summary}");
    assert!(summary.contains("## Next step"), "{summary}");
    assert!(
        summary.contains("Confirmed facts") || summary.contains("Decisions"),
        "{summary}"
    );
    assert!(
        summary.contains("Work already done"),
        "prior tool calls must be listed: {summary}"
    );
    assert!(summary.contains("grep("), "{summary}");
    assert!(
        summary.contains("## Dead ends") || summary.contains("failed"),
        "{summary}"
    );
    assert!(result.len() < history.len());
}

#[test]
fn compaction_persists_memory_and_it_can_be_searched() {
    let store = Arc::new(InMemoryMemoryStore::new());
    let trimmer = DefaultContextTrimmer::with_memory(small_config(), store.clone());
    let mut history = vec![Message::user("Find why tests hang")];
    history.push(Message::assistant(
        "Confirmed: the test waits forever on a mutex. Decided to use a timeout.",
    ));
    history.extend(tool_turn(1, true));
    for i in 0..6 {
        history.extend(long_turn(i + 2, 2_000));
    }

    let _ = trimmer.trim_for_session(Some("s"), &history, 4_000, 100, 200);

    // The initial fact is in the protected head; facts from compacted turns
    // must be persisted to the memory store.
    let facts = store.search("s", Some("root cause"), 20).unwrap();
    assert!(
        facts
            .iter()
            .any(|e| e.kind == MemoryKind::Fact || e.kind == MemoryKind::Decision),
        "expected persisted fact/decision from compacted turns, got {facts:?}"
    );
    let dead = store.search("s", Some("permission denied"), 20).unwrap();
    assert!(
        dead.iter().any(|e| e.kind == MemoryKind::DeadEnd),
        "expected persisted dead end, got {dead:?}"
    );
}

fn assert_no_orphan_tool_pairs(messages: &[Message]) {
    let call_ids: std::collections::HashSet<String> = messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|c| c.id.clone())
        .collect();
    let result_ids: std::collections::HashSet<String> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();
    assert_eq!(
        call_ids, result_ids,
        "tool call/result pairs must stay intact"
    );
}

#[test]
fn compaction_has_no_orphan_tool_messages() {
    let trimmer = DefaultContextTrimmer::new(small_config(), None);
    let mut history = Vec::new();
    for i in 0..8 {
        history.extend(tool_turn(i, i % 2 == 1));
    }
    let result = trimmer.trim_for_session(Some("s"), &history, 3_000, 100, 200);
    assert_no_orphan_tool_pairs(&result);
}

#[test]
fn compaction_respects_target_budget() {
    let config = ContextConfig {
        compact_target_pct: 35,
        max_summary_tokens: 600,
        ..small_config()
    };
    let trimmer = DefaultContextTrimmer::new(config, None);
    let mut history = Vec::new();
    for i in 0..10 {
        history.extend(long_turn(i, 2_000));
    }
    let result = trimmer.trim_for_session(Some("s"), &history, 4_000, 100, 200);
    let budget = 4_000u32 - 200 - 100;
    let target = budget * 35 / 100;
    assert!(
        estimate_messages(&result) <= target,
        "result {} tokens exceeds target {target}",
        estimate_messages(&result)
    );
}

#[test]
fn repeated_work_is_deduplicated_in_work_log() {
    let mut messages = Vec::new();
    for i in 0..4 {
        let id = format!("c{i}");
        messages.push(Message::tool_call(vec![ToolCallRequest {
            id: id.clone(),
            name: "grep".into(),
            arguments: r#"{"pattern":"todo"}"#.into(),
        }]));
        messages.push(Message::tool("no matches", id));
    }
    let log = build_work_log(&messages);
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("x4"), "{}", log[0]);
}

#[test]
fn observation_truncation_keeps_the_error_at_the_end() {
    let mut content = "start line\n".repeat(1000);
    content.push_str("FATAL ERROR: out of memory at the very end");
    let truncated = truncate_observation(&content, 200);
    assert!(truncated.chars().count() < content.chars().count());
    assert!(truncated.contains("FATAL ERROR"), "{truncated}");
    assert!(truncated.starts_with("start line"));
    assert!(truncated.contains("truncated"));
}

#[test]
fn memory_is_not_required_for_compaction() {
    let trimmer = DefaultContextTrimmer::new(small_config(), None);
    let mut history = Vec::new();
    for i in 0..8 {
        history.extend(long_turn(i, 2_000));
    }
    let result = trimmer.trim_for_session(Some("s"), &history, 3_000, 100, 200);
    assert!(compacted_summary(&result).is_some());
}

#[test]
fn extreme_budget_still_keeps_task_anchor() {
    let trimmer = DefaultContextTrimmer::new(small_config(), None);
    let history = vec![
        Message::user("the original goal must survive"),
        Message::assistant("a".repeat(50_000)),
    ];
    let result = trimmer.trim(&history, 1_000, 500, 900);
    assert!(!result.is_empty());
    assert!(
        result.iter().any(|m| m.content.contains("original goal")),
        "task anchor lost: {result:?}"
    );
}

#[test]
fn persisted_memory_from_agent_is_injected_into_next_summary() {
    let store = Arc::new(InMemoryMemoryStore::new());
    store
        .append(NewMemoryEntry::new(
            "s",
            MemoryKind::Decision,
            "Do not retry the legacy migration path",
        ))
        .unwrap();
    let trimmer = DefaultContextTrimmer::with_memory(small_config(), store);
    let mut history = Vec::new();
    for i in 0..8 {
        history.extend(long_turn(i, 2_000));
    }
    let result = trimmer.trim_for_session(Some("s"), &history, 3_000, 100, 200);
    let summary = compacted_summary(&result).expect("summary");
    assert!(
        summary.contains("legacy migration path"),
        "agent-recorded memory missing from summary: {summary}"
    );
}

/// One user message followed by dozens of assistant/tool rounds is the shape
/// of a real agent task. The old turn segmentation keyed only on user messages
/// treated the whole transcript as a single turn and fell back to a 2-message
/// result, losing all confirmed facts and recent work.
fn agent_task_history() -> Vec<Message> {
    let mut history = vec![Message::user(
        "Investigate why 7 of 12 documents failed on 10-01. Use SQL over the logs.".to_string(),
    )];
    for i in 0..40 {
        let id = format!("call_{i}");
        history.push(Message::tool_call(vec![ToolCallRequest {
            id: id.clone(),
            name: "run_sql".into(),
            arguments: format!(
                r#"{{"sql":"SELECT * FROM logs WHERE q={i} {}"}}"#,
                "x".repeat(200)
            ),
        }]));
        let out = if i == 12 {
            "ERROR: table logz does not exist".to_string()
        } else {
            format!("row {i}: {}", "y".repeat(1500))
        };
        history.push(Message::tool(out, id));
        if i == 20 {
            history.push(Message::assistant(
                "Confirmed: dots.ocr on :8765 returned 500 at 20:59:43 then went silent until 21:11:52. Decided to check retries next.".to_string(),
            ));
        }
    }
    history
}

#[test]
fn single_user_agent_task_preserves_recent_work_facts_and_dead_ends() {
    let store = Arc::new(InMemoryMemoryStore::default());
    let trimmer = DefaultContextTrimmer::with_memory(ContextConfig::default(), store.clone());
    let history = agent_task_history();
    let out = trimmer.trim_for_session(Some("s1"), &history, 32_000, 3_000, 4_096);
    let summary = compacted_summary(&out).expect("expected a compaction summary");

    // Not the degenerate goal + summary result: the last tool rounds stay
    // verbatim so the model can continue from them.
    assert!(
        out.len() >= 10,
        "compaction kept only {} messages: {out:?}",
        out.len()
    );
    assert!(
        out.iter()
            .any(|m| m.role == Role::User && m.content.contains("Investigate why 7 of 12"))
    );
    assert!(
        out.iter()
            .any(|m| m.role == Role::Tool && m.content.contains("row 39")),
        "most recent tool result was dropped"
    );
    assert!(
        out.iter()
            .any(|m| m.role == Role::Tool && m.content.contains("row 38")),
        "recent tool results were dropped"
    );

    // The confirmed fact must keep the actionable details (:8765, timestamp),
    // not be split at the dot in `dots.ocr`.
    assert!(
        summary.contains(":8765") && summary.contains("20:59:43"),
        "confirmed fact lost from summary: {summary}"
    );
    assert!(
        summary.contains("logz"),
        "failed query lost from summary: {summary}"
    );
    assert!(
        summary.contains("Work already done") && summary.contains("run_sql("),
        "work log missing from summary: {summary}"
    );

    // Facts are persisted so the `memory` tool can retrieve them later.
    let memory = store.list("s1", 200).unwrap();
    assert!(
        memory.iter().any(|e| e.content.contains(":8765")),
        "confirmed fact was not persisted: {memory:?}"
    );
    assert!(
        memory.iter().any(|e| e.content.contains("logz")),
        "dead end was not persisted: {memory:?}"
    );

    // Stay within the compact target: budget * 45%.
    let budget = (32_000u32 - 4_096 - 3_000) * 88 / 100;
    let target = budget * 45 / 100;
    assert!(
        estimate_messages(&out) <= target,
        "result {} tokens exceeds target {target}",
        estimate_messages(&out)
    );
    assert_no_orphan_tool_pairs(&out);
}

#[test]
fn compaction_prefix_is_reused_until_the_trigger_is_reached() {
    let trimmer = DefaultContextTrimmer::new(ContextConfig::default(), None);
    let mut history = agent_task_history();
    let first = trimmer.trim_for_session(Some("s"), &history, 32_000, 3_000, 4_096);
    let first_summary = compacted_summary(&first).expect("summary").to_string();
    assert!(first.len() >= 10);

    // One more tool round: well below the trigger. The compacted prefix must be
    // reused instead of regenerating the summary and sliding the tail.
    history.push(Message::tool_call(vec![ToolCallRequest {
        id: "call_new".into(),
        name: "run_sql".into(),
        arguments: r#"{"sql":"SELECT count(*) FROM logs"}"#.into(),
    }]));
    history.push(Message::tool("row: 12", "call_new"));
    let second = trimmer.trim_for_session(Some("s"), &history, 32_000, 3_000, 4_096);
    assert_eq!(
        &second[..first.len()],
        &first[..],
        "compaction re-ran instead of appending to the cached prefix"
    );
    assert_eq!(compacted_summary(&second).unwrap(), first_summary);
    assert_eq!(second.len(), first.len() + 2);

    // Grow the transcript enough to cross the trigger again: a fresh
    // compaction is expected (and the old prefix may move).
    for i in 0..40 {
        let id = format!("more_{i}");
        history.push(Message::tool_call(vec![ToolCallRequest {
            id: id.clone(),
            name: "run_sql".into(),
            arguments: format!(r#"{{"sql":"SELECT * FROM logs WHERE z={i}"}}"#),
        }]));
        history.push(Message::tool(format!("row {i}: {}", "z".repeat(1500)), id));
    }
    let third = trimmer.trim_for_session(Some("s"), &history, 32_000, 3_000, 4_096);
    assert!(
        !third.as_slice().starts_with(first.as_slice()),
        "expected a fresh compaction after crossing the trigger"
    );
    assert!(compacted_summary(&third).is_some());
    assert_no_orphan_tool_pairs(&third);
}

#[test]
fn turn_starts_handles_agent_steps_without_user_messages() {
    let history = vec![
        Message::user("task"),
        Message::tool_call(vec![ToolCallRequest {
            id: "a".into(),
            name: "grep".into(),
            arguments: "{}".into(),
        }]),
        Message::tool("result", "a"),
        Message::tool_call(vec![ToolCallRequest {
            id: "b".into(),
            name: "grep".into(),
            arguments: "{}".into(),
        }]),
        Message::tool("result", "b"),
        Message::assistant("conclusion"),
    ];
    // [user+first step], [second step], [conclusion]
    assert_eq!(turn_starts(&history), vec![0, 3, 5]);
}

#[test]
fn sub_unit_usage_samples_do_not_inflate_the_budget() {
    let trimmer = DefaultContextTrimmer::default();
    let history = vec![Message::user("a".repeat(4000))];
    let before = trimmer.estimate_tokens(Some("s"), &history);

    // Provider reports less than the heuristic. This can be a cached prompt
    // (Anthropic excludes cache tokens from input_tokens), so it must not be
    // used to increase the window budget.
    trimmer.observe_usage(Some("s"), before, before / 2);
    assert_eq!(trimmer.estimate_tokens(Some("s"), &history), before);
}
