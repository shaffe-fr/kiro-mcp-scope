//! Secret safeguard for values written into a project's `mcp.json`.
//!
//! A project `mcp.json` is meant to be versioned; a literal secret in an `env`
//! or `headers` value would leak it. The only accepted form for a sensitive
//! value is `${VAR}`, which Kiro resolves from the environment.
//!
//! The check carries **no provider list**. Such a list ages at every new
//! service and silently misses what it does not know. Instead the value must
//! prove it is harmless, through three rules applied in order:
//!
//! 1. Every `${IDENTIFIER}` occurrence is stripped; only the residue — what
//!    ends up in cleartext — is analyzed. `${TOKEN}` leaves nothing,
//!    `Bearer ${TOKEN}` leaves `Bearer `.
//! 2. If the key name evokes an identifier, any non-empty residue is refused.
//!    This is the only rule that catches a short, weak password.
//! 3. Otherwise the residue is refused if it is long, high-entropy, space-free,
//!    and looks like neither a path nor a URL.
//!
//! The scan walks `env` **and** `headers` of every entry regardless of its
//! variant: an identifier lives just as well in a local entry's `env`. Error
//! messages name the offending key, never its value.

use serde_json::{Map, Value};
use thiserror::Error;

/// Key-name substrings that mark a value as sensitive by intent (rule 2).
const SENSITIVE_KEY_TERMS: [&str; 10] = [
    "token",
    "secret",
    "key",
    "password",
    "auth",
    "credential",
    "bearer",
    "session",
    "cookie",
    "private",
];

/// Rule 3 thresholds. A residue must clear both to be considered secret-like.
const HIGH_ENTROPY_MIN_LENGTH: usize = 24;
const HIGH_ENTROPY_MIN_BITS: f64 = 3.5;

/// Why a value was refused. Never carries the value itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Rule 2: the key name evokes a secret and the residue is non-empty.
    SensitiveKeyName,
    /// Rule 3: the residue is long, high-entropy and unstructured.
    HighEntropy,
}

impl Reason {
    fn describe(self) -> &'static str {
        match self {
            Reason::SensitiveKeyName => "key name evokes a secret",
            Reason::HighEntropy => "high-entropy value",
        }
    }
}

/// One refused value, located by server and field, named by key only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub server: String,
    pub field: String,
    pub key: String,
    pub reason: Reason,
}

#[derive(Debug, Error)]
pub struct SecretError {
    pub findings: Vec<Finding>,
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Write blocked: literal secret detected.")?;
        for finding in &self.findings {
            write!(
                f,
                "\n  - server \"{}\", {}.{}: {}. Replace with ${{VAR}}.",
                finding.server,
                finding.field,
                finding.key,
                finding.reason.describe()
            )?;
        }
        Ok(())
    }
}

