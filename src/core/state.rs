//! What the catalog said about the project-tunable fields, over time.
//!
//! `autoApprove` and `disabledTools` live in the catalog but a project may
//! tweak them. To tell "this copy still carries an older catalog value" (safe to
//! refresh) from "someone customized this copy" (keep, and say so), kms keeps
//! the successive catalog values of each field in
//! `~/.kiro/mcp-catalog.state.json`. A copy whose value matches any value the
//! catalog ever had is not a customization; one that matches none is.
//!
//! The values are tool names, never secrets, so they are stored readable. They
//! are normalized (sorted, deduplicated) before comparison and before being
//! recorded, so a mere reordering is never mistaken for a customization.
//!
//! Only commands that write (`--sync`, `--activate`, the TUI) record into the
//! file; read-only commands never touch it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};
use thiserror::Error;

use super::jsonedit::{render_document, write_atomic, Layout};
use super::types::Catalog;

/// The fields a project may tune; the three-version rule applies to them.
pub const TRACKED_FIELDS: [&str; 2] = ["autoApprove", "disabledTools"];

/// How many successive values are kept per server and field.
const MAX_HISTORY: usize = 50;

/// A field value reduced to what matters for comparison: `None` when the field
/// is absent, otherwise its items sorted and deduplicated.
pub type Norm = Option<Vec<String>>;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("Cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("{path} is not valid kms state ({message}). Fix or delete the file.")]
    Invalid { path: PathBuf, message: String },
}

/// Sort and deduplicate the items of a field value.
pub fn normalize(value: Option<&Value>) -> Norm {
    let value = value?;
    let mut items: Vec<String> = match value {
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect(),
        other => vec![other.to_string()],
    };
    items.sort();
    items.dedup();
    Some(items)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// server -> field -> successive catalog values, oldest first.
    history: BTreeMap<String, BTreeMap<String, Vec<Norm>>>,
}

impl State {
    /// Whether anything is known about this server's past catalog values.
    pub fn has_history(&self, server: &str) -> bool {
        self.history
            .get(server)
            .is_some_and(|fields| fields.values().any(|values| !values.is_empty()))
    }

    /// Whether the catalog ever held exactly this value for the field.
    pub fn knows(&self, server: &str, field: &str, value: &Norm) -> bool {
        self.history
            .get(server)
            .and_then(|fields| fields.get(field))
            .is_some_and(|values| values.contains(value))
    }

    /// Add the catalog's current values. Returns whether anything was new.
    pub fn record_catalog(&mut self, catalog: &Catalog) -> bool {
        let mut changed = false;
        for (name, entry) in &catalog.servers {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            for field in TRACKED_FIELDS {
                let value = normalize(entry.get(field));
                let values = self
                    .history
                    .entry(name.clone())
                    .or_default()
                    .entry(field.to_string())
                    .or_default();
                if !values.contains(&value) {
                    values.push(value);
                    if values.len() > MAX_HISTORY {
                        values.remove(0);
                    }
                    changed = true;
                }
            }
        }
        changed
    }

    fn to_value(&self) -> Value {
        let mut history = Map::new();
        for (server, fields) in &self.history {
            let mut out = Map::new();
            for (field, values) in fields {
                let list: Vec<Value> = values
                    .iter()
                    .map(|value| match value {
                        None => Value::Null,
                        Some(items) => json!(items),
                    })
                    .collect();
                out.insert(field.clone(), Value::Array(list));
            }
            history.insert(server.clone(), Value::Object(out));
        }
        json!({ "version": 1, "history": Value::Object(history) })
    }

    fn from_value(data: &Value) -> Result<Self, String> {
        let history = data
            .get("history")
            .and_then(Value::as_object)
            .ok_or_else(|| "missing \"history\" object".to_string())?;
        let mut state = State::default();
        for (server, fields) in history {
            let fields = fields
                .as_object()
                .ok_or_else(|| format!("\"{server}\" must be an object"))?;
            for (field, values) in fields {
                let values = values
                    .as_array()
                    .ok_or_else(|| format!("\"{server}.{field}\" must be an array"))?;
                let mut list = Vec::new();
                for value in values {
                    list.push(match value {
                        Value::Null => None,
                        Value::Array(items) => Some(
                            items
                                .iter()
                                .map(|item| item.as_str().unwrap_or_default().to_string())
                                .collect(),
                        ),
                        _ => return Err(format!("\"{server}.{field}\" holds an invalid value")),
                    });
                }
                state
                    .history
                    .entry(server.clone())
                    .or_default()
                    .insert(field.clone(), list);
            }
        }
        Ok(state)
    }
}

