//! Reading the global `~/.kiro/settings/mcp.json`, read-only outside migration.
//!
//! In the target model the global file defines nothing optional: any server
//! still listed there is active in every project, outside the catalog and
//! invisible to project-level tooling. kms never writes here except through
//! migration; it only reports the drift.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The global config location, `~/.kiro/settings/mcp.json`.
pub fn default_global_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".kiro").join("settings").join("mcp.json"))
}

/// Names of servers still declared in the global file, sorted. A missing file,
/// unreadable file, or invalid JSON yields an empty list: drift reporting must
/// never be the reason a command fails.
pub fn lingering_servers(path: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let Some(Value::Object(servers)) = obj.get("mcpServers") else {
        return Vec::new();
    };
    let mut names: Vec<String> = servers.keys().cloned().collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reports_no_drift() {
        let dir = tempfile::tempdir().unwrap();
        assert!(lingering_servers(&dir.path().join("absent.json")).is_empty());
    }

    #[test]
    fn lists_servers_still_present() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        std::fs::write(
            &file,
            r#"{ "mcpServers": { "zeta": { "command": "z" }, "alpha": { "command": "a" } } }"#,
        )
        .unwrap();
        assert_eq!(lingering_servers(&file), vec!["alpha", "zeta"]);
    }

    #[test]
    fn empty_servers_reports_no_drift() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        std::fs::write(&file, r#"{ "mcpServers": {}, "powers": {} }"#).unwrap();
        assert!(lingering_servers(&file).is_empty());
    }
}
