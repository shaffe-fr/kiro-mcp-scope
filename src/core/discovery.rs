//! Discovery of Kiro projects under one or more roots.
//!
//! A project is a directory containing a `.kiro/` subdirectory. For each one,
//! its per-server state is computed via `merge`, so a glance shows where a
//! server is active and where it was missed.

use std::path::{Path, PathBuf};

use super::merge::{compute_statuses, ServerState, ServerStatus};
use super::types::Catalog;
use super::workspace::read_workspace;

#[derive(Debug, Clone)]
pub struct DiscoveredProject {
    pub name: String,
    pub dir: PathBuf,
    pub statuses: Vec<ServerStatus>,
    /// How many catalog servers are present (any non-absent state).
    pub active_count: usize,
}

fn has_kiro(dir: &Path) -> bool {
    dir.join(".kiro").is_dir()
}

/// Scan a root up to `max_depth` levels, returning directories that contain a
/// `.kiro/`. The root itself is tested at depth 0. `.kiro` and dot-directories
/// are not descended into.
pub fn scan_root(root: &Path, max_depth: u32) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, 0, max_depth, &mut found);
    found
}

fn walk(dir: &Path, depth: u32, max_depth: u32, found: &mut Vec<PathBuf>) {
    if has_kiro(dir) {
        found.push(dir.to_path_buf());
    }
    if depth >= max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        walk(&entry.path(), depth + 1, max_depth, found);
    }
}

/// Discover projects under every root, deduplicated and sorted by path.
pub fn discover_projects(
    catalog: &Catalog,
    roots: &[PathBuf],
    max_depth: u32,
) -> Vec<DiscoveredProject> {
    let mut dirs: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for root in roots {
        for dir in scan_root(root, max_depth) {
            dirs.insert(dir);
        }
    }

    dirs.into_iter()
        .map(|dir| {
            let ws = read_workspace(&dir).unwrap_or_default();
            let statuses = compute_statuses(catalog, &ws.servers);
            let active_count = statuses
                .iter()
                .filter(|s| s.state != ServerState::Absent)
                .count();
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.to_string_lossy().into_owned());
            DiscoveredProject {
                name,
                dir,
                statuses,
                active_count,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn make_project(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join(".kiro")).unwrap();
        dir
    }

    fn empty_catalog() -> Catalog {
        Catalog {
            servers: Map::new(),
        }
    }

    #[test]
    fn scan_finds_project_at_root_depth_zero() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".kiro")).unwrap();
        let found = scan_root(dir.path(), 2);
        assert_eq!(found, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn scan_finds_nested_projects_within_depth() {
        let dir = tempfile::tempdir().unwrap();
        make_project(dir.path(), "alpha");
        make_project(dir.path(), "bravo");
        let found = scan_root(dir.path(), 2);
        assert!(found.contains(&dir.path().join("alpha")));
        assert!(found.contains(&dir.path().join("bravo")));
    }

    #[test]
    fn scan_respects_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        // A project two levels down is out of reach at depth 1.
        let deep = dir.path().join("group").join("proj");
        std::fs::create_dir_all(deep.join(".kiro")).unwrap();
        assert!(scan_root(dir.path(), 1).is_empty());
        assert!(scan_root(dir.path(), 2).contains(&deep));
    }

    #[test]
    fn scan_does_not_descend_into_dot_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let hidden_child = dir.path().join(".hidden").join("proj");
        std::fs::create_dir_all(hidden_child.join(".kiro")).unwrap();
        assert!(!scan_root(dir.path(), 3).contains(&hidden_child));
    }

    #[test]
    fn discover_dedups_across_overlapping_roots() {
        let dir = tempfile::tempdir().unwrap();
        let proj = make_project(dir.path(), "proj");
        let roots = vec![dir.path().to_path_buf(), dir.path().to_path_buf()];
        let projects = discover_projects(&empty_catalog(), &roots, 2);
        let matching: Vec<_> = projects.iter().filter(|p| p.dir == proj).collect();
        assert_eq!(
            matching.len(),
            1,
            "same project found once despite overlapping roots"
        );
    }

    #[test]
    fn discover_counts_active_servers() {
        let dir = tempfile::tempdir().unwrap();
        let proj = make_project(dir.path(), "proj");
        let ws = proj.join(".kiro").join("settings");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("mcp.json"),
            r#"{ "mcpServers": { "a": { "command": "x" } } }"#,
        )
        .unwrap();

        let mut servers = Map::new();
        servers.insert("a".to_string(), serde_json::json!({ "command": "x" }));
        servers.insert("b".to_string(), serde_json::json!({ "command": "y" }));
        let catalog = Catalog { servers };

        let projects = discover_projects(&catalog, &[dir.path().to_path_buf()], 2);
        let found = projects.iter().find(|p| p.dir == proj).unwrap();
        assert_eq!(found.active_count, 1);
    }
}
