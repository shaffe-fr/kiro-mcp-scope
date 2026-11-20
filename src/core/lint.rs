//! Suspicious configuration, reported but never fixed behind the user's back.
//!
//! Two kinds of finding, both worded without ever quoting a value:
//!
//! - an `env` variable of an `mcp-remote` server that no argument references.
//!   `mcp-remote` only reads such a variable when an argument spells it out
//!   (`--header Authorization:${AUTH_HEADER}`); otherwise the block does
//!   nothing, and the authentication quietly falls back to OAuth;
//! - a literal secret in `env` or `headers`, which belongs in a `${VAR}`.

use serde_json::{Map, Value};

use super::secrets::scan_server;

/// `env` keys that configure the process itself rather than an argument, so
/// they are legitimate without being referenced.
const PROCESS_LEVEL_KEYS: [&str; 17] = [
    "PATH",
    "PATHEXT",
    "HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "TEMP",
    "TMP",
    "SYSTEMROOT",
    "LANG",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "NODE_OPTIONS",
    "NODE_EXTRA_CA_CERTS",
    "MCP_REMOTE_CONFIG_DIR",
];

/// One finding in a given file for a given server.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Warning {
    pub server: String,
    pub message: String,
}

fn is_mcp_remote(entry: &Map<String, Value>) -> bool {
    entry
        .get("args")
        .and_then(Value::as_array)
        .is_some_and(|args| {
            args.iter()
                .filter_map(Value::as_str)
                .any(|arg| arg.contains("mcp-remote"))
        })
}

fn is_process_level(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    PROCESS_LEVEL_KEYS.contains(&upper.as_str()) || upper.starts_with("MCP_REMOTE_")
}

/// Whether `arg` mentions variable `key` as `${KEY}`, `$KEY` or `%KEY%`.
fn mentions(arg: &str, key: &str) -> bool {
    if arg.contains(&format!("${{{key}}}")) || arg.contains(&format!("%{key}%")) {
        return true;
    }
    let dollar = format!("${key}");
    let mut from = 0;
    while let Some(at) = arg[from..].find(&dollar) {
        let end = from + at + dollar.len();
        let next = arg[end..].chars().next();
        if !next.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
            return true;
        }
        from = end;
    }
    false
}

/// Findings for one server entry.
pub fn lint_entry(name: &str, entry: &Map<String, Value>) -> Vec<Warning> {
    let mut warnings = Vec::new();

    if is_mcp_remote(entry) {
        let args: Vec<&str> = entry
            .get("args")
            .and_then(Value::as_array)
            .map(|args| args.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if let Some(env) = entry.get("env").and_then(Value::as_object) {
            for key in env.keys() {
                if is_process_level(key) || args.iter().any(|arg| mentions(arg, key)) {
                    continue;
                }
                warnings.push(Warning {
                    server: name.to_string(),
                    message: format!(
                        "env.{key} is not referenced by any argument: mcp-remote ignores it. \
                         Reference it as ${{{key}}} in an argument, or remove it."
                    ),
                });
            }
        }
    }

    for finding in scan_server(name, entry) {
        warnings.push(Warning {
            server: name.to_string(),
            message: format!(
                "{}.{} holds a literal secret. Replace it with a ${{VAR}} reference.",
                finding.field, finding.key
            ),
        });
    }

    warnings.sort();
    warnings
}

/// Findings for every server of an `mcpServers` map.
pub fn lint_servers(servers: &Map<String, Value>) -> Vec<Warning> {
    let mut warnings = Vec::new();
    for (name, entry) in servers {
        if let Some(entry) = entry.as_object() {
            warnings.extend(lint_entry(name, entry));
        }
    }
    warnings.sort();
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SECRET: &str = "sk-live-9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c";

    fn entry(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn flags_an_env_variable_no_argument_uses() {
        let rovo = entry(json!({
            "command": "npx",
            "args": ["mcp-remote@latest", "https://mcp.atlassian.com/v2/mcp"],
            "env": { "AUTH_HEADER": "${KMS__ROVO__AUTH_HEADER}" }
        }));
        let warnings = lint_entry("rovo", &rovo);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("env.AUTH_HEADER"));
        assert!(warnings[0].message.contains("mcp-remote ignores it"));
    }

    #[test]
    fn a_referenced_variable_is_fine() {
        let used = entry(json!({
            "command": "npx",
            "args": ["mcp-remote@latest", "u", "--header", "Authorization:${AUTH_HEADER}"],
            "env": { "AUTH_HEADER": "${KMS__X}" }
        }));
        assert!(lint_entry("s", &used).is_empty());
    }

    #[test]
    fn near_miss_names_do_not_count_as_references() {
        let near = entry(json!({
            "command": "npx",
            "args": ["mcp-remote", "--header", "X:$AUTH_HEADER_2"],
            "env": { "AUTH_HEADER": "${KMS__X}" }
        }));
        assert_eq!(lint_entry("s", &near).len(), 1);
    }

    #[test]
    fn process_level_variables_and_other_servers_are_left_alone() {
        let proxy = entry(json!({
            "command": "npx",
            "args": ["mcp-remote", "u"],
            "env": { "HTTPS_PROXY": "http://proxy:3128", "NODE_OPTIONS": "--x" }
        }));
        assert!(lint_entry("s", &proxy).is_empty());

        let other = entry(json!({
            "command": "php",
            "args": ["artisan", "boost:mcp"],
            "env": { "UNUSED": "1" }
        }));
        assert!(
            lint_entry("s", &other).is_empty(),
            "only mcp-remote ignores env"
        );
    }

    #[test]
    fn a_literal_secret_is_reported_by_key_never_by_value() {
        let leaky = entry(json!({
            "command": "node",
            "args": ["server.js"],
            "env": { "API_TOKEN": SECRET }
        }));
        let warnings = lint_entry("leaky", &leaky);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("env.API_TOKEN"));
        assert!(!warnings[0].message.contains(SECRET));
    }
}
