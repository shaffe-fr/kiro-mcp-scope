//! Tool configuration, distinct from the catalog.
//!
//! The catalog says which servers exist; the config says where to look for
//! projects. It lives next to the catalog at
//! `~/.kiro/mcp-catalog.config.json` and Kiro never reads it.
//!
//! No default paths are guessed. Without a config, the fallback is the current
//! directory and its parent: the launch location is always relevant, with no
//! setup. Root resolution priority is CLI > config > fallback.

use std::path::{Path, PathBuf};

use serde_json::Value;

const DEFAULT_MAX_DEPTH: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Roots to scan for project discovery.
    pub roots: Vec<String>,
    /// Maximum scan depth under each root.
    pub max_depth: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }
}

/// The config location, `~/.kiro/mcp-catalog.config.json`.
pub fn default_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".kiro").join("mcp-catalog.config.json"))
}

/// Replace a leading `~` with the user's home directory. A `~` that is not the
/// whole path or a path prefix (`~/`, `~\`) is left as-is.
pub fn expand_home(path: &str) -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return PathBuf::from(path);
    };
    if path == "~" {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Fallback roots when none are configured: the current directory and its
/// parent. Covers launching from a project as well as from a folder grouping
/// several projects.
pub fn fallback_roots(cwd: &Path) -> Vec<PathBuf> {
    match cwd.parent() {
        Some(parent) if parent != cwd => vec![cwd.to_path_buf(), parent.to_path_buf()],
        _ => vec![cwd.to_path_buf()],
    }
}

/// Load the config. A missing file yields the default (empty roots, depth 2).
/// Invalid or partial JSON degrades field by field rather than failing.
pub fn load_config(path: &Path) -> Config {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Config::default();
    };
    let Ok(data) = serde_json::from_str::<Value>(&raw) else {
        return Config::default();
    };

    let roots = data
        .get("roots")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let max_depth = data
        .get("maxDepth")
        .and_then(Value::as_u64)
        .filter(|&d| d > 0)
        .map(|d| d as u32)
        .unwrap_or(DEFAULT_MAX_DEPTH);

    Config { roots, max_depth }
}

/// Resolve the roots to scan: absolute, `~`-expanded, deduplicated, order
/// preserved. Priority: explicit (CLI) > config > fallback (cwd + parent).
pub fn resolve_roots(explicit: &[String], config: &Config, cwd: &Path) -> Vec<PathBuf> {
    let chosen: Vec<PathBuf> = if !explicit.is_empty() {
        explicit.iter().map(|r| expand_home(r)).collect()
    } else if !config.roots.is_empty() {
        config.roots.iter().map(|r| expand_home(r)).collect()
    } else {
        fallback_roots(cwd)
    };

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for root in chosen {
        let abs = if root.is_absolute() {
            root
        } else {
            cwd.join(root)
        };
        if seen.insert(abs.clone()) {
            out.push(abs);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_replaces_leading_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_home("~"), home);
        assert_eq!(expand_home("~/projects"), home.join("projects"));
        assert_eq!(expand_home("~\\projects"), home.join("projects"));
        assert_eq!(expand_home("/abs/path"), PathBuf::from("/abs/path"));
        // A tilde mid-path is not a home reference.
        assert_eq!(expand_home("a/~/b"), PathBuf::from("a/~/b"));
    }

    #[test]
    fn fallback_is_cwd_and_parent() {
        let cwd = Path::new("/home/user/project");
        let roots = fallback_roots(cwd);
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/home/user/project"),
                PathBuf::from("/home/user")
            ]
        );
    }

    #[test]
    fn missing_config_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = load_config(&dir.path().join("absent.json"));
        assert_eq!(config, Config::default());
        assert_eq!(config.max_depth, 2);
    }

    #[test]
    fn loads_roots_and_depth() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.json");
        std::fs::write(&file, r#"{ "roots": ["~/a", "/b"], "maxDepth": 4 }"#).unwrap();
        let config = load_config(&file);
        assert_eq!(config.roots, vec!["~/a".to_string(), "/b".to_string()]);
        assert_eq!(config.max_depth, 4);
    }

    #[test]
    fn invalid_depth_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.json");
        std::fs::write(&file, r#"{ "maxDepth": 0 }"#).unwrap();
        assert_eq!(load_config(&file).max_depth, 2);
    }

    #[test]
    fn explicit_roots_win_over_config() {
        let config = Config {
            roots: vec!["/from/config".to_string()],
            max_depth: 2,
        };
        let cwd = Path::new("/cwd");
        let roots = resolve_roots(&["/from/cli".to_string()], &config, cwd);
        assert_eq!(roots, vec![PathBuf::from("/from/cli")]);
    }

    #[test]
    fn config_roots_used_when_no_explicit() {
        let config = Config {
            roots: vec!["/from/config".to_string()],
            max_depth: 2,
        };
        let roots = resolve_roots(&[], &config, Path::new("/cwd"));
        assert_eq!(roots, vec![PathBuf::from("/from/config")]);
    }

    #[test]
    fn fallback_used_when_nothing_configured() {
        let roots = resolve_roots(&[], &Config::default(), Path::new("/home/user/proj"));
        assert_eq!(
            roots,
            vec![
                PathBuf::from("/home/user/proj"),
                PathBuf::from("/home/user")
            ]
        );
    }

    #[test]
    fn duplicate_roots_are_removed() {
        let config = Config::default();
        let roots = resolve_roots(
            &["/a".to_string(), "/a".to_string(), "/b".to_string()],
            &config,
            Path::new("/cwd"),
        );
        assert_eq!(roots, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn relative_root_resolved_against_cwd() {
        let config = Config::default();
        let roots = resolve_roots(&["sub".to_string()], &config, Path::new("/cwd"));
        assert_eq!(roots, vec![PathBuf::from("/cwd/sub")]);
    }
}