/// The state file location, `~/.kiro/mcp-catalog.state.json`.
pub fn default_state_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".kiro").join("mcp-catalog.state.json"))
}

/// Load the state. A missing file is an empty history.
pub fn load_state(path: &Path) -> Result<State, StateError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
        Err(source) => {
            return Err(StateError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let data: Value = serde_json::from_str(&raw).map_err(|err| StateError::Invalid {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    State::from_value(&data).map_err(|message| StateError::Invalid {
        path: path.to_path_buf(),
        message,
    })
}

/// Write the state; nothing is written when the content is unchanged.
pub fn save_state(path: &Path, state: &State) -> std::io::Result<()> {
    let next = render_document(&state.to_value(), &Layout::default());
    if std::fs::read_to_string(path).is_ok_and(|current| current == next) {
        return Ok(());
    }
    write_atomic(path, &next)
}

/// Record the catalog's current values into the state at `path`.
pub fn record_catalog_at(path: &Path, catalog: &Catalog) -> Result<bool, String> {
    let mut state = load_state(path).map_err(|err| err.to_string())?;
    if !state.record_catalog(catalog) {
        return Ok(false);
    }
    save_state(path, &state).map_err(|err| format!("Cannot write {}: {err}", path.display()))?;
    Ok(true)
}

/// Record into the default state file, ignoring failures: activation must not
/// fail because the history could not be kept.
pub fn record_best_effort(catalog: &Catalog) {
    if let Some(path) = default_state_path() {
        let _ = record_catalog_at(&path, catalog);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(value: Value) -> Catalog {
        Catalog {
            servers: value.as_object().unwrap().clone(),
        }
    }

    #[test]
    fn normalization_sorts_and_deduplicates() {
        let value = json!(["b", "a", "b"]);
        assert_eq!(
            normalize(Some(&value)),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(normalize(None), None);
    }

    #[test]
    fn records_successive_catalog_values_once() {
        let mut state = State::default();
        let v1 = catalog(json!({ "s": { "command": "x", "disabledTools": ["b", "a"] } }));
        assert!(state.record_catalog(&v1));
        assert!(
            !state.record_catalog(&v1),
            "same values are not re-recorded"
        );

        let reordered = catalog(json!({ "s": { "command": "x", "disabledTools": ["a", "b"] } }));
        assert!(!state.record_catalog(&reordered), "order is not a change");

        let v2 = catalog(json!({ "s": { "command": "x", "disabledTools": ["a"] } }));
        assert!(state.record_catalog(&v2));

        assert!(state.knows("s", "disabledTools", &Some(vec!["a".into(), "b".into()])));
        assert!(state.knows("s", "disabledTools", &Some(vec!["a".into()])));
        assert!(!state.knows("s", "disabledTools", &Some(vec!["z".into()])));
        assert!(
            state.knows("s", "autoApprove", &None),
            "absence is a value too"
        );
        assert!(state.has_history("s"));
        assert!(!state.has_history("other"));
    }

    #[test]
    fn round_trips_through_the_file_and_skips_identical_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let cat = catalog(json!({ "s": { "command": "x", "autoApprove": ["t"] } }));

        assert!(record_catalog_at(&path, &cat).unwrap());
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(!record_catalog_at(&path, &cat).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);

        let loaded = load_state(&path).unwrap();
        assert!(loaded.knows("s", "autoApprove", &Some(vec!["t".into()])));
    }

    #[test]
    fn missing_file_is_empty_and_garbage_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_state(&dir.path().join("none.json")).unwrap(),
            State::default()
        );
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{ nope").unwrap();
        assert!(matches!(
            load_state(&bad).unwrap_err(),
            StateError::Invalid { .. }
        ));
    }
}
