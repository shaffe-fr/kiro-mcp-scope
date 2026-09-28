//! Migration of the global `mcp.json` into the catalog model.
//!
//! The global file mixes definition and activation; its servers are active
//! everywhere. Migration moves them into the catalog, replaces literal secrets
//! with `${VAR}` references, and empties the global's `mcpServers`. The rest of
//! the global (e.g. `powers`) is preserved intact.
//!
//! Two normalizations:
//! - `disabled` is **not** carried over: catalog entries are normalized to
//!   active. The current global servers are all `disabled: true`, which meant
//!   "not wanted everywhere" — exactly the problem being removed. The catalog
//!   stays hand-editable for servers one prefers to arrive switched off.
//! - Variable names are generic, derived from the **server** and the **key**
//!   where the secret was found, never from the value's shape:
//!   `KMS__<SERVER>__<KEY>`. `env.AUTH_HEADER` of `Remote-MCP-A` yields
//!   `${KMS__REMOTE_MCP_A__AUTH_HEADER}`. Every server gets its own variable,
//!   even when several hold the same value, so one token can be rotated without
//!   touching the others.
//!
//! The prefix marks the variables as kms's own: rollback removes exactly the
//! names a migration of the backup produces, and never a variable the user set
//! for another purpose. Names depend only on server and key names, visited in
//! sorted server order, which is what makes that re-derivation exact.

use std::collections::HashSet;
use std::fmt;

use serde_json::{Map, Value};

use super::secrets::is_secret;
use super::types::Catalog;

const DISABLED: &str = "disabled";
const VAR_PREFIX: &str = "KMS";
/// Between prefix, server and key. Segments never contain it, see
/// [`sanitize_segment`].
const SEPARATOR: &str = "__";

/// A secret value. Its `Debug` output is redacted so it cannot leak through a
/// log or an error message; only [`Secret::expose`] yields the text.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// A variable the migration asks to define in the user's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub value: Secret,
}

/// One secret value that was replaced by a variable reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub server: String,
    pub field: String,
    pub key: String,
    pub variable: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationResult {
    pub catalog: Catalog,
    /// The global after migration: `mcpServers` emptied, the rest preserved.
    pub emptied_global: Map<String, Value>,
    pub replacements: Vec<Replacement>,
    /// One entry per extracted secret, sorted by variable name.
    pub variables: Vec<EnvVar>,
    /// Whether any server carried a `disabled` field that was dropped.
    pub dropped_disabled: bool,
}

/// Hands out one variable per server and key, so each can be rotated on its own
/// even when several servers hold the same value today.
struct VariableAllocator {
    variables: Vec<EnvVar>,
    used: HashSet<String>,
}

impl VariableAllocator {
    fn new() -> Self {
        Self {
            variables: Vec::new(),
            used: HashSet::new(),
        }
    }

    /// `KMS__<SERVER>__<KEY>`. Normalization can make two pairs collide
    /// (`svc-a` and `svc_a`); the later one, in sorted server order, is
    /// suffixed.
    fn variable_for(&mut self, server: &str, key: &str, value: &str) -> String {
        let base = format!(
            "{VAR_PREFIX}{SEPARATOR}{}{SEPARATOR}{}",
            sanitize_segment(server),
            sanitize_segment(key)
        );
        let mut candidate = base.clone();
        let mut n = 2;
        while self.used.contains(&candidate) {
            candidate = format!("{base}_{n}");
            n += 1;
        }
        self.used.insert(candidate.clone());
        self.variables.push(EnvVar {
            name: candidate.clone(),
            value: Secret(value.to_string()),
        });
        candidate
    }

    fn into_variables(mut self) -> Vec<EnvVar> {
        self.variables.sort_by(|a, b| a.name.cmp(&b.name));
        self.variables
    }
}

/// Turn a server name or a key into one segment of a `${VAR}` identifier:
/// uppercase, `[A-Z0-9]` kept, every other run of characters turned into a
/// single `_`.
///
/// Kiro resolves `${...}` only for identifier-shaped names, and the secret
/// scanner only treats those as references — so a name that is not
/// identifier-shaped would be flagged as a secret all over again. A segment
/// never contains `__`, which keeps the separator between segments unambiguous.
fn sanitize_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "VAR".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Replace secret values in one string map, recording replacements.
fn replace_in_map(
    server: &str,
    field: &str,
    map: &Map<String, Value>,
    alloc: &mut VariableAllocator,
    replacements: &mut Vec<Replacement>,
) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in map {
        if let Value::String(text) = value {
            if is_secret(key, text) {
                let variable = alloc.variable_for(server, key, text);
                out.insert(key.clone(), Value::String(format!("${{{variable}}}")));
                replacements.push(Replacement {
                    server: server.to_string(),
                    field: field.to_string(),
                    key: key.clone(),
                    variable,
                });
                continue;
            }
        }
        out.insert(key.clone(), value.clone());
    }
    out
}

