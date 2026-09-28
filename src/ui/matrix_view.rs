//! Matrix view state and 2D navigation, independent of the terminal.
//!
//! Projects are rows, catalog servers columns. The cursor moves in two
//! dimensions; a cell is toggled by keyboard or by clicking it. Hit-testing is
//! derived from the name-column width and the per-column step, kept pure here so
//! it can be tested without a terminal.

use std::path::PathBuf;

use crate::core::matrix::Matrix;
use crate::core::types::Catalog;
use crate::core::workspace::WriteError;

/// Width of the left column holding project names, in cells.
pub const NAME_COL_WIDTH: u16 = 22;
/// Width of each server column (checkbox + padding).
pub const CELL_STEP: u16 = 5;
/// Rows before the first project row: title, blank, header row, blank.
pub const HEADER_ROWS: u16 = 4;

pub struct MatrixView {
    pub matrix: Matrix,
    catalog: Catalog,
    pub project_cursor: usize,
    pub server_cursor: usize,
    pub message: Option<String>,
}

impl MatrixView {
    pub fn new(catalog: Catalog, matrix: Matrix) -> Self {
        Self {
            matrix,
            catalog,
            project_cursor: 0,
            server_cursor: 0,
            message: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.matrix.project_count() == 0 || self.matrix.server_count() == 0
    }

    pub fn move_project(&mut self, delta: isize) {
        let n = self.matrix.project_count();
        if n == 0 {
            return;
        }
        self.project_cursor =
            (self.project_cursor as isize + delta).rem_euclid(n as isize) as usize;
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

    /// Map a mouse click (column, row) to a `(project, server)` cell, if it
    /// lands on one. Columns before the grid or rows in the header miss.
    pub fn hit_test(&self, col: u16, row: u16) -> Option<(usize, usize)> {
        let project = (row.checked_sub(HEADER_ROWS)?) as usize;
        if project >= self.matrix.project_count() {
            return None;
        }
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
}
