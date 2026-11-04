#![cfg(test)]
use crate::index::*;
use crate::store::Store;
use std::path::Path;
use std::sync::Arc;

fn create_store(dir: &tempfile::TempDir) -> Store {
    let db_path = dir.path().join("test.db");
    Store::open(&db_path).expect("Failed to open store")
}

fn create_indexer(store: &Arc<Store>) -> Indexer {
    Indexer::new(store.clone())
}

fn default_config() -> CodeGraphConfig {
    CodeGraphConfig {
        exclude_dirs: vec![
            "node_modules".into(),
            ".git".into(),
            "dist".into(),
            "build".into(),
        ],
        supported_extensions: vec![".ts".into(), ".tsx".into()],
    }
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let full = root.join(rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&full, content).unwrap();
}

// ------------------------------------------------------------------
//  Step 5a: Full build tests
// ------------------------------------------------------------------

#[test]
fn test_full_build_basic() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    write_file(&project, "a.ts", "export function foo() { return 1; }\n");
    write_file(&project, "b.ts", "function bar() { return 2; }\n");
    write_file(
        &project,
        "c.ts",
        "export const baz = () => { return 3; };\n",
    );

    let store = Arc::new(create_store(&tmp));
    let indexer = create_indexer(&store);
    let config = default_config();

    indexer.build_full(&project, &config).unwrap();

    let symbols = store.search_symbols("", 100).unwrap();
    assert_eq!(symbols.len(), 3);
}

#[test]
fn test_full_build_excludes_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(project.join("node_modules")).unwrap();
    std::fs::create_dir_all(project.join("src")).unwrap();

    write_file(&project, "src/valid.ts", "export function hello() {}\n");
    write_file(
        &project,
        "node_modules/ignored.ts",
        "export function ignored() {}\n",
    );

    let store = Arc::new(create_store(&tmp));
    let indexer = create_indexer(&store);
    let config = default_config();

    indexer.build_full(&project, &config).unwrap();

    let symbols = store.search_symbols("", 100).unwrap();
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].name, "hello");
}

#[test]
fn test_collect_files_excludes_heavy_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    write_file(&project, "node_modules/foo.ts", "export function a() {}\n");
    write_file(&project, "target/gen.ts", "export function b() {}\n");
    write_file(&project, "src/a.ts", "export function c() {}\n");

    let files = collect_files(&project, &CodeGraphConfig::default()).unwrap();
    let rel: Vec<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(&project)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(rel, vec!["src/a.ts".to_string()]);
}

#[test]
fn test_default_config_excludes_heavy_dirs() {
    let config = CodeGraphConfig::default();
    assert!(config.exclude_dirs.iter().any(|d| d == "node_modules"));
    assert!(config.exclude_dirs.iter().any(|d| d == "target"));
}

#[test]
fn test_full_build_cross_file_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    write_file(&project, "a.ts", "export function foo() {}\n");
    write_file(&project, "b.ts", "import { foo } from './a';\nfoo();\n");

    let store = Arc::new(create_store(&tmp));
    let indexer = create_indexer(&store);
    let config = default_config();

    indexer.build_full(&project, &config).unwrap();

    let foo_symbols = store.get_symbols_by_name("foo").unwrap();
    assert!(!foo_symbols.is_empty(), "foo symbol should exist");

    let unresolved = store.get_unresolved_edges().unwrap();
    assert!(
        unresolved.is_empty(),
        "all edges should be resolved, got {} unresolved",
        unresolved.len()
    );
}

#[test]
fn test_full_build_skip_parse_error() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    write_file(&project, "valid.ts", "export function fine() {}\n");
    write_file(&project, "broken.ts", "export function { bad syntax here\n");

    let store = Arc::new(create_store(&tmp));
    let indexer = create_indexer(&store);
    let config = default_config();

    indexer.build_full(&project, &config).unwrap();

    let symbols = store.search_symbols("", 100).unwrap();
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].name, "fine");
}

// ------------------------------------------------------------------
//  Step 5b: Incremental update tests
// ------------------------------------------------------------------

#[test]
fn test_incremental_add_file() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    write_file(&project, "a.ts", "export function existing() {}\n");

    let store = Arc::new(create_store(&tmp));
    let indexer = create_indexer(&store);
    let config = default_config();

    indexer.build_full(&project, &config).unwrap();

    // Add a new file
    let new_path = project.join("b.ts");
    write_file(&project, "b.ts", "export function added() {}\n");
    indexer
        .update_file(&project, &new_path, FileEvent::Created)
        .unwrap();

    let symbols = store.search_symbols("", 100).unwrap();
    assert_eq!(symbols.len(), 2);
}

#[test]
fn test_incremental_modify_file() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    let file_path = project.join("a.ts");
    write_file(&project, "a.ts", "export function original() {}\n");

    let store = Arc::new(create_store(&tmp));
    let indexer = create_indexer(&store);
    let config = default_config();

    indexer.build_full(&project, &config).unwrap();

    // Replace symbol
    write_file(&project, "a.ts", "export function modified() {}\n");
    indexer
        .update_file(&project, &file_path, FileEvent::Modified)
        .unwrap();

    let symbols = store.search_symbols("", 100).unwrap();
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(!names.contains(&"original"), "original should be gone");
    assert!(names.contains(&"modified"), "modified should exist");
}

#[test]
fn test_incremental_delete_file() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();

    write_file(&project, "a.ts", "export function keep() {}\n");
    let file_to_delete = project.join("b.ts");
    write_file(&project, "b.ts", "export function remove() {}\n");

    let store = Arc::new(create_store(&tmp));
    let indexer = create_indexer(&store);
    let config = default_config();

    indexer.build_full(&project, &config).unwrap();

    // Delete b.ts
    std::fs::remove_file(&file_to_delete).unwrap();
    indexer
        .update_file(&project, &file_to_delete, FileEvent::Removed)
        .unwrap();

    let symbols = store.search_symbols("", 100).unwrap();
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(symbols.len(), 1);
    assert!(names.contains(&"keep"));
    assert!(!names.contains(&"remove"));
}
