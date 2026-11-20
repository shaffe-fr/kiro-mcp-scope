//! Matrix view state and 2D navigation, independent of the terminal.
//!
//! Projects are rows, catalog servers columns. The cursor moves in two
//! dimensions; a cell is toggled by keyboard or by clicking it. Hit-testing is
//! derived from the name-column width and the per-column step, kept pure here so
//! it can be tested without a terminal.
//!
//! Worktrees sit under their project and are folded away by default. A project
//! unfolds by itself as soon as one of its worktrees diverges from the catalog
//! or has no config, so nothing that needs a look stays hidden; `e` folds or
//! unfolds one by hand.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::core::matrix::Matrix;
use crate::core::sync::init_worktree_now;
use crate::core::types::Catalog;
use crate::core::workspace::WriteError;

/// Width of the left column holding project names, in cells.
pub const NAME_COL_WIDTH: u16 = 30;
/// Width of each server column (checkbox + padding).
pub const CELL_STEP: u16 = 5;
/// Rows before the first project row: title, blank, header row, blank.
pub const HEADER_ROWS: u16 = 4;

/// Where `i` (initialize a worktree from its parent) keeps its state and
/// backups.
#[derive(Debug, Clone)]
pub struct InitPaths {
    pub state: PathBuf,
    pub backups: PathBuf,
}

pub struct MatrixView {
    pub matrix: Matrix,
    catalog: Catalog,
    /// Project cursor, as a row of `matrix.projects`.
    pub project_cursor: usize,
    pub server_cursor: usize,
    pub message: Option<String>,
    /// Lines shown under the grid: copies outside it that differ.
    pub outside: Vec<String>,
    init_paths: Option<InitPaths>,
    /// Folds the user chose; absent means "automatic".
    manual_fold: HashMap<usize, bool>,
    visible: Vec<usize>,
}

impl MatrixView {
    pub fn new(catalog: Catalog, matrix: Matrix) -> Self {
        let mut view = Self {
            matrix,
            catalog,
            project_cursor: 0,
            server_cursor: 0,
            message: None,
            outside: Vec::new(),
            init_paths: None,
            manual_fold: HashMap::new(),
            visible: Vec::new(),
        };
        view.refresh_visible();
        view
    }

    /// Enable the `i` action with the given state and backup locations.
    pub fn with_init_paths(mut self, paths: InitPaths) -> Self {
        self.init_paths = Some(paths);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.matrix.project_count() == 0 || self.matrix.server_count() == 0
    }

    /// Whether the worktrees of `project` are shown.
    pub fn is_expanded(&self, project: usize) -> bool {
        if let Some(&chosen) = self.manual_fold.get(&project) {
            return chosen;
        }
        self.matrix
            .worktrees_of(project)
            .into_iter()
            .any(|child| self.matrix.needs_attention(child))
    }

    fn refresh_visible(&mut self) {
        let mut visible = Vec::new();
        for p in 0..self.matrix.project_count() {
            match self.matrix.projects[p].parent {
                None => visible.push(p),
                Some(parent) if self.is_expanded(parent) => visible.push(p),
                Some(_) => {}
            }
        }
        self.visible = visible;
        if !self.visible.contains(&self.project_cursor) {
            self.project_cursor = self
                .matrix
                .projects
                .get(self.project_cursor)
                .and_then(|p| p.parent)
                .filter(|parent| self.visible.contains(parent))
                .or_else(|| self.visible.first().copied())
                .unwrap_or(0);
        }
    }

    /// Rows currently shown, top to bottom, as indexes into the matrix.
    pub fn visible_rows(&self) -> &[usize] {
        &self.visible
    }

    pub fn move_project(&mut self, delta: isize) {
        let n = self.visible.len();
        if n == 0 {
            return;
        }
        let at = self
            .visible
            .iter()
            .position(|&p| p == self.project_cursor)
            .unwrap_or(0);
        let next = (at as isize + delta).rem_euclid(n as isize) as usize;
        self.project_cursor = self.visible[next];
    }

    pub fn move_server(&mut self, delta: isize) {
        let n = self.matrix.server_count();
        if n == 0 {
            return;
        }
        self.server_cursor = (self.server_cursor as isize + delta).rem_euclid(n as isize) as usize;
    }

