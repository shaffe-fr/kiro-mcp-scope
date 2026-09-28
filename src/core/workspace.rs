//! Reading and writing `<project>/.kiro/settings/mcp.json`.
//!
//! Writing is the only surface that activates a server: writing its complete
//! entry activates it, removing the entry deactivates it. Entries and top-level
//! keys the catalog does not know about are preserved as-is — in particular the
//! `powers` section Kiro manages.
//!
//! Serialization is deterministic and writing is idempotent: re-applying the
//! same state does not rewrite the file, so git diffs stay clean.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use thiserror::Error;

use super::secrets::{self, SecretError};
use super::types::McpServer;

/// Failure while writing a project's `mcp.json`.
#[derive(Debug, Error)]
pub enum WriteError {
    #[error(transparent)]
    Secret(#[from] SecretError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A project's `mcp.json`, split into the servers kms manages and everything
/// else, which is preserved verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceFile {
    /// `mcpServers` map, name to entry.
    pub servers: Map<String, Value>,
    /// Top-level keys other than `mcpServers`, preserved untouched.
    pub extra: Map<String, Value>,
}

/// Fixed order for the known keys of an MCP entry; anything else follows in
/// alphabetical order.
const SERVER_KEY_ORDER: [&str; 11] = [
    "type",
    "command",
    "args",
    "url",
    "headers",
    "env",
    "oauth",
    "oauthScopes",
    "disabled",
    "autoApprove",
    "disabledTools",
];

pub fn workspace_mcp_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".kiro").join("settings").join("mcp.json")
}

/// Reorder an object's keys: `preferred` first in their fixed order, then the
/// rest alphabetically. Recurses into nested objects and arrays.
fn order_keys(value: &Value, preferred: &[&str]) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(|v| order_keys(v, &[])).collect()),
        Value::Object(obj) => {
            let mut ordered = Map::new();
            for key in preferred {
                if let Some(v) = obj.get(*key) {
                    ordered.insert((*key).to_string(), order_keys(v, &[]));
                }
            }
            let mut rest: Vec<&String> = obj
                .keys()
                .filter(|k| !preferred.contains(&k.as_str()))
                .collect();
            rest.sort();
            for key in rest {
                ordered.insert(key.clone(), order_keys(&obj[key], &[]));
            }
            Value::Object(ordered)
        }
        other => other.clone(),
    }
}

/// Serialize a workspace file: `mcpServers` first (servers sorted by name, each
/// entry's keys in the fixed order), then the extra top-level keys sorted
/// alphabetically. Two-space indent, trailing newline.
pub fn serialize(file: &WorkspaceFile) -> String {
    let mut servers = Map::new();
    let mut names: Vec<&String> = file.servers.keys().collect();
    names.sort();
    for name in names {
        servers.insert(
            name.clone(),
            order_keys(&file.servers[name], &SERVER_KEY_ORDER),
        );
    }

    let mut top = Map::new();
    top.insert("mcpServers".to_string(), Value::Object(servers));
    let mut extra_keys: Vec<&String> = file.extra.keys().collect();
    extra_keys.sort();
    for key in extra_keys {
        top.insert(key.clone(), order_keys(&file.extra[key], &[]));
    }

    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"  ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    serde::Serialize::serialize(&Value::Object(top), &mut ser)
        .expect("serializing a serde_json::Value cannot fail");
    let mut out = String::from_utf8(buf).expect("serde_json emits valid UTF-8");
    out.push('\n');
    out
}

/// Read a project's `mcp.json`. A missing file is the empty state, not an
/// error. A present `mcpServers` that is not an object is treated as empty; the
/// remaining top-level keys are always preserved.
pub fn read_workspace(project_dir: &Path) -> std::io::Result<WorkspaceFile> {
    let path = workspace_mcp_path(project_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspaceFile::default());
        }
        Err(err) => return Err(err),
    };
    let data: Value = serde_json::from_str(&raw)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

    let mut obj = match data {
        Value::Object(obj) => obj,
        _ => return Ok(WorkspaceFile::default()),
    };

    let servers = match obj.remove("mcpServers") {
        Some(Value::Object(servers)) => servers,
        _ => Map::new(),
    };

    Ok(WorkspaceFile {
        servers,
        extra: obj,
    })
}

/// Write a project's `mcp.json`, creating parent directories as needed.
/// Idempotent: if the serialized content matches what is on disk, nothing is
/// written. The secret safeguard runs first, so a literal secret blocks the
/// write before anything touches the disk.
pub fn write_workspace(project_dir: &Path, file: &WorkspaceFile) -> Result<(), WriteError> {
    secrets::assert_no_secrets(&file.servers)?;

    let path = workspace_mcp_path(project_dir);
    let next = serialize(file);

    match std::fs::read_to_string(&path) {
        Ok(current) if current == next => return Ok(()),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, next)?;
    Ok(())
}

/// Insert or replace one server entry in a workspace file.
pub fn set_server(file: &mut WorkspaceFile, name: &str, entry: McpServer) {
    file.servers
        .insert(name.to_string(), Value::Object(entry.into_map()));
}

