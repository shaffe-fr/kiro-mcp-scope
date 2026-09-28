//! Reading and validating `~/.kiro/mcp-catalog.json`.
//!
//! Validation names the offending server so an error is actionable. An entry
//! must define exactly one transport: `command` (local) or `url` (remote),
//! never both, never neither.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use thiserror::Error;

use super::types::Catalog;

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error(
        "Catalog not found at {0}. Run `kms --migrate` to create it from the global mcp.json."
    )]
    NotFound(PathBuf),

    #[error("Cannot read catalog at {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Invalid JSON in catalog at {path}: {message}")]
    InvalidJson { path: PathBuf, message: String },

    #[error("Catalog at {path} must be a JSON object.")]
    NotObject { path: PathBuf },

    #[error("Catalog at {path} must contain an \"mcpServers\" object.")]
    MissingServers { path: PathBuf },

    #[error("Server \"{name}\" must be an object.")]
    ServerNotObject { name: String },

    #[error("Server \"{name}\" must define \"command\" (local) or \"url\" (remote).")]
    NoTransport { name: String },

    #[error("Server \"{name}\" defines both \"command\" and \"url\"; choose only one variant.")]
    BothTransports { name: String },
}

/// The default catalog location, `~/.kiro/mcp-catalog.json`.
pub fn default_catalog_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".kiro").join("mcp-catalog.json"))
}

fn is_string_field(entry: &Map<String, Value>, key: &str) -> bool {
    matches!(entry.get(key), Some(Value::String(_)))
}

fn validate_server(name: &str, value: &Value) -> Result<(), CatalogError> {
    let entry = value
        .as_object()
        .ok_or_else(|| CatalogError::ServerNotObject {
            name: name.to_string(),
        })?;
    let has_command = is_string_field(entry, "command");
    let has_url = is_string_field(entry, "url");
    match (has_command, has_url) {
        (false, false) => Err(CatalogError::NoTransport {
            name: name.to_string(),
        }),
        (true, true) => Err(CatalogError::BothTransports {
            name: name.to_string(),
        }),
        _ => Ok(()),
    }
}

/// Parse and validate catalog text. `path` is carried only for error messages.
pub fn parse_catalog(raw: &str, path: &Path) -> Result<Catalog, CatalogError> {
    let data: Value = serde_json::from_str(raw).map_err(|err| CatalogError::InvalidJson {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;

    let obj = data.as_object().ok_or_else(|| CatalogError::NotObject {
        path: path.to_path_buf(),
    })?;

    let servers = obj
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| CatalogError::MissingServers {
            path: path.to_path_buf(),
        })?;

    for (name, value) in servers {
        validate_server(name, value)?;
    }

    Ok(Catalog {
        servers: servers.clone(),
    })
}

/// Read and validate the catalog at `path`.
pub fn load_catalog(path: &Path) -> Result<Catalog, CatalogError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CatalogError::NotFound(path.to_path_buf()));
        }
        Err(err) => {
            return Err(CatalogError::Read {
                path: path.to_path_buf(),
                source: err,
            });
        }
    };
    parse_catalog(&raw, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::ServerKind;

    fn path() -> PathBuf {
        PathBuf::from("/virtual/catalog.json")
    }

    #[test]
    fn accepts_valid_catalog_local_and_remote() {
        let raw = r#"{
            "mcpServers": {
                "local": { "command": "php", "args": ["x.phar"] },
                "remote": { "url": "https://example.com/mcp", "type": "http" }
            }
        }"#;
        let catalog = parse_catalog(raw, &path()).unwrap();
        let names: Vec<&String> = catalog.servers.keys().collect();
        assert_eq!(names, vec!["local", "remote"]);
        assert_eq!(catalog.get("local").unwrap().kind(), ServerKind::Local);
        assert_eq!(catalog.get("remote").unwrap().kind(), ServerKind::Remote);
    }

    #[test]
    fn rejects_invalid_json() {
        let err = parse_catalog("{ not json", &path()).unwrap_err();
        assert!(matches!(err, CatalogError::InvalidJson { .. }));
    }

    #[test]
    fn rejects_entry_without_command_or_url() {
        let raw = r#"{ "mcpServers": { "orphan": { "args": [] } } }"#;
        let err = parse_catalog(raw, &path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("orphan"), "message names the server: {msg}");
        assert!(msg.contains("command"), "message mentions command: {msg}");
    }

    #[test]
    fn rejects_entry_with_both_command_and_url() {
        let raw = r#"{ "mcpServers": { "double": { "command": "x", "url": "https://y" } } }"#;
        let err = parse_catalog(raw, &path()).unwrap_err();
        assert!(matches!(err, CatalogError::BothTransports { .. }));
        assert!(err.to_string().contains("both"));
    }

    #[test]
    fn accepts_local_entry_carrying_env() {
        let raw = r#"{
            "mcpServers": {
                "proxy": {
                    "command": "npx",
                    "args": ["mcp-remote@latest"],
                    "env": { "AUTH_HEADER": "${AUTH_HEADER}" }
                }
            }
        }"#;
        let catalog = parse_catalog(raw, &path()).unwrap();
        let proxy = catalog.get("proxy").unwrap();
        assert_eq!(proxy.kind(), ServerKind::Local);
        assert!(proxy.get("env").is_some());
    }

    #[test]
    fn accepts_remote_entry_carrying_env() {
        let raw = r#"{
            "mcpServers": {
                "svc": {
                    "url": "https://example.com/mcp",
                    "env": { "REGION": "eu" }
                }
            }
        }"#;
        let catalog = parse_catalog(raw, &path()).unwrap();
        let svc = catalog.get("svc").unwrap();
        assert_eq!(svc.kind(), ServerKind::Remote);
        assert!(svc.get("env").is_some());
    }

    #[test]
    fn rejects_catalog_without_mcp_servers() {
        let err = parse_catalog("{}", &path()).unwrap_err();
        assert!(matches!(err, CatalogError::MissingServers { .. }));
    }

    #[test]
    fn rejects_non_object_catalog() {
        let err = parse_catalog("[]", &path()).unwrap_err();
        assert!(matches!(err, CatalogError::NotObject { .. }));
    }

    #[test]
    fn loads_present_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cat.json");
        std::fs::write(&file, r#"{ "mcpServers": { "a": { "command": "x" } } }"#).unwrap();
        let catalog = load_catalog(&file).unwrap();
        assert!(catalog.get("a").is_some());
    }

    #[test]
    fn reports_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("absent.json");
        let err = load_catalog(&file).unwrap_err();
        assert!(matches!(err, CatalogError::NotFound(_)));
        assert!(err.to_string().contains("not found"));
    }
}
