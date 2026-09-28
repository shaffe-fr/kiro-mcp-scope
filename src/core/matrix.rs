//! Matrix model: projects in rows, catalog servers in columns.
//!
//! A cell is checked when the server is present (active or diverged) in the
//! project. Editing happens on a grid of booleans held in memory; saving writes
//! only the projects whose row changed, each through `apply_toggles` — so the
//! grouped save touches only modified projects.
//!
//! The edited grid lives here, not re-read from disk, so switching views before
//! saving does not lose pending checks.

use std::path::PathBuf;

use super::discovery::DiscoveredProject;
use super::merge::{apply_toggles, OnDiverge, ServerState, Toggle};
use super::types::Catalog;
use super::workspace::{read_workspace, write_workspace, WorkspaceFile, WriteError};

#[derive(Debug, Clone)]
pub struct ProjectRef {
    pub name: String,
    pub dir: PathBuf,
}

/// The editable matrix. `initial` is the state as discovered; `checked` is the
/// edited grid. A row is dirty when the two differ.
#[derive(Debug, Clone)]
pub struct Matrix {
    pub server_names: Vec<String>,
    pub projects: Vec<ProjectRef>,
    /// `checked[project][server]`: edited selection.
    pub checked: Vec<Vec<bool>>,
    /// `diverged[project][server]`: present but divergent, for display.
    pub diverged: Vec<Vec<bool>>,
    /// The selection as first discovered, to detect per-row changes.
    initial: Vec<Vec<bool>>,
}

impl Matrix {
    pub fn build(catalog: &Catalog, projects: &[DiscoveredProject]) -> Self {
        let server_names = catalog.names_sorted();
        let index: std::collections::HashMap<&str, usize> = server_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();

        let mut checked = Vec::with_capacity(projects.len());
        let mut diverged = Vec::with_capacity(projects.len());
        for project in projects {
            let mut row_checked = vec![false; server_names.len()];
            let mut row_diverged = vec![false; server_names.len()];
            for status in &project.statuses {
                if let Some(&col) = index.get(status.name.as_str()) {
                    row_checked[col] = status.state != ServerState::Absent;
                    row_diverged[col] = status.state == ServerState::Diverged;
                }
            }
            checked.push(row_checked);
            diverged.push(row_diverged);
        }

        Self {
            server_names,
            projects: projects
                .iter()
                .map(|p| ProjectRef {
                    name: p.name.clone(),
                    dir: p.dir.clone(),
                })
                .collect(),
            initial: checked.clone(),
            checked,
            diverged,
        }
    }

    pub fn project_count(&self) -> usize {
        self.projects.len()
    }

    pub fn server_count(&self) -> usize {
        self.server_names.len()
    }

    pub fn toggle(&mut self, project: usize, server: usize) {
        if let Some(cell) = self
            .checked
            .get_mut(project)
            .and_then(|row| row.get_mut(server))
        {
            *cell = !*cell;
        }
    }

    /// Whether a project's row differs from its discovered state.
    pub fn row_dirty(&self, project: usize) -> bool {
        match (self.checked.get(project), self.initial.get(project)) {
            (Some(edited), Some(initial)) => edited != initial,
            _ => false,
        }
    }

    pub fn is_dirty(&self) -> bool {
        (0..self.project_count()).any(|p| self.row_dirty(p))
    }

