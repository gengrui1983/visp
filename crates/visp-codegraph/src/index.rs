use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use walkdir::WalkDir;

use crate::graph::Edge;
use crate::parser::Parser;
use crate::store::Store;

/// Configuration for indexing a codebase
pub struct CodeGraphConfig {
    pub exclude_dirs: Vec<String>,
    pub supported_extensions: Vec<String>,
}

impl Default for CodeGraphConfig {
    fn default() -> Self {
        Self {
            exclude_dirs: vec![
                "node_modules".into(),
                ".git".into(),
                "dist".into(),
                "build".into(),
                "target".into(),
                ".next".into(),
                ".nuxt".into(),
                "coverage".into(),
                "__pycache__".into(),
                ".venv".into(),
                "venv".into(),
                "vendor".into(),
                ".cache".into(),
                "out".into(),
            ],
            supported_extensions: vec![
                ".ts".into(),
                ".tsx".into(),
                ".rs".into(),
                ".py".into(),
                ".c".into(),
                ".h".into(),
                ".cpp".into(),
                ".hpp".into(),
                ".cc".into(),
                ".go".into(),
            ],
        }
    }
}

/// Map a file extension to a human-readable language name.
fn language_name(rel_path: &str) -> &'static str {
    if rel_path.ends_with(".rs") {
        "rust"
    } else if rel_path.ends_with(".py") {
        "python"
    } else if rel_path.ends_with(".ts") || rel_path.ends_with(".tsx") {
        "typescript"
    } else if rel_path.ends_with(".c") || rel_path.ends_with(".h") {
        "c"
    } else if rel_path.ends_with(".cpp") || rel_path.ends_with(".hpp") || rel_path.ends_with(".cc")
    {
        "cpp"
    } else if rel_path.ends_with(".go") {
        "go"
    } else {
        "unknown"
    }
}

/// Types of file system events for incremental indexing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEvent {
    Created,
    Modified,
    Removed,
}

/// Indexes a codebase into a graph database, supporting full builds and
/// incremental updates with cross-file symbol resolution.
pub struct Indexer {
    store: Arc<Store>,
}

impl Indexer {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Perform a full build: walk the project directory, parse all supported
    /// files, insert symbols/edges/imports/exports, and resolve cross-file edges.
    pub fn build_full(
        &self,
        project_path: &Path,
        config: &CodeGraphConfig,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let files = collect_files(project_path, config)?;

        let mut parser = Parser::new()?;

        for file_path in &files {
            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Skipping {}: {}", file_path.display(), e);
                    continue;
                }
            };

            let rel_path = file_path
                .strip_prefix(project_path)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string();

