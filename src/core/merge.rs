//! Per-server state, divergence, and toggle application.
//!
//! For each catalog server, its state in a project is one of:
//! - [`ServerState::Absent`]: not in the project file.
//! - [`ServerState::Present`]: present and matching the catalog.
//! - [`ServerState::Diverged`]: present but some field differs from the catalog.
//!
//! Divergence compares the **whole entry except `disabled`** and reports
//! **which** fields differ. `disabled` belongs to Kiro: it is written only when
//! inserting an absent entry, preserved on updates, and
//! excluded from the comparison — otherwise every server switched on in Kiro
//! would look permanently diverged.

use serde_json::{Map, Value};

use super::types::{Catalog, ServerKind};

/// The field Kiro owns and kms must never treat as divergence.
const DISABLED: &str = "disabled";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Absent,
    Present,
    Diverged,
}

/// Status of one catalog server against a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub name: String,
    pub kind: ServerKind,
    pub state: ServerState,
    /// Names of the fields that differ from the catalog, sorted. Empty unless
    /// `state` is `Diverged`.
    pub diverging_fields: Vec<String>,
    /// The project entry's `disabled` value, surfaced separately as "switched
    /// off in Kiro". Never counted as divergence.
    pub disabled_in_kiro: Option<bool>,
}

fn kind_of(entry: &Map<String, Value>) -> ServerKind {
    if matches!(entry.get("url"), Some(Value::String(_))) {
        ServerKind::Remote
    } else {
        ServerKind::Local
    }
}

/// Fields that differ between two entries, ignoring `disabled`. The union of
/// keys is considered, so a field present on one side only counts as differing.
fn diverging_fields(catalog: &Map<String, Value>, project: &Map<String, Value>) -> Vec<String> {
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for key in catalog.keys().chain(project.keys()) {
        if key == DISABLED {
            continue;
        }
        if catalog.get(key) != project.get(key) {
            names.insert(key.clone());
        }
    }
    names.into_iter().collect()
}