    /// Write only the projects whose row changed, each via `apply_toggles`.
    /// Returns the directories actually written. Rebaselines written rows so a
    /// second save is a no-op. On a blocked write (secret) the row is left dirty
    /// and the error is returned with the project that triggered it.
    pub fn save(&mut self, catalog: &Catalog) -> Result<Vec<PathBuf>, (PathBuf, WriteError)> {
        let mut written = Vec::new();
        for p in 0..self.project_count() {
            if !self.row_dirty(p) {
                continue;
            }
            let project = &self.projects[p];
            let ws = read_workspace(&project.dir).unwrap_or_default();
            let toggles: Vec<Toggle> = self
                .server_names
                .iter()
                .enumerate()
                .map(|(s, name)| Toggle {
                    name: name.clone(),
                    enabled: self.checked[p][s],
                    on_diverge: OnDiverge::Keep,
                })
                .collect();

            let mut servers = ws.servers.clone();
            apply_toggles(catalog, &mut servers, &toggles);
            let next = WorkspaceFile {
                servers,
                extra: ws.extra,
            };
            match write_workspace(&project.dir, &next) {
                Ok(()) => {
                    self.initial[p] = self.checked[p].clone();
                    written.push(project.dir.clone());
                }
                Err(err) => return Err((project.dir.clone(), err)),
            }
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::merge::{compute_statuses, ServerStatus};
    use serde_json::Map;

    fn catalog(names: &[&str]) -> Catalog {
        let mut servers = Map::new();
        for name in names {
            servers.insert((*name).to_string(), serde_json::json!({ "command": "x" }));
        }
        Catalog { servers }
    }

    fn project(dir: &str, statuses: Vec<ServerStatus>) -> DiscoveredProject {
        DiscoveredProject {
            name: dir.to_string(),
            dir: PathBuf::from(dir),
            statuses,
            active_count: 0,
        }
    }

    fn discovered(catalog: &Catalog, dir: &str, present: &[&str]) -> DiscoveredProject {
        let mut servers = Map::new();
        for name in present {
            servers.insert((*name).to_string(), serde_json::json!({ "command": "x" }));
        }
        let statuses = compute_statuses(catalog, &servers);
        project(dir, statuses)
    }

    #[test]
    fn cells_reflect_presence() {
        let cat = catalog(&["a", "b"]);
        let projects = vec![discovered(&cat, "/p1", &["a"])];
        let m = Matrix::build(&cat, &projects);
        // server_names sorted: [a, b]
        assert_eq!(m.checked[0], vec![true, false]);
    }

    #[test]
    fn toggle_marks_row_dirty() {
        let cat = catalog(&["a"]);
        let projects = vec![discovered(&cat, "/p1", &[])];
        let mut m = Matrix::build(&cat, &projects);
        assert!(!m.is_dirty());
        m.toggle(0, 0);
        assert!(m.row_dirty(0));
        m.toggle(0, 0);
        assert!(!m.row_dirty(0), "toggling back clears the row");
    }

    #[test]
    fn edits_survive_without_save() {
        // The whole point of the persistence fix: the edited grid is held in
        // memory, so a check stays set until save or explicit toggle-back.
        let cat = catalog(&["a", "b"]);
        let projects = vec![discovered(&cat, "/p1", &[])];
        let mut m = Matrix::build(&cat, &projects);
        m.toggle(0, 1);
        assert!(m.checked[0][1], "edit persists in the model");
        assert!(m.row_dirty(0));
    }

    #[test]
    fn save_writes_only_changed_projects() {
        let cat = catalog(&["a"]);
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();

        let projects = vec![
            discovered(&cat, dir1.path().to_str().unwrap(), &[]),
            discovered(&cat, dir2.path().to_str().unwrap(), &[]),
        ];
        let mut m = Matrix::build(&cat, &projects);
        // Only change project 0.
        m.toggle(0, 0);

        let written = m.save(&cat).unwrap();
        assert_eq!(written.len(), 1, "only the changed project is written");
        assert_eq!(written[0], dir1.path());

        // Project 0 now has the server; project 1 has no mcp.json at all.
        let ws1 = read_workspace(dir1.path()).unwrap();
        assert!(ws1.servers.contains_key("a"));
        assert!(!crate::core::workspace::workspace_mcp_path(dir2.path()).exists());
    }

    #[test]
    fn save_is_idempotent_after_rebaseline() {
        let cat = catalog(&["a"]);
        let dir = tempfile::tempdir().unwrap();
        let projects = vec![discovered(&cat, dir.path().to_str().unwrap(), &[])];
        let mut m = Matrix::build(&cat, &projects);
        m.toggle(0, 0);

        assert_eq!(m.save(&cat).unwrap().len(), 1);
        assert!(!m.is_dirty(), "clean after save");
        assert_eq!(m.save(&cat).unwrap().len(), 0, "second save writes nothing");
    }

    #[test]
    fn save_stops_on_secret_and_reports_project() {
        let cat = {
            let mut servers = Map::new();
            servers.insert(
                "a".to_string(),
                serde_json::json!({ "command": "x", "env": { "TOKEN": "literal-secret-value-x" } }),
            );
            Catalog { servers }
        };
        let dir = tempfile::tempdir().unwrap();
        let projects = vec![discovered(&cat, dir.path().to_str().unwrap(), &[])];
        let mut m = Matrix::build(&cat, &projects);
        m.toggle(0, 0);

        let (project, err) = m.save(&cat).unwrap_err();
        assert_eq!(project, dir.path());
        assert!(matches!(err, WriteError::Secret(_)));
    }
}