    pub fn toggle_cursor(&mut self) {
        self.matrix.toggle(self.project_cursor, self.server_cursor);
        self.message = None;
    }

    /// Fold or unfold. On a worktree, folds its project and moves up to it.
    pub fn toggle_fold(&mut self) {
        let here = self.project_cursor;
        let target = self
            .matrix
            .projects
            .get(here)
            .and_then(|p| p.parent)
            .unwrap_or(here);
        if self.matrix.worktrees_of(target).is_empty() {
            self.message = Some("No worktrees under this project.".to_string());
            return;
        }
        let expanded = self.is_expanded(target);
        self.manual_fold.insert(target, !expanded);
        self.project_cursor = target;
        self.refresh_visible();
        self.message = None;
    }

    /// What the row under the cursor allows, said where it applies: the keys
    /// are explained next to the thing they act on, not in a legend to decode.
    pub fn row_hint(&self) -> Option<String> {
        let info = self.matrix.projects.get(self.project_cursor)?;
        if let Some(parent) = info.parent {
            let parent_name = &self.matrix.projects[parent].name;
            return Some(if info.has_config {
                format!("worktree of {parent_name} — e: fold")
            } else {
                format!("no config — i: write it from {parent_name}")
            });
        }
        let count = self.matrix.worktrees_of(self.project_cursor).len();
        if count == 0 {
            return None;
        }
        let shown = self.is_expanded(self.project_cursor);
        Some(format!(
            "{count} worktree(s) {} — e: {}",
            if shown { "shown" } else { "hidden" },
            if shown { "hide" } else { "show" }
        ))
    }

    /// Write the cursor worktree's config from its parent, when it has none.
    pub fn init_worktree(&mut self) {
        let Some(paths) = self.init_paths.clone() else {
            return;
        };
        let p = self.project_cursor;
        let Some(info) = self.matrix.projects.get(p) else {
            return;
        };
        if info.parent.is_none() || info.has_config {
            self.message =
                Some("Nothing to initialize: select a worktree with no config.".to_string());
            return;
        }
        let (name, dir) = (info.name.clone(), info.dir.clone());
        match init_worktree_now(&self.catalog, &paths.state, &paths.backups, &dir) {
            Ok(plan) => {
                self.matrix.reload_row(&self.catalog, p);
                self.refresh_visible();
                self.message = Some(format!(
                    "Initialized {name} from {}: {} server(s). kms --undo reverts it.",
                    plan.parent.display(),
                    plan.catalog_servers.len() + plan.outside_servers.len()
                ));
            }
            Err(message) => self.message = Some(message),
        }
    }

    /// Map a mouse click (column, row) to a `(project, server)` cell, if it
    /// lands on one. Columns before the grid or rows in the header miss.
    pub fn hit_test(&self, col: u16, row: u16) -> Option<(usize, usize)> {
        let line = (row.checked_sub(HEADER_ROWS)?) as usize;
        let project = *self.visible.get(line)?;
        let grid_col = col.checked_sub(NAME_COL_WIDTH)?;
        let server = (grid_col / CELL_STEP) as usize;
        if server >= self.matrix.server_count() {
            return None;
        }
        Some((project, server))
    }