fn disabled_of(entry: &Map<String, Value>) -> Option<bool> {
    match entry.get(DISABLED) {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// Compute the status of one catalog server against its project entry.
pub fn compute_status(
    name: &str,
    catalog_entry: &Map<String, Value>,
    project_entry: Option<&Map<String, Value>>,
) -> ServerStatus {
    let kind = kind_of(catalog_entry);
    let Some(project_entry) = project_entry else {
        return ServerStatus {
            name: name.to_string(),
            kind,
            state: ServerState::Absent,
            diverging_fields: Vec::new(),
            disabled_in_kiro: None,
        };
    };

    let fields = diverging_fields(catalog_entry, project_entry);
    let state = if fields.is_empty() {
        ServerState::Present
    } else {
        ServerState::Diverged
    };
    ServerStatus {
        name: name.to_string(),
        kind,
        state,
        diverging_fields: fields,
        disabled_in_kiro: disabled_of(project_entry),
    }
}

/// Status of every catalog server against the project, sorted by name.
pub fn compute_statuses(
    catalog: &Catalog,
    project_servers: &Map<String, Value>,
) -> Vec<ServerStatus> {
    catalog
        .names_sorted()
        .into_iter()
        .map(|name| {
            let catalog_entry = catalog.servers[&name]
                .as_object()
                .cloned()
                .unwrap_or_default();
            let project_entry = project_servers.get(&name).and_then(Value::as_object);
            compute_status(&name, &catalog_entry, project_entry)
        })
        .collect()
}

/// How to resolve a divergence when enabling a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDiverge {
    /// Overwrite with the catalog definition.
    Catalog,
    /// Preserve the local entry as-is.
    Keep,
}

/// One requested change.
#[derive(Debug, Clone)]
pub struct Toggle {
    pub name: String,
    /// true = present in the project, false = removed.
    pub enabled: bool,
    pub on_diverge: OnDiverge,
}

/// Apply one toggle to the project's servers map, in place.
///
/// - enable, entry absent: insert the complete catalog entry, applying the
///   catalog's `disabled` as the initial state.
/// - enable, entry present: refresh from the catalog but preserve the project's
///   `disabled`. With `OnDiverge::Keep`, leave the local entry untouched.
/// - disable: remove the entry.
///
/// A name absent from the catalog is never touched.
pub fn apply_toggle(catalog: &Catalog, project_servers: &mut Map<String, Value>, toggle: &Toggle) {
    let Some(catalog_value) = catalog.servers.get(&toggle.name) else {
        return;
    };
    let Some(catalog_entry) = catalog_value.as_object() else {
        return;
    };

    if !toggle.enabled {
        project_servers.remove(&toggle.name);
        return;
    }

    let existing = project_servers.get(&toggle.name).and_then(Value::as_object);

    if toggle.on_diverge == OnDiverge::Keep && existing.is_some() {
        return;
    }

    let mut next = catalog_entry.clone();
    match existing {
        Some(existing) => {
            // Update: preserve Kiro's `disabled`; drop the catalog's own.
            match existing.get(DISABLED) {
                Some(value) => {
                    next.insert(DISABLED.to_string(), value.clone());
                }
                None => {
                    next.remove(DISABLED);
                }
            }
        }
        None => {
            // Insert: keep the catalog's `disabled` as the initial state, if any.
        }
    }

    project_servers.insert(toggle.name.clone(), Value::Object(next));
}

/// Apply several toggles in order.
pub fn apply_toggles(
    catalog: &Catalog,
    project_servers: &mut Map<String, Value>,
    toggles: &[Toggle],
) {
    for toggle in toggles {
        apply_toggle(catalog, project_servers, toggle);
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

    fn servers(entries: &[(&str, Value)]) -> Map<String, Value> {
        let mut m = Map::new();
        for (name, entry) in entries {
            m.insert((*name).to_string(), entry.clone());
        }
        m
    }

    #[test]
    fn state_absent_when_not_in_project() {
        let cat = catalog(&[("a", serde_json::json!({ "command": "x" }))]);
        let proj = Map::new();
        let statuses = compute_statuses(&cat, &proj);
        assert_eq!(statuses[0].state, ServerState::Absent);
    }

    #[test]
    fn state_present_when_matching() {
        let cat = catalog(&[("a", serde_json::json!({ "command": "x", "args": ["1"] }))]);
        let proj = servers(&[("a", serde_json::json!({ "command": "x", "args": ["1"] }))]);
        let statuses = compute_statuses(&cat, &proj);
        assert_eq!(statuses[0].state, ServerState::Present);
        assert!(statuses[0].diverging_fields.is_empty());
    }

    #[test]
    fn diverges_on_env() {
        let cat = catalog(&[(
            "a",
            serde_json::json!({ "command": "x", "env": { "A": "1" } }),
        )]);
        let proj = servers(&[(
            "a",
            serde_json::json!({ "command": "x", "env": { "A": "2" } }),
        )]);
        let statuses = compute_statuses(&cat, &proj);
        assert_eq!(statuses[0].state, ServerState::Diverged);
        assert_eq!(statuses[0].diverging_fields, vec!["env".to_string()]);
    }

    #[test]
    fn diverges_on_args() {
        let cat = catalog(&[("a", serde_json::json!({ "command": "x", "args": ["1"] }))]);
        let proj = servers(&[("a", serde_json::json!({ "command": "x", "args": ["2"] }))]);
        let statuses = compute_statuses(&cat, &proj);
        assert_eq!(statuses[0].state, ServerState::Diverged);
        assert_eq!(statuses[0].diverging_fields, vec!["args".to_string()]);
    }

    #[test]
    fn changed_disabled_is_not_a_divergence() {
        let cat = catalog(&[("a", serde_json::json!({ "command": "x" }))]);
        let proj = servers(&[("a", serde_json::json!({ "command": "x", "disabled": true }))]);
        let statuses = compute_statuses(&cat, &proj);
        assert_eq!(statuses[0].state, ServerState::Present);
        assert_eq!(statuses[0].disabled_in_kiro, Some(true));
    }

    #[test]
    fn insert_applies_catalog_disabled_as_initial_state() {
        let cat = catalog(&[("a", serde_json::json!({ "command": "x", "disabled": true }))]);
        let mut proj = Map::new();
        apply_toggle(
            &cat,
            &mut proj,
            &Toggle {
                name: "a".into(),
                enabled: true,
                on_diverge: OnDiverge::Catalog,
            },
        );
        assert_eq!(proj["a"]["disabled"], serde_json::json!(true));
    }

    #[test]
    fn update_preserves_kiro_disabled() {
        // Catalog has no disabled; project switched it off in Kiro. An update
        // (resolving a divergence with Catalog) must keep disabled:true.
        let cat = catalog(&[("a", serde_json::json!({ "command": "x", "args": ["new"] }))]);
        let mut proj = servers(&[(
            "a",
            serde_json::json!({ "command": "x", "args": ["old"], "disabled": true }),
        )]);
        apply_toggle(
            &cat,
            &mut proj,
            &Toggle {
                name: "a".into(),
                enabled: true,
                on_diverge: OnDiverge::Catalog,
            },
        );
        assert_eq!(proj["a"]["args"], serde_json::json!(["new"]));
        assert_eq!(proj["a"]["disabled"], serde_json::json!(true));
    }

    #[test]
    fn keep_preserves_local_version() {
        let cat = catalog(&[("a", serde_json::json!({ "command": "x", "args": ["new"] }))]);
        let mut proj = servers(&[(
            "a",
            serde_json::json!({ "command": "x", "args": ["local"] }),
        )]);
        apply_toggle(
            &cat,
            &mut proj,
            &Toggle {
                name: "a".into(),
                enabled: true,
                on_diverge: OnDiverge::Keep,
            },
        );
        assert_eq!(proj["a"]["args"], serde_json::json!(["local"]));
    }

    #[test]
    fn catalog_overwrites_on_diverge() {
        let cat = catalog(&[("a", serde_json::json!({ "command": "x", "args": ["new"] }))]);
        let mut proj = servers(&[(
            "a",
            serde_json::json!({ "command": "x", "args": ["local"] }),
        )]);
        apply_toggle(
            &cat,
            &mut proj,
            &Toggle {
                name: "a".into(),
                enabled: true,
                on_diverge: OnDiverge::Catalog,
            },
        );
        assert_eq!(proj["a"]["args"], serde_json::json!(["new"]));
    }

    #[test]
    fn entry_outside_catalog_left_intact() {
        let cat = catalog(&[("a", serde_json::json!({ "command": "x" }))]);
        let mut proj = servers(&[("stranger", serde_json::json!({ "command": "keep" }))]);
        apply_toggle(
            &cat,
            &mut proj,
            &Toggle {
                name: "stranger".into(),
                enabled: false,
                on_diverge: OnDiverge::Catalog,
            },
        );
        assert!(
            proj.contains_key("stranger"),
            "unknown name is never touched"
        );
    }

    #[test]
    fn disable_removes_entry() {
        let cat = catalog(&[("a", serde_json::json!({ "command": "x" }))]);
        let mut proj = servers(&[("a", serde_json::json!({ "command": "x" }))]);
        apply_toggle(
            &cat,
            &mut proj,
            &Toggle {
                name: "a".into(),
                enabled: false,
                on_diverge: OnDiverge::Catalog,
            },
        );
        assert!(!proj.contains_key("a"));
    }

    #[test]
    fn insert_drops_catalog_disabled_none_stays_none() {
        let cat = catalog(&[("a", serde_json::json!({ "command": "x" }))]);
        let mut proj = Map::new();
        apply_toggle(
            &cat,
            &mut proj,
            &Toggle {
                name: "a".into(),
                enabled: true,
                on_diverge: OnDiverge::Catalog,
            },
        );
        assert!(proj["a"].as_object().unwrap().get("disabled").is_none());
    }
}
