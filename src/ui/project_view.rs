//! Project view state and transitions, independent of the terminal.
//!
//! The primary path of the tool: see the catalog and choose what to take for
//! this project. A checked box means "present in this project" — not "switched
//! on now", which is Kiro's job. Toggles are staged in memory; `save` applies
//! them to the workspace file through the same core the CLI uses.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::core::merge::{
    apply_toggle, compute_statuses, OnDiverge, ServerState, ServerStatus, Toggle,
};
use crate::core::types::Catalog;
use crate::core::workspace::{read_workspace, write_workspace, WorkspaceFile, WriteError};

/// One row: a catalog server and the user's staged intent for it.
#[derive(Debug, Clone)]
pub struct Row {
    pub status: ServerStatus,
    /// Whether the box is checked: present after the pending edits are applied.
    pub selected: bool,
    /// For a diverged row, how a save resolves it.
    pub on_diverge: OnDiverge,
}

impl Row {
    pub fn is_diverged(&self) -> bool {
        self.status.state == ServerState::Diverged
    }
}

/// The project view: rows derived from catalog vs workspace, a cursor, and a
/// dirty flag once the staged selection departs from what is on disk.
#[derive(Debug, Clone)]
pub struct ProjectView {
    project_dir: PathBuf,
    catalog: Catalog,
    /// The workspace as last read/saved. Rows are compared against it for dirt.
    baseline: WorkspaceFile,
    pub rows: Vec<Row>,
    pub cursor: usize,
    pub message: Option<String>,
}

fn rows_from(catalog: &Catalog, workspace: &WorkspaceFile) -> Vec<Row> {
    compute_statuses(catalog, &workspace.servers)
        .into_iter()
        .map(|status| Row {
            selected: status.state != ServerState::Absent,
            on_diverge: OnDiverge::Keep,
            status,
        })
        .collect()
}

impl ProjectView {
    /// Build a view from the catalog and the project's current workspace.
    pub fn new(project_dir: PathBuf, catalog: Catalog, workspace: WorkspaceFile) -> Self {
        let rows = rows_from(&catalog, &workspace);
        Self {
            project_dir,
            catalog,
            baseline: workspace,
            rows,
            cursor: 0,
            message: None,
        }
    }

    /// Load the view from disk.
    pub fn load(project_dir: PathBuf, catalog: Catalog) -> std::io::Result<Self> {
        let workspace = read_workspace(&project_dir)?;
        Ok(Self::new(project_dir, catalog, workspace))
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as isize;
        self.cursor = (self.cursor as isize + delta).rem_euclid(len) as usize;
    }

    pub fn set_cursor(&mut self, index: usize) {
        if index < self.rows.len() {
            self.cursor = index;
        }
    }

    /// Check/uncheck the row under the cursor.
    pub fn toggle(&mut self) {
        if let Some(row) = self.rows.get_mut(self.cursor) {
            row.selected = !row.selected;
            self.message = None;
        }
    }

    /// Flip how the row under the cursor resolves its divergence. No effect on a
    /// non-diverged row.
    pub fn cycle_divergence(&mut self) {
        if let Some(row) = self.rows.get_mut(self.cursor) {
            if row.is_diverged() {
                row.on_diverge = match row.on_diverge {
                    OnDiverge::Keep => OnDiverge::Catalog,
                    OnDiverge::Catalog => OnDiverge::Keep,
                };
                self.message = None;
            }
        }
    }

    /// The toggles that the current selection implies against the baseline.
    fn pending_toggles(&self) -> Vec<Toggle> {
        self.rows
            .iter()
            .filter_map(|row| {
                let present = self.baseline.servers.contains_key(&row.status.name);
                let diverged_resolving =
                    row.selected && row.is_diverged() && row.on_diverge == OnDiverge::Catalog;
                if row.selected == present && !diverged_resolving {
                    return None;
                }
                Some(Toggle {
                    name: row.status.name.clone(),
                    enabled: row.selected,
                    on_diverge: row.on_diverge,
                })
            })
            .collect()
    }

    /// Whether the staged selection differs from what is on disk.
    pub fn is_dirty(&self) -> bool {
        !self.pending_toggles().is_empty()
    }

    /// Apply the staged selection to the workspace file. Rebuilds rows from the
    /// written state so the view reflects disk. A blocked write (secret) leaves
    /// the file untouched and surfaces the reason.
    pub fn save(&mut self) -> Result<(), WriteError> {
        let toggles = self.pending_toggles();
        let mut servers: Map<String, Value> = self.baseline.servers.clone();
        for toggle in &toggles {
            apply_toggle(&self.catalog, &mut servers, toggle);
        }
        let next = WorkspaceFile {
            servers,
            extra: self.baseline.extra.clone(),
        };
        write_workspace(&self.project_dir, &next)?;
        self.baseline = next;
        let cursor = self.cursor;
        self.rows = rows_from(&self.catalog, &self.baseline);
        self.cursor = cursor.min(self.rows.len().saturating_sub(1));
        self.message = Some(format!("Saved {} change(s).", toggles.len()));
        Ok(())
    }

    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(entries: &[(&str, Value)]) -> Catalog {
        let mut servers = Map::new();
        for (name, entry) in entries {
            servers.insert((*name).to_string(), entry.clone());
        }
        Catalog { servers }
    }

