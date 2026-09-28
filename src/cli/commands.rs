//! Command implementations usable without the TUI: `--list`, `--status`,
//! `--activate`, `--deactivate`. `--activate` / `--deactivate` are a scripting
//! convenience; the TUI is the primary path.

use std::path::Path;

use thiserror::Error;

use crate::core::catalog::{load_catalog, CatalogError};
use crate::core::config::{load_config, resolve_roots};
use crate::core::discovery::discover_projects;
use crate::core::global::lingering_servers;
use crate::core::merge::{
    apply_toggle, compute_statuses, OnDiverge, ServerState, ServerStatus, Toggle,
};
use crate::core::types::{Catalog, ServerKind};
use crate::core::workspace::{read_workspace, write_workspace, WriteError};

#[derive(Debug, Error)]
pub enum CommandError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),

    #[error(transparent)]
    Write(#[from] WriteError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("Server \"{0}\" is not in the catalog.")]
    UnknownServer(String),
}

fn kind_label(kind: ServerKind) -> &'static str {
    match kind {
        ServerKind::Local => "local",
        ServerKind::Remote => "remote",
    }
}

fn format_status_line(status: &ServerStatus) -> String {
    let mark = match status.state {
        ServerState::Absent => "[ ]",
        ServerState::Present => "[x]",
        ServerState::Diverged => "[!]",
    };
    let mut line = format!("{mark} {} ({})", status.name, kind_label(status.kind));
    if status.state == ServerState::Diverged {
        line.push_str(&format!(
            " diverges: {}",
            status.diverging_fields.join(", ")
        ));
    }
    if status.disabled_in_kiro == Some(true) {
        line.push_str(" — off in Kiro");
    }
    line
}

/// `--list`: catalog servers and their state in the project at `project_dir`.
pub fn list(catalog_path: &Path, project_dir: &Path) -> Result<String, CommandError> {
    let catalog = load_catalog(catalog_path)?;
    let ws = read_workspace(project_dir)?;
    let statuses = compute_statuses(&catalog, &ws.servers);

    let mut out = String::new();
    if statuses.is_empty() {
        out.push_str("Catalog is empty.");
        return Ok(out);
    }
    for status in &statuses {
        out.push_str(&format_status_line(status));
        out.push('\n');
    }
    Ok(out.trim_end().to_string())
}

/// `--status`: like `list`, followed by the global-drift warning if the global
/// `mcp.json` still lists servers.
pub fn status(
    catalog_path: &Path,
    project_dir: &Path,
    global_path: &Path,
) -> Result<String, CommandError> {
    let mut out = list(catalog_path, project_dir)?;
    let lingering = lingering_servers(global_path);
    if !lingering.is_empty() {
        out.push_str("\n\nWarning: the global mcp.json still defines servers, active in every project and outside the catalog:\n");
        for name in &lingering {
            out.push_str(&format!("  - {name}\n"));
        }
        out.push_str("Run --migrate to move them into the catalog.");
    }
    Ok(out.trim_end().to_string())
}

/// `--discover`: list detected Kiro projects and how many catalog servers each
/// has taken. Roots come from CLI overrides, else the config, else cwd + parent.
pub fn discover(
    catalog_path: &Path,
    config_path: &Path,
    explicit_roots: &[String],
    cwd: &Path,
) -> Result<String, CommandError> {
    let catalog = load_catalog(catalog_path)?;
    let config = load_config(config_path);
    let roots = resolve_roots(explicit_roots, &config, cwd);
    let projects = discover_projects(&catalog, &roots, config.max_depth);

    let mut out = String::new();
    if projects.is_empty() {
        out.push_str("No Kiro projects found under:\n");
        for root in &roots {
            out.push_str(&format!("  - {}\n", root.display()));
        }
        return Ok(out.trim_end().to_string());
    }
    let total = catalog.servers.len();
    for project in &projects {
        out.push_str(&format!(
            "{}  ({}/{} servers)  {}\n",
            project.name,
            project.active_count,
            total,
            project.dir.display()
        ));
    }
    Ok(out.trim_end().to_string())
}

fn require_in_catalog<'a>(catalog: &'a Catalog, name: &str) -> Result<&'a Catalog, CommandError> {
    if catalog.servers.contains_key(name) {
        Ok(catalog)
    } else {
        Err(CommandError::UnknownServer(name.to_string()))
    }
}

/// `--activate <name>`: take a catalog server into the project. On divergence,
/// this overwrites with the catalog definition (the scripting default);
/// interactive resolution is the TUI's job.
pub fn activate(
    catalog_path: &Path,
    project_dir: &Path,
    name: &str,
) -> Result<String, CommandError> {
    let catalog = load_catalog(catalog_path)?;
    require_in_catalog(&catalog, name)?;
    let mut ws = read_workspace(project_dir)?;
    apply_toggle(
        &catalog,
        &mut ws.servers,
        &Toggle {
            name: name.to_string(),
            enabled: true,
            on_diverge: OnDiverge::Catalog,
        },
    );
    write_workspace(project_dir, &ws)?;
    Ok(format!("Activated \"{name}\" in this project."))
}