/// Remove every `${IDENTIFIER}` occurrence, returning the residue (rule 1).
fn strip_var_references(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(close) = value[i + 2..].find('}') {
                let ident = &value[i + 2..i + 2 + close];
                if is_var_identifier(ident) {
                    i = i + 2 + close + 1;
                    continue;
                }
            }
        }
        let ch = value[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// A `${...}` body is a variable identifier: a letter or `_`, then letters,
/// digits or `_`.
fn is_var_identifier(ident: &str) -> bool {
    let mut chars = ident.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn key_is_sensitive(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_KEY_TERMS.iter().any(|term| lower.contains(term))
}

fn shannon_entropy(value: &str) -> f64 {
    let chars: Vec<char> = value.chars().collect();
    if chars.is_empty() {
        return 0.0;
    }
    let mut counts: std::collections::HashMap<char, usize> = std::collections::HashMap::new();
    for ch in &chars {
        *counts.entry(*ch).or_insert(0) += 1;
    }
    let len = chars.len() as f64;
    counts
        .values()
        .map(|&count| {
            let p = count as f64 / len;
            -p * p.log2()
        })
        .sum()
}

fn looks_like_path(value: &str) -> bool {
    value.contains('/') || value.contains('\\')
}

fn looks_like_url(value: &str) -> bool {
    value.contains("://")
}

/// Rule 3: is the residue long, high-entropy, space-free and unstructured?
fn residue_is_secret_like(residue: &str) -> bool {
    let trimmed = residue.trim();
    if trimmed.contains(char::is_whitespace) {
        return false;
    }
    if looks_like_path(trimmed) || looks_like_url(trimmed) {
        return false;
    }
    if trimmed.chars().count() < HIGH_ENTROPY_MIN_LENGTH {
        return false;
    }
    shannon_entropy(trimmed) >= HIGH_ENTROPY_MIN_BITS
}

/// Whether a key/value pair reads as a literal secret under the three rules.
/// Migration uses this to decide which values to extract into `${VAR}`.
pub fn is_secret(key: &str, value: &str) -> bool {
    classify(key, value).is_some()
}

/// Classify a single key/value, returning why it is refused, if it is.
fn classify(key: &str, value: &str) -> Option<Reason> {
    let residue = strip_var_references(value);
    if residue.trim().is_empty() {
        return None;
    }
    if key_is_sensitive(key) {
        return Some(Reason::SensitiveKeyName);
    }
    if residue_is_secret_like(&residue) {
        return Some(Reason::HighEntropy);
    }
    None
}

fn scan_map(server: &str, field: &str, map: Option<&Value>, findings: &mut Vec<Finding>) {
    let Some(Value::Object(obj)) = map else {
        return;
    };
    for (key, value) in obj {
        if let Value::String(text) = value {
            if let Some(reason) = classify(key, text) {
                findings.push(Finding {
                    server: server.to_string(),
                    field: field.to_string(),
                    key: key.clone(),
                    reason,
                });
            }
        }
    }
}

/// Scan one entry's `env` and `headers`, regardless of its transport variant.
pub fn scan_server(name: &str, entry: &Map<String, Value>) -> Vec<Finding> {
    let mut findings = Vec::new();
    scan_map(name, "env", entry.get("env"), &mut findings);
    scan_map(name, "headers", entry.get("headers"), &mut findings);
    findings
}

/// Scan every server's `env` and `headers`.
pub fn scan_servers(servers: &Map<String, Value>) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut names: Vec<&String> = servers.keys().collect();
    names.sort();
    for name in names {
        if let Some(Value::Object(entry)) = servers.get(name) {
            findings.extend(scan_server(name, entry));
        }
    }
    findings
}

/// Refuse the write if any value looks like a literal secret.
pub fn assert_no_secrets(servers: &Map<String, Value>) -> Result<(), SecretError> {
    let findings = scan_servers(servers);
    if findings.is_empty() {
        Ok(())
    } else {
        Err(SecretError { findings })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(field: &str, pairs: &[(&str, &str)]) -> Map<String, Value> {
        let mut inner = Map::new();
        for (k, v) in pairs {
            inner.insert((*k).to_string(), Value::String((*v).to_string()));
        }
        let mut m = Map::new();
        m.insert("command".to_string(), Value::String("cmd".into()));
        m.insert(field.to_string(), Value::Object(inner));
        m
    }

    #[test]
    fn var_only_reference_is_accepted() {
        let e = entry("env", &[("AUTH_HEADER", "${AUTH_HEADER}")]);
        assert!(scan_server("s", &e).is_empty());
    }

    #[test]
    fn bearer_var_reference_accepted_residue_is_harmless() {
        // Rule 1 strips ${TOKEN}, leaving "Bearer " which is harmless: too
        // short and too low-entropy for rule 3. The key here is neutral, so
        // rule 2 does not apply — this is the form references are meant to take.
        let e = entry("headers", &[("Proxy", "Bearer ${TOKEN}")]);
        assert!(scan_server("s", &e).is_empty());
    }

    #[test]
    fn bearer_var_reference_refused_on_sensitive_key() {
        // On an "authorization"-named key rule 2 fires: any non-empty residue
        // ("Bearer") is refused. The remedy is a bare reference, ${TOKEN}, which
        // leaves an empty residue.
        let e = entry("headers", &[("Authorization", "Bearer ${TOKEN}")]);
        let findings = scan_server("s", &e);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, Reason::SensitiveKeyName);

        let bare = entry("headers", &[("Authorization", "${TOKEN}")]);
        assert!(scan_server("s", &bare).is_empty());
    }

    #[test]
    fn literal_refused_because_key_is_auth_header() {
        // A value that would clear rule 3 on its own, but the key seals it.
        let e = entry("env", &[("AUTH_HEADER", "Xq7Lp2Vn9Rt4Kw8Hs3Jd6Fz1")]);
        let findings = scan_server("s", &e);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, Reason::SensitiveKeyName);
    }

    #[test]
    fn short_weak_password_refused_on_password_key() {
        // Too short and too low-entropy for rule 3; only rule 2 catches it.
        let e = entry("env", &[("PASSWORD", "hunter2")]);
        let findings = scan_server("s", &e);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, Reason::SensitiveKeyName);
    }

    #[test]
    fn log_level_and_debug_flag_accepted() {
        let e = entry(
            "env",
            &[("FASTMCP_LOG_LEVEL", "ERROR"), ("DEBUG_ENABLED", "false")],
        );
        assert!(scan_server("s", &e).is_empty());
    }

    #[test]
    fn long_windows_path_accepted_on_path_key() {
        let e = entry(
            "env",
            &[(
                "TOOL_PATH",
                "C:\\Users\\someone\\AppData\\Local\\Programs\\SomeTool\\tool.exe",
            )],
        );
        assert!(scan_server("s", &e).is_empty());
    }

    #[test]
    fn long_random_string_refused_even_under_harmless_key_name() {
        let e = entry("env", &[("REGION", "a7Kd93JxQ2pL8mZ0Wf4Rt6Yb1Nc5Vg")]);
        let findings = scan_server("s", &e);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason, Reason::HighEntropy);
    }

    #[test]
    fn secret_detected_in_local_env_and_remote_headers() {
        let local = entry("env", &[("AUTH_HEADER", "literal-secret-value-here")]);
        assert_eq!(scan_server("local", &local).len(), 1);

        let mut remote = Map::new();
        remote.insert("url".to_string(), Value::String("https://x".into()));
        let mut headers = Map::new();
        headers.insert(
            "Authorization".to_string(),
            Value::String("literal-token-value".into()),
        );
        remote.insert("headers".to_string(), Value::Object(headers));
        assert_eq!(scan_server("remote", &remote).len(), 1);
    }

    #[test]
    fn error_message_names_key_never_value() {
        let secret = "q9Zr-super-secret-do-not-leak-7Wx";
        let e = entry("env", &[("AUTH_HEADER", secret)]);
        let err = assert_no_secrets(&{
            let mut servers = Map::new();
            servers.insert("s".to_string(), Value::Object(e));
            servers
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("AUTH_HEADER"), "names the key: {msg}");
        assert!(!msg.contains(secret), "never the value");
        assert!(
            !msg.contains("super-secret"),
            "not even a fragment of the value"
        );
    }

    #[test]
    fn residue_of_bearer_with_two_references_is_still_harmless() {
        let e = entry("headers", &[("X", "${A}/${B}")]);
        assert!(scan_server("s", &e).is_empty());
    }

    #[test]
    fn url_value_accepted_under_neutral_key() {
        let e = entry(
            "env",
            &[("ENDPOINT", "https://api.example.com/v1/very/long/path")],
        );
        assert!(scan_server("s", &e).is_empty());
    }
}