    fn workspace(entries: &[(&str, Value)]) -> WorkspaceFile {
        let mut servers = Map::new();
        for (name, entry) in entries {
            servers.insert((*name).to_string(), entry.clone());
        }
        WorkspaceFile {
            servers,
            extra: Map::new(),
        }
    }

    #[test]
    fn rows_reflect_presence() {
        let cat = catalog(&[
            ("a", serde_json::json!({ "command": "x" })),
            ("b", serde_json::json!({ "command": "y" })),
        ]);
        let ws = workspace(&[("a", serde_json::json!({ "command": "x" }))]);
        let view = ProjectView::new(PathBuf::from("/p"), cat, ws);
        let a = view.rows.iter().find(|r| r.status.name == "a").unwrap();
        let b = view.rows.iter().find(|r| r.status.name == "b").unwrap();
        assert!(a.selected);
        assert!(!b.selected);
    }

    #[test]
    fn toggle_marks_dirty() {
        let cat = catalog(&[("a", serde_json::json!({ "command": "x" }))]);
        let mut view = ProjectView::new(PathBuf::from("/p"), cat, workspace(&[]));
        assert!(!view.is_dirty());
        view.toggle();
        assert!(view.is_dirty());
        view.toggle();
        assert!(!view.is_dirty(), "toggling back clears dirt");
    }

    #[test]
    fn cursor_wraps() {
        let cat = catalog(&[
            ("a", serde_json::json!({ "command": "x" })),
            ("b", serde_json::json!({ "command": "y" })),
        ]);
        let mut view = ProjectView::new(PathBuf::from("/p"), cat, workspace(&[]));
        assert_eq!(view.cursor, 0);
        view.move_cursor(-1);
        assert_eq!(view.cursor, 1, "wraps to last");
        view.move_cursor(1);
        assert_eq!(view.cursor, 0, "wraps to first");
    }

    #[test]
    fn save_activates_selected_server() {
        let dir = tempfile::tempdir().unwrap();
        let cat = catalog(&[("a", serde_json::json!({ "command": "x", "args": ["1"] }))]);
        let mut view = ProjectView::load(dir.path().to_path_buf(), cat).unwrap();
        view.set_cursor(0);
        view.toggle();
        view.save().unwrap();

        let ws = read_workspace(dir.path()).unwrap();
        assert!(ws.servers.contains_key("a"));
        assert!(!view.is_dirty(), "clean after save");
    }

    #[test]
    fn save_removes_deselected_server() {
        let dir = tempfile::tempdir().unwrap();
        let ws_path = dir.path().join(".kiro").join("settings").join("mcp.json");
        std::fs::create_dir_all(ws_path.parent().unwrap()).unwrap();
        std::fs::write(&ws_path, r#"{ "mcpServers": { "a": { "command": "x" } } }"#).unwrap();

        let cat = catalog(&[("a", serde_json::json!({ "command": "x" }))]);
        let mut view = ProjectView::load(dir.path().to_path_buf(), cat).unwrap();
        assert!(view.rows[0].selected);
        view.toggle();
        view.save().unwrap();

        let ws = read_workspace(dir.path()).unwrap();
        assert!(!ws.servers.contains_key("a"));
    }

    #[test]
    fn diverged_row_starts_on_keep_and_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let ws_path = dir.path().join(".kiro").join("settings").join("mcp.json");
        std::fs::create_dir_all(ws_path.parent().unwrap()).unwrap();
        std::fs::write(
            &ws_path,
            r#"{ "mcpServers": { "a": { "command": "x", "args": ["local"] } } }"#,
        )
        .unwrap();
        let cat = catalog(&[(
            "a",
            serde_json::json!({ "command": "x", "args": ["catalog"] }),
        )]);
        let mut view = ProjectView::load(dir.path().to_path_buf(), cat).unwrap();

        assert!(view.rows[0].is_diverged());
        assert_eq!(view.rows[0].on_diverge, OnDiverge::Keep);
        // Keep on a present, selected, diverged row is a no-op: not dirty.
        assert!(!view.is_dirty());

        view.set_cursor(0);
        view.cycle_divergence();
        assert_eq!(view.rows[0].on_diverge, OnDiverge::Catalog);
        assert!(view.is_dirty(), "resolving to catalog is a pending change");

        view.save().unwrap();
        let ws = read_workspace(dir.path()).unwrap();
        assert_eq!(ws.servers["a"]["args"], serde_json::json!(["catalog"]));
    }

    #[test]
    fn save_blocked_on_secret_surfaces_error_and_keeps_file() {
        let dir = tempfile::tempdir().unwrap();
        let cat = catalog(&[(
            "a",
            serde_json::json!({ "command": "x", "env": { "AUTH_HEADER": "literal-secret-value" } }),
        )]);
        let mut view = ProjectView::load(dir.path().to_path_buf(), cat).unwrap();
        view.set_cursor(0);
        view.toggle();
        let err = view.save().unwrap_err();
        assert!(matches!(err, WriteError::Secret(_)));
        assert!(
            !view.project_dir().join(".kiro").exists(),
            "nothing written"
        );
    }
}