    /// Click a cell: move both cursors there and toggle it.
    pub fn click(&mut self, col: u16, row: u16) {
        if let Some((project, server)) = self.hit_test(col, row) {
            self.project_cursor = project;
            self.server_cursor = server;
            self.toggle_cursor();
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.matrix.is_dirty()
    }

    /// Save modified projects. On success reports how many were written; a
    /// blocked write names the project and the reason.
    pub fn save(&mut self) -> Result<usize, (PathBuf, WriteError)> {
        let written = self.matrix.save(&self.catalog)?;
        let count = written.len();
        self.message = Some(if count == 0 {
            "Nothing to save.".to_string()
        } else {
            format!("Saved {count} project(s).")
        });
        Ok(count)
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::discovery::DiscoveredProject;
    use crate::core::merge::compute_statuses;
    use serde_json::Map;

    fn catalog(names: &[&str]) -> Catalog {
        let mut servers = Map::new();
        for name in names {
            servers.insert((*name).to_string(), serde_json::json!({ "command": "x" }));
        }
        Catalog { servers }
    }

    fn project(cat: &Catalog, dir: &str, present: &[&str]) -> DiscoveredProject {
        let mut servers = Map::new();
        for name in present {
            servers.insert((*name).to_string(), serde_json::json!({ "command": "x" }));
        }
        DiscoveredProject {
            name: dir.to_string(),
            dir: PathBuf::from(dir),
            statuses: compute_statuses(cat, &servers),
            active_count: 0,
            parent: None,
            has_config: true,
        }
    }

    fn view(server_names: &[&str], project_dirs: &[&str]) -> MatrixView {
        let cat = catalog(server_names);
        let projects: Vec<_> = project_dirs.iter().map(|d| project(&cat, d, &[])).collect();
        let matrix = Matrix::build(&cat, &projects);
        MatrixView::new(cat, matrix)
    }

    #[test]
    fn cursors_wrap_in_both_dimensions() {
        let mut v = view(&["a", "b"], &["/p1", "/p2", "/p3"]);
        v.move_project(-1);
        assert_eq!(v.project_cursor, 2);
        v.move_project(1);
        assert_eq!(v.project_cursor, 0);
        v.move_server(-1);
        assert_eq!(v.server_cursor, 1);
    }

    #[test]
    fn hit_test_maps_click_to_cell() {
        let v = view(&["a", "b", "c"], &["/p1", "/p2"]);
        // First project row, first server column.
        assert_eq!(v.hit_test(NAME_COL_WIDTH, HEADER_ROWS), Some((0, 0)));
        // Second server column on the second project row.
        assert_eq!(
            v.hit_test(NAME_COL_WIDTH + CELL_STEP, HEADER_ROWS + 1),
            Some((1, 1))
        );
        // Third server column.
        assert_eq!(
            v.hit_test(NAME_COL_WIDTH + 2 * CELL_STEP, HEADER_ROWS),
            Some((0, 2))
        );
    }

    #[test]
    fn hit_test_misses_header_and_name_column() {
        let v = view(&["a"], &["/p1"]);
        assert_eq!(
            v.hit_test(NAME_COL_WIDTH, HEADER_ROWS - 1),
            None,
            "header row"
        );
        assert_eq!(
            v.hit_test(NAME_COL_WIDTH - 1, HEADER_ROWS),
            None,
            "name column"
        );
    }

    #[test]
    fn hit_test_misses_beyond_grid() {
        let v = view(&["a"], &["/p1"]);
        assert_eq!(
            v.hit_test(NAME_COL_WIDTH + CELL_STEP, HEADER_ROWS),
            None,
            "no 2nd server"
        );
        assert_eq!(
            v.hit_test(NAME_COL_WIDTH, HEADER_ROWS + 1),
            None,
            "no 2nd project"
        );
    }

    #[test]
    fn click_toggles_the_hit_cell() {
        let mut v = view(&["a", "b"], &["/p1"]);
        assert!(!v.matrix.checked[0][1]);
        v.click(NAME_COL_WIDTH + CELL_STEP, HEADER_ROWS);
        assert!(v.matrix.checked[0][1], "clicked cell is toggled on");
        assert_eq!(v.server_cursor, 1);
    }

    #[test]
    fn edits_persist_in_the_view_model() {
        let mut v = view(&["a"], &["/p1"]);
        v.toggle_cursor();
        assert!(v.is_dirty());
        assert!(v.matrix.checked[0][0]);
    }

    fn family(cat: &Catalog, worktree_has_config: bool) -> MatrixView {
        let mut parent = project(cat, "/p", &[]);
        parent.name = "p".to_string();
        let mut child = project(cat, "/p/.worktrees/wt", &[]);
        child.name = "wt".to_string();
        child.parent = Some(PathBuf::from("/p"));
        child.has_config = worktree_has_config;
        let other = project(cat, "/q", &[]);
        let matrix = Matrix::build(cat, &[parent, child, other]);
        MatrixView::new(cat.clone(), matrix)
    }

    #[test]
    fn worktrees_are_folded_under_a_clean_parent() {
        let cat = catalog(&["a"]);
        let mut v = family(&cat, true);
        assert_eq!(v.visible_rows(), &[0, 2], "the worktree row is hidden");
        v.move_project(1);
        assert_eq!(v.project_cursor, 2, "the cursor skips hidden rows");
        // Clicks map through the visible rows: second line is project /q.
        assert_eq!(v.hit_test(NAME_COL_WIDTH, HEADER_ROWS + 1), Some((2, 0)));

        v.project_cursor = 0;
        v.toggle_fold();
        assert_eq!(v.visible_rows(), &[0, 1, 2]);
        assert_eq!(v.hit_test(NAME_COL_WIDTH, HEADER_ROWS + 1), Some((1, 0)));
    }

    #[test]
    fn the_hint_explains_the_keys_where_they_apply() {
        let cat = catalog(&["a"]);
        let mut v = family(&cat, true);

        v.project_cursor = 0;
        let hint = v.row_hint().unwrap();
        assert_eq!(hint, "1 worktree(s) hidden — e: show");

        v.project_cursor = 2;
        assert_eq!(v.row_hint(), None, "no worktrees, no hint");

        let mut v = family(&cat, false);
        v.project_cursor = 0;
        assert_eq!(v.row_hint().unwrap(), "1 worktree(s) shown — e: hide");
        v.project_cursor = 1;
        let hint = v.row_hint().unwrap();
        assert_eq!(hint, "no config — i: write it from p");
    }

    #[test]
    fn a_worktree_without_config_unfolds_its_parent_by_itself() {
        let cat = catalog(&["a"]);
        let v = family(&cat, false);
        assert_eq!(v.visible_rows(), &[0, 1, 2]);
        assert!(v.is_expanded(0));
    }

    #[test]
    fn a_diverging_worktree_unfolds_its_parent_by_itself() {
        let cat = catalog(&["a"]);
        let mut v = family(&cat, true);
        assert!(!v.is_expanded(0));
        v.matrix.diverged[1][0] = true;
        v.refresh_visible();
        assert_eq!(v.visible_rows(), &[0, 1, 2]);
    }

    #[test]
    fn folding_from_a_worktree_moves_the_cursor_to_its_parent() {
        let cat = catalog(&["a"]);
        let mut v = family(&cat, false);
        v.project_cursor = 1;
        v.toggle_fold();
        assert_eq!(
            v.visible_rows(),
            &[0, 2],
            "manual fold beats the automatic unfold"
        );
        assert_eq!(v.project_cursor, 0);
    }

    #[test]
    fn init_writes_the_config_of_a_worktree_that_has_none() {
        use crate::core::discovery::discover_projects;
        use crate::core::worktrees::testing::{linked, main_repo};

        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("proj");
        std::fs::create_dir_all(main.join(".kiro").join("settings")).unwrap();
        main_repo(&main);
        std::fs::write(
            main.join(".kiro").join("settings").join("mcp.json"),
            r#"{ "mcpServers": { "a": { "command": "x", "disabled": true } } }"#,
        )
        .unwrap();
        let wt = main.join(".worktrees").join("feat");
        linked(&main, &wt, "feat");

        let cat = catalog(&["a"]);
        let projects = discover_projects(&cat, &[dir.path().to_path_buf()], 2);
        assert_eq!(projects.len(), 2);
        let matrix = Matrix::build(&cat, &projects);
        let mut v = MatrixView::new(cat, matrix).with_init_paths(InitPaths {
            state: dir.path().join("state.json"),
            backups: dir.path().join("backups"),
        });
        assert!(v.is_expanded(0), "no config: shown, not folded");

        v.project_cursor = 0;
        v.init_worktree();
        assert!(v
            .message
            .as_deref()
            .unwrap()
            .contains("Nothing to initialize"));

        v.project_cursor = 1;
        v.init_worktree();
        let written = std::fs::read_to_string(wt.join(".kiro/settings/mcp.json")).unwrap();
        assert!(written.contains("\"disabled\": true"), "{written}");
        assert!(v.matrix.projects[1].has_config);
        assert!(
            v.matrix.checked[1][0],
            "the row now shows the copied servers"
        );
        assert!(!v.is_dirty(), "initializing is not a pending edit");
        assert!(v.message.as_deref().unwrap().contains("Initialized feat"));
    }
}