            let parse_result = match parser.parse_file(&rel_path, &content) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Skipping {}: {}", rel_path, e);
                    continue;
                }
            };

            // Insert symbols first to get real DB ids
            let old_ids: Vec<u64> = parse_result.symbols.iter().map(|s| s.id).collect();
            let mut symbols = parse_result.symbols;
            self.store.insert_symbols(&mut symbols)?;

            let id_map: HashMap<u64, u64> = old_ids
                .into_iter()
                .zip(symbols.iter().map(|s| s.id))
                .collect();

            // Remap edge source_ids from temp → real.
            // Skip edges whose source_id wasn't mapped (temp id 0 with no
            // real counterpart – top-level calls outside any symbol).
            let edges: Vec<Edge> = parse_result
                .edges
                .into_iter()
                .filter_map(|mut e| {
                    if let Some(&new_id) = id_map.get(&e.source_id) {
                        e.source_id = new_id;
                        Some(e)
                    } else {
                        None
                    }
                })
                .collect();
            self.store.insert_edges(&edges)?;

            self.store
                .insert_imports(&rel_path, &parse_result.imports)?;
            self.store
                .insert_exports(&rel_path, &parse_result.exports)?;

            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            self.store.upsert_file(
                &rel_path,
                language_name(&rel_path),
                symbols.len() as u32,
                ts,
            )?;
        }

        // Cross-file resolution
        resolve_cross_file_edges(&self.store)?;

        Ok(())
    }

    /// Incrementally update the index for a single file change.
    ///
    /// For `Created`/`Modified`: removes old data for the file, re-parses and
    /// inserts fresh data, then re-resolves cross-file edges.
    /// For `Removed`: deletes the file's symbols and file record.
    pub fn update_file(
        &self,
        project_path: &Path,
        file_path: &Path,
        event: FileEvent,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let rel_path = file_path
            .strip_prefix(project_path)
            .unwrap_or(file_path)
            .to_string_lossy()
            .to_string();

        match event {
            FileEvent::Created | FileEvent::Modified => {
                // Remove old data（symbols / edges / imports / exports / files）。
                self.store.delete_file_data(&rel_path)?;

                // Parse the file
                let content = std::fs::read_to_string(file_path)?;
                let mut parser = Parser::new()?;
                let parse_result = parser.parse_file(&rel_path, &content)?;

                // Insert symbols
                let old_ids: Vec<u64> = parse_result.symbols.iter().map(|s| s.id).collect();
                let mut symbols = parse_result.symbols;
                self.store.insert_symbols(&mut symbols)?;

                let id_map: HashMap<u64, u64> = old_ids
                    .into_iter()
                    .zip(symbols.iter().map(|s| s.id))
                    .collect();

                // Remap edges (skip unmapped – see build_full)
                let edges: Vec<Edge> = parse_result
                    .edges
                    .into_iter()
                    .filter_map(|mut e| {
                        if let Some(&new_id) = id_map.get(&e.source_id) {
                            e.source_id = new_id;
                            Some(e)
                        } else {
                            None
                        }
                    })
                    .collect();
                self.store.insert_edges(&edges)?;

                self.store
                    .insert_imports(&rel_path, &parse_result.imports)?;
                self.store
                    .insert_exports(&rel_path, &parse_result.exports)?;

                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                self.store.upsert_file(
                    &rel_path,
                    language_name(&rel_path),
                    symbols.len() as u32,
                    ts,
                )?;

                // Re-resolve cross-file edges
                resolve_cross_file_edges(&self.store)?;
            }
            FileEvent::Removed => {
                // 彻底删除旧路径的全部索引痕迹（重命名旧路径不再遗留 dangling 行）。
                self.store.delete_file_data(&rel_path)?;
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
//  Helpers
// ---------------------------------------------------------------------------

/// Collect all supported files under `project_path`, excluding ignored dirs.
fn collect_files(
    project_path: &Path,
    config: &CodeGraphConfig,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut files = Vec::new();

    for entry in WalkDir::new(project_path)
        .into_iter()
        .filter_entry(|e| {
            // Prevent descending into excluded directories
            if e.file_type().is_dir()
                && let Some(name) = e.file_name().to_str()
            {
                return !config.exclude_dirs.iter().any(|d| d == name);
            }
            true
        })
        .filter_map(|e| e.ok())
    {
        let path = entry.path();

        if !entry.file_type().is_file() {
            continue;
        }

        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let dot_ext = format!(".{}", ext);
            if config.supported_extensions.contains(&dot_ext)
                || config.supported_extensions.contains(&ext.to_string())
            {
                files.push(path.to_path_buf());
            }
        }
    }

    files.sort();
    Ok(files)
}

/// Resolve all unresolved edges by matching target_name against known symbols.
///
/// For each unresolved edge (target_id IS NULL, target_name IS NOT NULL),
/// look up symbols with that name project-wide and resolve the edge.
fn resolve_cross_file_edges(store: &Store) -> Result<(), Box<dyn std::error::Error>> {
    let unresolved = store.get_unresolved_edges()?;
    for (edge_id, target_name) in &unresolved {
        let candidates = store.get_symbols_by_name(target_name)?;
        if let Some(sym) = candidates.first() {
            store.resolve_edge(*edge_id, sym.id)?;
        }
    }
    Ok(())
}

/// Resolve an import source path to an actual file path within the project.
///
/// Given an import like `./bar` from `src/a.ts`, tries:
/// - `src/bar` (exact), `src/bar.ts`, `src/bar.tsx`
/// - `src/bar/index.ts`, `src/bar/index.tsx`
///
/// Returns `None` if no matching file is found.
#[allow(dead_code)]
fn resolve_import_source(importer_dir: &Path, import_source: &str) -> Option<PathBuf> {
    let base = importer_dir.join(import_source);

    let trials = [
        base.clone(),
        base.with_extension("ts"),
        base.with_extension("tsx"),
        base.join("index.ts"),
        base.join("index.tsx"),
    ];

    for p in &trials {
        if p.exists() && p.is_file() {
            return Some(p.to_path_buf());
        }
    }

    None
}

// ---------------------------------------------------------------------------
//  Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "index_tests.rs"]
mod index_tests;