/// Migrate a global config object into a catalog, an emptied global, and the
/// list of secret replacements. Pure: writes nothing.
pub fn migrate(global: &Map<String, Value>) -> MigrationResult {
    let raw_servers = global
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    // Deterministic value->variable assignment: sort server names first, so a
    // value shared across servers always resolves to the same first variable.
    let mut names: Vec<&String> = raw_servers.keys().collect();
    names.sort();

    let mut alloc = VariableAllocator::new();
    let mut replacements = Vec::new();
    let mut dropped_disabled = false;
    let mut catalog_servers = Map::new();

    for name in names {
        let Some(entry) = raw_servers.get(name).and_then(Value::as_object) else {
            continue;
        };
        let mut cleaned = entry.clone();

        if cleaned.remove(DISABLED).is_some() {
            dropped_disabled = true;
        }

        if let Some(Value::Object(env)) = entry.get("env") {
            let replaced = replace_in_map(name, "env", env, &mut alloc, &mut replacements);
            cleaned.insert("env".to_string(), Value::Object(replaced));
        }
        if let Some(Value::Object(headers)) = entry.get("headers") {
            let replaced = replace_in_map(name, "headers", headers, &mut alloc, &mut replacements);
            cleaned.insert("headers".to_string(), Value::Object(replaced));
        }

        catalog_servers.insert(name.clone(), Value::Object(cleaned));
    }

    let mut emptied_global = global.clone();
    emptied_global.insert("mcpServers".to_string(), Value::Object(Map::new()));

    MigrationResult {
        catalog: Catalog {
            servers: catalog_servers,
        },
        emptied_global,
        replacements,
        variables: alloc.into_variables(),
        dropped_disabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global(json: Value) -> Map<String, Value> {
        json.as_object().unwrap().clone()
    }

    // A long, high-entropy, space-free literal that reads as a secret under the
    // generic rules. Synthetic, not a real provider token.
    const SECRET: &str = "a7Kd93JxQ2pL8mZ0Wf4Rt6Yb1Nc5VgHh2Xj";

    #[test]
    fn moves_servers_to_catalog_and_empties_global() {
        let result = migrate(&global(serde_json::json!({
            "mcpServers": { "a": { "command": "x" }, "b": { "url": "https://y" } }
        })));
        let mut names: Vec<&String> = result.catalog.servers.keys().collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(result.emptied_global["mcpServers"], serde_json::json!({}));
    }

    #[test]
    fn preserves_top_level_keys_such_as_powers() {
        let result = migrate(&global(serde_json::json!({
            "mcpServers": { "a": { "command": "x" } },
            "powers": { "power-example": { "type": "http", "url": "https://f" } }
        })));
        assert_eq!(
            result.emptied_global["powers"]["power-example"]["url"],
            serde_json::json!("https://f")
        );
    }

    #[test]
    fn each_server_gets_its_own_variable_even_for_a_shared_value() {
        let result = migrate(&global(serde_json::json!({
            "mcpServers": {
                "Remote-MCP-A": { "command": "npx", "env": { "AUTH_HEADER": SECRET } },
                "Remote-MCP-B": { "command": "npx", "env": { "AUTH_HEADER": SECRET } }
            }
        })));
        assert_eq!(
            result.catalog.servers["Remote-MCP-A"]["env"]["AUTH_HEADER"],
            serde_json::json!("${KMS__REMOTE_MCP_A__AUTH_HEADER}")
        );
        assert_eq!(
            result.catalog.servers["Remote-MCP-B"]["env"]["AUTH_HEADER"],
            serde_json::json!("${KMS__REMOTE_MCP_B__AUTH_HEADER}")
        );
        let names: Vec<&str> = result.variables.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "KMS__REMOTE_MCP_A__AUTH_HEADER",
                "KMS__REMOTE_MCP_B__AUTH_HEADER"
            ]
        );
        assert!(result.variables.iter().all(|v| v.value.expose() == SECRET));
    }

    #[test]
    fn each_secret_key_of_a_server_gets_its_own_variable() {
        let result = migrate(&global(serde_json::json!({
            "mcpServers": {
                "svc": {
                    "url": "https://x",
                    "headers": { "X-Api-Key": SECRET },
                    "env": { "TOKEN": "Zz9Yy8Xx7Ww6Vv5Uu4Tt3Ss2Rr1Qq0Pp" }
                }
            }
        })));
        let names: Vec<&str> = result.variables.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["KMS__SVC__TOKEN", "KMS__SVC__X_API_KEY"]);
    }

    #[test]
    fn names_colliding_after_normalization_are_suffixed() {
        let result = migrate(&global(serde_json::json!({
            "mcpServers": {
                "svc-a": { "command": "x", "env": { "TOKEN": SECRET } },
                "svc_a": { "command": "y", "env": { "TOKEN": SECRET } }
            }
        })));
        // Sorted server order: "svc-a" before "svc_a".
        assert_eq!(
            result.catalog.servers["svc-a"]["env"]["TOKEN"],
            serde_json::json!("${KMS__SVC_A__TOKEN}")
        );
        assert_eq!(
            result.catalog.servers["svc_a"]["env"]["TOKEN"],
            serde_json::json!("${KMS__SVC_A__TOKEN_2}")
        );
    }

    #[test]
    fn double_underscores_in_names_cannot_forge_a_separator() {
        // Unsanitized, both would read KMS__A__B__C.
        let result = migrate(&global(serde_json::json!({
            "mcpServers": {
                "a__b": { "command": "x", "env": { "c": SECRET } },
                "a": { "command": "y", "env": { "b__c": SECRET } }
            }
        })));
        let names: Vec<&str> = result.variables.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["KMS__A_B__C", "KMS__A__B_C"]);
    }

    #[test]
    fn variables_are_derived_deterministically() {
        // Rollback re-derives the names from the backup; two runs must agree.
        let input = global(serde_json::json!({
            "mcpServers": {
                "b": { "command": "y", "env": { "TOKEN": "Zz9Yy8Xx7Ww6Vv5Uu4Tt3Ss2Rr1Qq0Pp" } },
                "a": { "command": "x", "env": { "TOKEN": SECRET } }
            }
        }));
        assert_eq!(migrate(&input).variables, migrate(&input).variables);
    }

    #[test]
    fn secret_debug_output_is_redacted() {
        let result = migrate(&global(serde_json::json!({
            "mcpServers": { "r1": { "command": "npx", "env": { "AUTH_HEADER": SECRET } } }
        })));
        let debug = format!("{:?}", result.variables);
        assert!(!debug.contains(SECRET));
        assert!(debug.contains("KMS__R1__AUTH_HEADER"));
    }

    #[test]
    fn leaves_harmless_value_and_existing_reference_untouched() {
        let result = migrate(&global(serde_json::json!({
            "mcpServers": {
                "a": { "command": "x", "env": { "FASTMCP_LOG_LEVEL": "ERROR", "REF": "${EXISTING}" } }
            }
        })));
        assert_eq!(
            result.catalog.servers["a"]["env"],
            serde_json::json!({ "FASTMCP_LOG_LEVEL": "ERROR", "REF": "${EXISTING}" })
        );
        assert!(result.replacements.is_empty());
    }

    #[test]
    fn produced_catalog_contains_no_literal_secret() {
        let result = migrate(&global(serde_json::json!({
            "mcpServers": { "r1": { "command": "npx", "env": { "AUTH_HEADER": SECRET } } }
        })));
        let serialized = serde_json::to_string(&Value::Object(result.catalog.servers)).unwrap();
        assert!(!serialized.contains(SECRET));
    }

    #[test]
    fn global_without_servers_yields_empty_catalog_rest_preserved() {
        let result = migrate(&global(serde_json::json!({ "other": 1 })));
        assert!(result.catalog.servers.is_empty());
        assert_eq!(result.emptied_global["other"], serde_json::json!(1));
    }

    #[test]
    fn disabled_is_dropped_normalizing_to_active() {
        let result = migrate(&global(serde_json::json!({
            "mcpServers": {
                "a": { "command": "x", "disabled": true },
                "b": { "command": "y", "disabled": false }
            }
        })));
        assert!(result.catalog.servers["a"]
            .as_object()
            .unwrap()
            .get("disabled")
            .is_none());
        assert!(result.catalog.servers["b"]
            .as_object()
            .unwrap()
            .get("disabled")
            .is_none());
        assert!(result.dropped_disabled);
    }

    #[test]
    fn secret_in_remote_headers_is_extracted() {
        let result = migrate(&global(serde_json::json!({
            "mcpServers": {
                "svc": { "url": "https://x", "headers": { "X-Api-Key": SECRET } }
            }
        })));
        assert_eq!(
            result.catalog.servers["svc"]["headers"]["X-Api-Key"],
            serde_json::json!("${KMS__SVC__X_API_KEY}")
        );
    }

    #[test]
    fn migrated_variable_is_a_valid_reference_not_reflagged() {
        // The variable derived from a hyphenated key must be identifier-shaped,
        // else re-scanning the catalog would flag it as a secret again.
        let result = migrate(&global(serde_json::json!({
            "mcpServers": {
                "svc": { "url": "https://x", "headers": { "X-Api-Key": SECRET } }
            }
        })));
        let migrated = result.catalog.servers["svc"]["headers"]["X-Api-Key"]
            .as_str()
            .unwrap();
        assert!(!crate::core::secrets::is_secret("X-Api-Key", migrated));
    }

    #[test]
    fn sanitizes_segments() {
        assert_eq!(sanitize_segment("AUTH_HEADER"), "AUTH_HEADER");
        assert_eq!(sanitize_segment("Remote-MCP-A"), "REMOTE_MCP_A");
        assert_eq!(sanitize_segment("X-Api-Key"), "X_API_KEY");
        assert_eq!(sanitize_segment("a__b"), "A_B");
        assert_eq!(sanitize_segment("-weird--name-"), "WEIRD_NAME");
        assert_eq!(sanitize_segment("2fa"), "2FA");
        assert_eq!(sanitize_segment("!!!"), "VAR");
    }
}
