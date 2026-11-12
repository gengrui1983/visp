use super::*;

fn setup_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    Migrator::run(&conn).unwrap();
    conn
}

#[test]
fn test_migrate_creates_tables() {
    let conn = setup_db();

    // Verify session table exists
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name IN ('session', 'message', 'context_memory') ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(tables, vec!["context_memory", "message", "session"]);
}

#[test]
fn test_migrate_creates_indexes() {
    let conn = setup_db();

    let indexes: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%' ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let expected: Vec<&str> = vec![
        "idx_context_memory_session",
        "idx_message_session",
        "idx_message_session_role",
        "idx_message_tool_call",
        "idx_session_parent_id",
        "idx_session_project",
        "idx_session_updated",
    ];
    assert_eq!(indexes, expected);
}

#[test]
fn test_migrate_idempotent() {
    let conn = setup_db();
    // Second run should not error
    Migrator::run(&conn).unwrap();
    // Verify still has the expected tables (ignore sqlite_* system tables)
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 3);
}

#[test]
fn test_migrate_parent_id_index_idempotent() {
    let conn = setup_db();
    // Verify the index exists after first migration
    let idx_count_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_session_parent_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(idx_count_before, 1);

    // Second migration run should be idempotent (IF NOT EXISTS)
    Migrator::run(&conn).unwrap();

    let idx_count_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_session_parent_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(idx_count_after, 1);
}

#[test]
fn test_migrate_version() {
    let conn = setup_db();
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7);
}

#[test]
fn test_migrate_v1_to_v2() {
    // Create a v1 database first (simulate schema without tool_calls_json)
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA cache_size = -64000;
         PRAGMA user_version = 1;
         CREATE TABLE IF NOT EXISTS session (
            id TEXT PRIMARY KEY,
            project_path TEXT NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'idle',
            model TEXT NOT NULL DEFAULT '',
            system_prompt_template TEXT NOT NULL DEFAULT '',
            config_json TEXT NOT NULL DEFAULT '{}',
            approved_tools TEXT NOT NULL DEFAULT '[]',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS message (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            type TEXT NOT NULL,
            content TEXT NOT NULL DEFAULT '',
            tool_call_id TEXT,
            tool_name TEXT,
            tool_arguments TEXT,
            tool_result_is_error INTEGER,
            tool_result_duration_ms INTEGER,
            estimated_tokens INTEGER NOT NULL DEFAULT 0,
            extra_blocks TEXT,
            provider_metadata TEXT,
            actual_tokens_input INTEGER,
            actual_tokens_output INTEGER,
            actual_cache_read INTEGER,
            actual_cache_write INTEGER,
            actual_cost REAL,
            created_at INTEGER NOT NULL
         );",
    )
    .unwrap();

    // Verify version is 1
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1);

    // Run migration (should upgrade to v4)
    Migrator::run(&conn).unwrap();

    // Verify version is now 4 (migrates through v2→v3→v4 as well)
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7);

    // Verify tool_calls_json column exists
    let has_column: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('message') WHERE name='tool_calls_json'")
        .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
        .map(|c| c > 0)
        .unwrap_or(false);
    assert!(
        has_column,
        "tool_calls_json column should exist after migration"
    );
}

#[test]
fn test_migrate_v2_to_v3() {
    // Simulate a v2 database: message table WITH tool_calls_json but WITHOUT skip_context
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA cache_size = -64000;
         PRAGMA user_version = 2;
         CREATE TABLE IF NOT EXISTS session (
            id TEXT PRIMARY KEY,
            project_path TEXT NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'idle',
            model TEXT NOT NULL DEFAULT '',
            system_prompt_template TEXT NOT NULL DEFAULT '',
            config_json TEXT NOT NULL DEFAULT '{}',
            approved_tools TEXT NOT NULL DEFAULT '[]',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS message (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            type TEXT NOT NULL,
            content TEXT NOT NULL DEFAULT '',
            tool_call_id TEXT,
            tool_name TEXT,
            tool_arguments TEXT,
            tool_calls_json TEXT,
            tool_result_is_error INTEGER,
            tool_result_duration_ms INTEGER,
            estimated_tokens INTEGER NOT NULL DEFAULT 0,
            extra_blocks TEXT,
            provider_metadata TEXT,
            actual_tokens_input INTEGER,
            actual_tokens_output INTEGER,
            actual_cache_read INTEGER,
            actual_cache_write INTEGER,
            actual_cost REAL,
            created_at INTEGER NOT NULL
         );",
    )
    .unwrap();

    // Verify version is 2
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);

    // Verify skip_context column does NOT exist yet
    let has_column_before: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('message') WHERE name='skip_context'")
        .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
        .map(|c| c > 0)
        .unwrap_or(false);
    assert!(
        !has_column_before,
        "skip_context should not exist before migration"
    );

    // Insert a session and a row with raw SQL to verify default value after migration
    conn.execute(
        "INSERT INTO session (id, project_path, created_at, updated_at)
         VALUES ('test-session', '/tmp', 1700000000000, 1700000000000)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (session_id, role, type, content, created_at)
         VALUES ('test-session', 'user', 'user', 'test content', 1700000000000)",
        [],
    )
    .unwrap();

    // Run migration (should upgrade to v3→v4)
    Migrator::run(&conn).unwrap();

    // Verify version is now 5
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7);

    // Verify skip_context column exists
    let has_column: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('message') WHERE name='skip_context'")
        .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
        .map(|c| c > 0)
        .unwrap_or(false);
    assert!(
        has_column,
        "skip_context column should exist after migration"
    );

    // Verify existing data has skip_context = 0 (default)
    let skip_val: i64 = conn
        .query_row(
            "SELECT skip_context FROM message WHERE session_id = 'test-session'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(skip_val, 0, "existing rows should default to 0");
}