/// Remove a server entry, returning whether it was present.
pub fn remove_server(file: &mut WorkspaceFile, name: &str) -> bool {
    file.servers.remove(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(pairs: &[(&str, Value)]) -> Map<String, Value> {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        m
    }

    #[test]
    fn known_keys_come_first_in_fixed_order_then_rest_alphabetical() {
        let entry = serde_json::json!({
            "zeta": true,
            "autoApprove": ["a"],
            "command": "php",
            "alpha": 1,
            "args": ["x"],
            "env": { "B": "2", "A": "1" }
        });
        let file = WorkspaceFile {
            servers: obj(&[("srv", entry)]),
            extra: Map::new(),
        };
        let out = serialize(&file);
        let command_at = out.find("\"command\"").unwrap();
        let args_at = out.find("\"args\"").unwrap();
        let env_at = out.find("\"env\"").unwrap();
        let auto_at = out.find("\"autoApprove\"").unwrap();
        let alpha_at = out.find("\"alpha\"").unwrap();
        let zeta_at = out.find("\"zeta\"").unwrap();
        assert!(command_at < args_at, "command before args");
        assert!(args_at < env_at, "args before env");
        assert!(env_at < auto_at, "env before autoApprove");
        assert!(auto_at < alpha_at, "known keys before unknown");
        assert!(alpha_at < zeta_at, "unknown keys sorted alphabetically");
        // env's own keys are recursively sorted.
        let a_at = out.find("\"A\"").unwrap();
        let b_at = out.find("\"B\"").unwrap();
        assert!(a_at < b_at, "nested object keys sorted");
    }

    #[test]
    fn servers_sorted_by_name() {
        let file = WorkspaceFile {
            servers: obj(&[
                ("zebra", serde_json::json!({ "command": "z" })),
                ("alpha", serde_json::json!({ "command": "a" })),
            ]),
            extra: Map::new(),
        };
        let out = serialize(&file);
        assert!(out.find("\"alpha\"").unwrap() < out.find("\"zebra\"").unwrap());
    }

    #[test]
    fn ends_with_single_trailing_newline() {
        let file = WorkspaceFile::default();
        let out = serialize(&file);
        assert!(out.ends_with("}\n"));
        assert!(!out.ends_with("}\n\n"));
    }

    #[test]
    fn two_space_indent() {
        let file = WorkspaceFile {
            servers: obj(&[("srv", serde_json::json!({ "command": "php" }))]),
            extra: Map::new(),
        };
        let out = serialize(&file);
        assert!(
            out.contains("\n  \"mcpServers\""),
            "top-level indented by 2"
        );
        assert!(out.contains("\n    \"srv\""), "server indented by 4");
    }

    #[test]
    fn idempotent_write_does_not_touch_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = WorkspaceFile::default();
        set_server(
            &mut file,
            "srv",
            McpServer::from_map(obj(&[("command", Value::String("php".into()))])),
        );

        write_workspace(dir.path(), &file).unwrap();
        let path = workspace_mcp_path(dir.path());
        let mtime1 = std::fs::metadata(&path).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(20));
        write_workspace(dir.path(), &file).unwrap();
        let mtime2 = std::fs::metadata(&path).unwrap().modified().unwrap();

        assert_eq!(mtime1, mtime2, "second identical write must not touch file");
    }

    #[test]
    fn preserves_entry_outside_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let path = workspace_mcp_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{ "mcpServers": { "stranger": { "command": "keep-me", "custom": 42 } } }"#,
        )
        .unwrap();

        let ws = read_workspace(dir.path()).unwrap();
        assert!(ws.servers.contains_key("stranger"));

        // Round-trip preserves the unknown field.
        write_workspace(dir.path(), &ws).unwrap();
        let after = read_workspace(dir.path()).unwrap();
        assert_eq!(after.servers["stranger"]["custom"], serde_json::json!(42));
    }

    #[test]
    fn preserves_powers_top_level_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = workspace_mcp_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
                "mcpServers": {},
                "powers": { "power-example": { "type": "http", "url": "https://x" } }
            }"#,
        )
        .unwrap();

        let ws = read_workspace(dir.path()).unwrap();
        assert!(ws.extra.contains_key("powers"));

        write_workspace(dir.path(), &ws).unwrap();
        let after = read_workspace(dir.path()).unwrap();
        assert_eq!(
            after.extra["powers"]["power-example"]["url"],
            serde_json::json!("https://x")
        );
    }

    #[test]
    fn missing_file_reads_as_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let ws = read_workspace(dir.path()).unwrap();
        assert!(ws.servers.is_empty());
        assert!(ws.extra.is_empty());
    }

    #[test]
    fn write_blocked_when_entry_carries_literal_secret() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = WorkspaceFile::default();
        let entry = serde_json::json!({
            "command": "npx",
            "env": { "AUTH_HEADER": "literal-secret-not-a-var" }
        });
        file.servers.insert("proxy".to_string(), entry);

        let err = write_workspace(dir.path(), &file).unwrap_err();
        assert!(matches!(err, WriteError::Secret(_)));
        // Nothing was written.
        assert!(!workspace_mcp_path(dir.path()).exists());
    }

    #[test]
    fn remove_reports_presence() {
        let mut file = WorkspaceFile::default();
        set_server(
            &mut file,
            "srv",
            McpServer::from_map(obj(&[("command", Value::String("php".into()))])),
        );
        assert!(remove_server(&mut file, "srv"));
        assert!(!remove_server(&mut file, "srv"));
    }
}
