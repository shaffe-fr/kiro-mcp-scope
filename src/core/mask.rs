//! Display masking: nothing a command prints may carry a secret.
//!
//! Every value that reaches the terminal (diffs, status lines, warnings) goes
//! through here first. The rule is conservative on purpose:
//!
//! - `env` and `headers` values are always hidden, only the key names remain;
//! - elsewhere (`url`, `args`, ...) a string is hidden when it reads as a secret
//!   under the same rules as the write safeguard, and `key=value` / `key:value`
//!   pairs and URL query parameters are hidden when the key name evokes one;
//! - `${VAR}` references are not secrets and stay visible.

use serde_json::{Map, Value};

use super::secrets::{is_secret, is_sensitive_key};
use super::state::TRACKED_FIELDS;

/// What replaces a hidden value.
pub const HIDDEN: &str = "***";

/// Fields whose values are always hidden.
pub fn is_always_hidden(field: &str) -> bool {
    field == "env" || field == "headers"
}

/// Mask one string found outside `env` / `headers`.
pub fn mask_str(text: &str) -> String {
    if text.contains("://") {
        return mask_url(text);
    }
    if let Some(masked) = mask_pair(text) {
        return masked;
    }
    if is_secret("value", text) {
        return HIDDEN.to_string();
    }
    text.to_string()
}

/// `key=value` or `Key:value` where the key evokes a secret and the value is
/// not a bare `${VAR}` reference.
fn mask_pair(text: &str) -> Option<String> {
    let at = text.find(['=', ':'])?;
    let (key, rest) = text.split_at(at);
    let delimiter = &rest[..1];
    let value = &rest[1..];
    let key = key.trim_start_matches('-');
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    if is_secret(key, value) {
        let original_key = &text[..at];
        return Some(format!("{original_key}{delimiter}{HIDDEN}"));
    }
    None
}

/// Hide user info and sensitive query parameters of a URL; the rest (host,
/// path, harmless parameters such as `?tools=all`) stays readable.
fn mask_url(url: &str) -> String {
    let (scheme_end, rest_start) = match url.find("://") {
        Some(at) => (at, at + 3),
        None => return url.to_string(),
    };
    let scheme = &url[..scheme_end + 3];
    let rest = &url[rest_start..];

    let (authority_and_path, query) = match rest.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (rest, None),
    };

    let authority_end = authority_and_path
        .find('/')
        .unwrap_or(authority_and_path.len());
    let (authority, path) = authority_and_path.split_at(authority_end);
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => format!("{HIDDEN}@{host}"),
        None => authority.to_string(),
    };

    let mut out = format!("{scheme}{authority}{path}");
    if let Some(query) = query {
        let params: Vec<String> = query
            .split('&')
            .map(|param| match param.split_once('=') {
                Some((key, value)) if is_secret(key, value) => format!("{key}={HIDDEN}"),
                _ => param.to_string(),
            })
            .collect();
        out.push('?');
        out.push_str(&params.join("&"));
    }
    out
}

/// A copy of `value` safe to print, for the field named `field`.
///
/// `autoApprove` and `disabledTools` are lists of tool names, which are what
/// a reader needs to see; a long camel-case tool name would otherwise pass for
/// a high-entropy secret.
pub fn mask_field(field: &str, value: &Value) -> Value {
    if TRACKED_FIELDS.contains(&field) {
        return value.clone();
    }
    if is_always_hidden(field) {
        return match value {
            Value::Object(obj) => Value::Object(
                obj.keys()
                    .map(|key| (key.clone(), Value::String(HIDDEN.to_string())))
                    .collect::<Map<String, Value>>(),
            ),
            _ => Value::String(HIDDEN.to_string()),
        };
    }
    mask_value(value)
}

fn mask_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(mask_str(text)),
        Value::Array(items) => Value::Array(items.iter().map(mask_value).collect()),
        Value::Object(obj) => Value::Object(
            obj.iter()
                .map(|(key, item)| {
                    if is_sensitive_key(key) && matches!(item, Value::String(_)) {
                        let hidden = match item {
                            Value::String(text) if is_secret(key, text) => {
                                Value::String(HIDDEN.to_string())
                            }
                            other => mask_value(other),
                        };
                        (key.clone(), hidden)
                    } else {
                        (key.clone(), mask_value(item))
                    }
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Compact JSON of a masked field value, for one-line diffs.
pub fn render_field(field: &str, value: &Value) -> String {
    serde_json::to_string(&mask_field(field, value)).unwrap_or_else(|_| HIDDEN.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SECRET: &str = "sk-live-9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c";

    #[test]
    fn env_and_headers_show_keys_only() {
        let env = json!({ "API_TOKEN": SECRET, "MODE": "prod" });
        let rendered = render_field("env", &env);
        assert!(!rendered.contains(SECRET));
        assert!(!rendered.contains("prod"));
        assert!(rendered.contains("API_TOKEN"));
        assert!(rendered.contains(HIDDEN));

        let headers = json!({ "Authorization": format!("Bearer {SECRET}") });
        assert!(!render_field("headers", &headers).contains(SECRET));
    }

    #[test]
    fn tool_name_lists_are_shown_in_full() {
        let tools = json!(["getTransitionsForJiraIssue", "getVisibleJiraProjectsList"]);
        let rendered = render_field("disabledTools", &tools);
        assert!(rendered.contains("getTransitionsForJiraIssue"));
        assert!(!rendered.contains(HIDDEN));
        assert!(render_field("autoApprove", &tools).contains("getVisibleJiraProjectsList"));
    }

    #[test]
    fn variable_references_stay_visible_in_args() {
        let args = json!(["--header", "Authorization:${AUTH_HEADER}"]);
        let rendered = render_field("args", &args);
        assert!(rendered.contains("${AUTH_HEADER}"));
    }

    #[test]
    fn inline_header_values_in_args_are_hidden() {
        let args = json!(["--header", format!("Authorization:{SECRET}")]);
        let rendered = render_field("args", &args);
        assert!(!rendered.contains(SECRET));
        assert!(rendered.contains("Authorization"));
    }

    #[test]
    fn url_query_secrets_and_user_info_are_hidden() {
        let url = format!("https://user:pw@example.com/mcp?tools=all&api_key={SECRET}");
        let masked = mask_str(&url);
        assert!(!masked.contains(SECRET));
        assert!(!masked.contains("user:pw"));
        assert!(masked.contains("tools=all"));
        assert!(masked.contains("example.com/mcp"));
    }

    #[test]
    fn plain_urls_and_words_are_untouched() {
        assert_eq!(
            mask_str("https://mcp.atlassian.com/v2/mcp?tools=all"),
            "https://mcp.atlassian.com/v2/mcp?tools=all"
        );
        assert_eq!(mask_str("mcp-remote@latest"), "mcp-remote@latest");
        assert_eq!(mask_str("--transport"), "--transport");
    }

    #[test]
    fn a_bare_high_entropy_arg_is_hidden() {
        assert_eq!(mask_str(SECRET), HIDDEN);
    }
}