#[test]
fn test_migrate_v3_to_v4() {
    let conn = Connection::open_in_memory().unwrap();

    // Start at V3 by running migration then downgrading version
    Migrator::run(&conn).unwrap();
    conn.pragma_update(None, "user_version", 3i64).unwrap();

    // Insert a test row without the new columns (V3 schema)
    conn.execute(
        "INSERT INTO session (id, project_path, created_at, updated_at)
         VALUES ('test-session-v3', '/tmp', 1700000000000, 1700000000000)",
        [],
    )
    .unwrap();

    // Run migration again (should upgrade to V4)
    Migrator::run(&conn).unwrap();

    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7);

    // Verify new columns exist
    for col in &["agent_name", "parent_id", "permission_json"] {
        let has_column: bool = conn
            .prepare(&format!(
                "SELECT COUNT(*) FROM pragma_table_info('session') WHERE name='{col}'"
            ))
            .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
            .map(|c| c > 0)
            .unwrap_or(false);
        assert!(has_column, "{col} column should exist after V4 migration");
    }

    // Verify existing data has defaults
    let agent_name: String = conn
        .query_row(
            "SELECT agent_name FROM session WHERE id = 'test-session-v3'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        agent_name, "default",
        "existing rows should default to 'default'"
    );

    let parent_id: Option<String> = conn
        .query_row(
            "SELECT parent_id FROM session WHERE id = 'test-session-v3'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        parent_id.is_none(),
        "existing rows should have NULL parent_id"
    );

    let permission_json: String = conn
        .query_row(
            "SELECT permission_json FROM session WHERE id = 'test-session-v3'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        permission_json, "[]",
        "existing rows should default to '[]'"
    );
}

#[test]
fn test_migrate_pragma() {
    let conn = Connection::open_in_memory().unwrap();
    Migrator::run(&conn).unwrap();

    // journal_mode should be WAL (in-memory may differ, so just check no error)
    let _journal_mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    // In-memory databases may not support WAL, so just verify it ran
    // synchronous
    let sync: i64 = conn
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .unwrap();
    assert_eq!(sync, 1); // NORMAL = 1

    // foreign_keys
    let fk: bool = conn
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .unwrap();
    assert!(fk);
}

#[test]
fn test_migrate_self_heals_when_version_exceeds_schema() {
    // Regression: a DB whose user_version already equals VERSION but whose
    // message table is missing the tool_call_count column (e.g. version
    // was bumped by a prior partial run). The migrator must repair the
    // schema instead of skipping.
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "PRAGMA user_version = 4;
         CREATE TABLE IF NOT EXISTS session (
            id TEXT PRIMARY KEY,
            project_path TEXT NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'idle',
            model TEXT NOT NULL DEFAULT '',
            system_prompt_template TEXT NOT NULL DEFAULT '',
            config_json TEXT NOT NULL DEFAULT '{}',
            approved_tools TEXT NOT NULL DEFAULT '[]',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS message (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            type TEXT NOT NULL,
            content TEXT NOT NULL DEFAULT '',
            tool_call_id TEXT,
            tool_name TEXT,
            tool_arguments TEXT,
            tool_calls_json TEXT,
            tool_result_is_error INTEGER,
            tool_result_duration_ms INTEGER,
            estimated_tokens INTEGER NOT NULL DEFAULT 0,
            extra_blocks TEXT,
            provider_metadata TEXT,
            actual_tokens_input INTEGER,
            actual_tokens_output INTEGER,
            actual_cache_read INTEGER,
            actual_cache_write INTEGER,
            actual_cost REAL,
            skip_context INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_message_session ON message(session_id, id);
         CREATE INDEX IF NOT EXISTS idx_message_session_role ON message(session_id, role);
         CREATE INDEX IF NOT EXISTS idx_message_tool_call ON message(tool_call_id);
         CREATE INDEX IF NOT EXISTS idx_session_project ON session(project_path, created_at);
         CREATE INDEX IF NOT EXISTS idx_session_updated ON session(updated_at);",
    )
    .unwrap();

    // Before migration: tool_call_count column is missing
    let has_before: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('message') WHERE name='tool_call_count'")
        .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
        .map(|c| c > 0)
        .unwrap_or(false);
    assert!(
        !has_before,
        "tool_call_count should be missing before self-heal"
    );

    // Run migration — with the old code this early-returns because
    // user_version (4) >= VERSION (4), leaving the column missing.
    Migrator::run(&conn).unwrap();

    // After migration: tool_call_count column exists (self-healed)
    let has_after: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('message') WHERE name='tool_call_count'")
        .and_then(|mut s| s.query_row([], |row| row.get::<_, i64>(0)))
        .map(|c| c > 0)
        .unwrap_or(false);
    assert!(has_after, "tool_call_count should be added by self-heal");
}
