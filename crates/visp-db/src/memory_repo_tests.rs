use super::*;
use rusqlite::Connection;
use visp_core::memory::{MemoryKind, MemoryStore, NewMemoryEntry};

fn insert_session(conn: &Connection, id: &str) {
    conn.execute(
        "INSERT INTO session (id, project_path, created_at, updated_at) VALUES (?1, '/tmp', 0, 0)",
        rusqlite::params![id],
    )
    .unwrap();
}

fn setup() -> SqliteMemoryStore {
    SqliteMemoryStore::open_in_memory().unwrap()
}

#[test]
fn append_search_list_roundtrip() {
    let store = setup();
    insert_session(&store.conn_for_tests(), "s1");

    let fact = store
        .append(NewMemoryEntry::new(
            "s1",
            MemoryKind::Fact,
            "pool is leaking",
        ))
        .unwrap();
    let decision = store
        .append(NewMemoryEntry::new(
            "s1",
            MemoryKind::Decision,
            "add Drop impl",
        ))
        .unwrap();
    assert!(fact.id > 0);
    assert!(decision.id > fact.id);

    let found = store.search("s1", Some("LEAK"), 10).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].content, "pool is leaking");
    assert_eq!(found[0].kind, MemoryKind::Fact);

    let all = store.list("s1", 10).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].kind, MemoryKind::Fact, "list is chronological");
    assert_eq!(store.count("s1").unwrap(), 2);
}

#[test]
fn append_is_idempotent_for_identical_entries() {
    let store = setup();
    insert_session(&store.conn_for_tests(), "s1");

    let a = store
        .append(NewMemoryEntry::new("s1", MemoryKind::Fact, "same fact"))
        .unwrap();
    let b = store
        .append(NewMemoryEntry::new("s1", MemoryKind::Fact, "same fact"))
        .unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(store.count("s1").unwrap(), 1);
}

#[test]
fn entries_are_isolated_per_session_and_ordered() {
    let store = setup();
    insert_session(&store.conn_for_tests(), "s1");
    insert_session(&store.conn_for_tests(), "s2");

    for i in 0..5 {
        store
            .append(NewMemoryEntry::new(
                "s1",
                MemoryKind::Work,
                format!("step {i}"),
            ))
            .unwrap();
    }
    store
        .append(NewMemoryEntry::new("s2", MemoryKind::Fact, "other"))
        .unwrap();

    let recent = store.list("s1", 2).unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].content, "step 3");
    assert_eq!(recent[1].content, "step 4");

    assert_eq!(store.count("s2").unwrap(), 1);
}

#[test]
fn deleting_session_cascades_memory() {
    let store = setup();
    insert_session(&store.conn_for_tests(), "s1");
    store
        .append(NewMemoryEntry::new("s1", MemoryKind::Fact, "gone soon"))
        .unwrap();

    store
        .conn_for_tests()
        .execute("DELETE FROM session WHERE id = 's1'", [])
        .unwrap();
    assert_eq!(store.count("s1").unwrap(), 0);
}

#[test]
fn search_escapes_like_wildcards() {
    let store = setup();
    insert_session(&store.conn_for_tests(), "s1");
    store
        .append(NewMemoryEntry::new("s1", MemoryKind::Fact, "100% done"))
        .unwrap();
    store
        .append(NewMemoryEntry::new("s1", MemoryKind::Fact, "plain"))
        .unwrap();

    let found = store.search("s1", Some("100%"), 10).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].content, "100% done");
}