/// `--deactivate <name>`: remove a server from the project.
pub fn deactivate(
    catalog_path: &Path,
    project_dir: &Path,
    name: &str,
) -> Result<String, CommandError> {
    let catalog = load_catalog(catalog_path)?;
    require_in_catalog(&catalog, name)?;
    let mut ws = read_workspace(project_dir)?;
    apply_toggle(
        &catalog,
        &mut ws.servers,
        &Toggle {
            name: name.to_string(),
            enabled: false,
            on_diverge: OnDiverge::Catalog,
        },
    );
    write_workspace(project_dir, &ws)?;
    Ok(format!("Deactivated \"{name}\" in this project."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_catalog(dir: &Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("catalog.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn list_shows_states() {
        let dir = tempfile::tempdir().unwrap();
        let catalog_path = write_catalog(
            dir.path(),
            r#"{ "mcpServers": {
                "absent": { "command": "a" },
                "present": { "command": "p", "args": ["1"] }
            } }"#,
        );
        let proj = tempfile::tempdir().unwrap();
        let ws_path = proj.path().join(".kiro").join("settings").join("mcp.json");
        std::fs::create_dir_all(ws_path.parent().unwrap()).unwrap();
        std::fs::write(
            &ws_path,
            r#"{ "mcpServers": { "present": { "command": "p", "args": ["1"] } } }"#,
        )
        .unwrap();

        let out = list(&catalog_path, proj.path()).unwrap();
        assert!(out.contains("[ ] absent"));
        assert!(out.contains("[x] present"));
    }

    #[test]
    fn status_warns_on_global_drift() {
        let dir = tempfile::tempdir().unwrap();
        let catalog_path = write_catalog(
            dir.path(),
            r#"{ "mcpServers": { "a": { "command": "x" } } }"#,
        );
        let proj = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.json");
        std::fs::write(
            &global,
            r#"{ "mcpServers": { "leftover": { "command": "l" } } }"#,
        )
        .unwrap();

        let out = status(&catalog_path, proj.path(), &global).unwrap();
        assert!(out.contains("Warning"));
        assert!(out.contains("leftover"));
    }

    #[test]
    fn status_silent_when_global_clean() {
        let dir = tempfile::tempdir().unwrap();
        let catalog_path = write_catalog(
            dir.path(),
            r#"{ "mcpServers": { "a": { "command": "x" } } }"#,
        );
        let proj = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.json");
        std::fs::write(&global, r#"{ "mcpServers": {} }"#).unwrap();

        let out = status(&catalog_path, proj.path(), &global).unwrap();
        assert!(!out.contains("Warning"));
    }

    #[test]
    fn activate_then_deactivate_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let catalog_path = write_catalog(
            dir.path(),
            r#"{ "mcpServers": { "srv": { "command": "x", "args": ["1"] } } }"#,
        );
        let proj = tempfile::tempdir().unwrap();

        activate(&catalog_path, proj.path(), "srv").unwrap();
        let ws = read_workspace(proj.path()).unwrap();
        assert!(ws.servers.contains_key("srv"));

        deactivate(&catalog_path, proj.path(), "srv").unwrap();
        let ws = read_workspace(proj.path()).unwrap();
        assert!(!ws.servers.contains_key("srv"));
    }

    #[test]
    fn activate_unknown_server_errors() {
        let dir = tempfile::tempdir().unwrap();
        let catalog_path = write_catalog(
            dir.path(),
            r#"{ "mcpServers": { "a": { "command": "x" } } }"#,
        );
        let proj = tempfile::tempdir().unwrap();
        let err = activate(&catalog_path, proj.path(), "ghost").unwrap_err();
        assert!(matches!(err, CommandError::UnknownServer(_)));
    }

    #[test]
    fn diverged_server_marked() {
        let dir = tempfile::tempdir().unwrap();
        let catalog_path = write_catalog(
            dir.path(),
            r#"{ "mcpServers": { "srv": { "command": "x", "args": ["new"] } } }"#,
        );
        let proj = tempfile::tempdir().unwrap();
        let ws_path = proj.path().join(".kiro").join("settings").join("mcp.json");
        std::fs::create_dir_all(ws_path.parent().unwrap()).unwrap();
        std::fs::write(
            &ws_path,
            r#"{ "mcpServers": { "srv": { "command": "x", "args": ["local"] } } }"#,
        )
        .unwrap();

        let out = list(&catalog_path, proj.path()).unwrap();
        assert!(out.contains("[!] srv"), "diverged marker: {out}");
        assert!(out.contains("diverges: args"), "names the field: {out}");
    }
}
