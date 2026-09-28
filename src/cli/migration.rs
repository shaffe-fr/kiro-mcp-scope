//! `--migrate` and `--rollback`: moving between the global model and the
//! catalog model, both ways.
//!
//! Migration runs once. It keeps the original global as `mcp.json.bak`, writes
//! the catalog with secrets turned into `${KMS__…}` references, empties the
//! global's `mcpServers`, and defines the `KMS__…` variables in the user's
//! environment. Rollback undoes it from that backup: servers go back into the
//! global, the catalog is set aside, and the variables the migration defined are
//! removed — re-derived from the backup, so no other variable is touched.
//! Project `mcp.json` files are never modified by either direction.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use thiserror::Error;

use crate::core::discovery::scan_root;
use crate::core::env_store::EnvStore;
use crate::core::migrate::{migrate, MigrationResult};
use crate::core::workspace::{read_workspace, serialize, WorkspaceFile};

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("No global mcp.json at {0}: nothing to migrate.")]
    GlobalNotFound(PathBuf),

    #[error("Invalid JSON in {path}: {message}")]
    InvalidJson { path: PathBuf, message: String },

    #[error("The global mcp.json defines no servers: nothing to migrate.")]
    NothingToMigrate,

    #[error(
        "A catalog already exists at {0}: kms migrates only once. \
         Run `kms --rollback` first to migrate again."
    )]
    AlreadyMigrated(PathBuf),

    #[error(
        "A migration backup exists at {0} and holds servers the global mcp.json \
         no longer has; migrating again would overwrite it. Run `kms --rollback`, \
         or move the backup away."
    )]
    BackupInTheWay(PathBuf),

    #[error("No migration backup at {0}: nothing to roll back.")]
    NoBackup(PathBuf),

    #[error("{done}, but {action} {name} failed: {source}. {remedy}")]
    Env {
        done: &'static str,
        action: &'static str,
        name: String,
        remedy: &'static str,
        source: io::Error,
    },

    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MigrateOptions {
    pub dry_run: bool,
    /// Leave the user's environment alone; the report names the variables to
    /// define by hand.
    pub skip_env: bool,
}

/// The global backup written by migration, `mcp.json.bak` next to the global.
pub fn backup_path(global_path: &Path) -> PathBuf {
    global_path.with_extension("json.bak")
}

fn read_json_object(path: &Path) -> Result<Option<Map<String, Value>>, MigrationError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let value: Value = serde_json::from_str(&raw).map_err(|err| MigrationError::InvalidJson {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    Ok(Some(value.as_object().cloned().unwrap_or_default()))
}

fn servers_of(config: &Map<String, Value>) -> Map<String, Value> {
    config
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Serialize a config object in the deterministic project format.
fn serialize_config(mut config: Map<String, Value>) -> String {
    let servers = match config.remove("mcpServers") {
        Some(Value::Object(servers)) => servers,
        _ => Map::new(),
    };
    serialize(&WorkspaceFile {
        servers,
        extra: config,
    })
}

/// Whether overwriting the backup loses nothing: every server it holds is still
/// in the global, unchanged. True right after a rollback.
fn backup_is_redundant(backup: &Map<String, Value>, global: &Map<String, Value>) -> bool {
    let kept = servers_of(global);
    servers_of(backup)
        .iter()
        .all(|(name, entry)| kept.get(name) == Some(entry))
}

fn migration_report(
    result: &MigrationResult,
    options: MigrateOptions,
    env: &dyn EnvStore,
    backup: &Path,
) -> String {
    let dry = options.dry_run;
    let mut out = String::new();
    if dry {
        out.push_str("Dry run: nothing written, no variable set.\n");
    }
    out.push_str(&format!(
        "{} {} server(s) into the catalog.\n",
        if dry { "Would move" } else { "Moved" },
        result.catalog.servers.len()
    ));

    if !result.replacements.is_empty() {
        out.push_str("Secrets extracted into variables:\n");
        for r in &result.replacements {
            out.push_str(&format!(
                "  - {}.{} in \"{}\" -> ${{{}}}\n",
                r.field, r.key, r.server, r.variable
            ));
        }
    }

    if !result.variables.is_empty() {
        let names: Vec<&str> = result.variables.iter().map(|v| v.name.as_str()).collect();
        if options.skip_env {
            out.push_str(&format!(
                "Variables left for you to define (--no-env); their values are in {}:\n",
                backup.display()
            ));
        } else {
            out.push_str(if dry { "Would define:\n" } else { "Defined:\n" });
        }
        for name in &names {
            out.push_str(&format!("  - {name}\n"));
        }
        if !options.skip_env {
            out.push_str(&env.activation_hint());
            out.push('\n');
        }
        out.push_str(
            "Kiro expands only the variables you approve: allow these when Kiro asks, \
             or in its MCP settings.\n",
        );
    }

    if result.dropped_disabled {
        out.push_str(
            "Note: `disabled` was not carried over; catalog entries are normalized to active.\n",
        );
    }
    out
}

/// Whether launching kms should offer to migrate: no catalog yet, and the
/// global still defines servers. The backup is not consulted: it may have been
/// removed after a successful migration.
pub fn should_offer_migration(catalog_path: &Path, global_path: &Path) -> bool {
    !catalog_path.exists() && !crate::core::global::lingering_servers(global_path).is_empty()
}

/// Reading a `[Y/n]` answer: empty means yes. French answers count too.
pub fn accepts(answer: &str) -> bool {
    matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "" | "y" | "yes" | "o" | "oui"
    )
}

/// `--migrate`: move the global's servers into the catalog, their secrets into
/// `KMS__…` variables, and empty the global's `mcpServers`.
///
/// A catalog with a backup next to it comes from a migration: migrating again is
/// refused. A catalog without one — written by hand, or left over from a manual
/// restore — is set aside as `mcp-catalog.json.before-migrate`, never deleted.
pub fn run_migrate(
    catalog_path: &Path,
    global_path: &Path,
    options: MigrateOptions,
    env: &mut dyn EnvStore,
) -> Result<String, MigrationError> {
    let mut set_aside = None;
    if catalog_path.exists() {
        if backup_path(global_path).exists() {
            return Err(MigrationError::AlreadyMigrated(catalog_path.to_path_buf()));
        }
        set_aside = Some(next_free_path(
            &catalog_path.with_extension("json.before-migrate"),
        ));
    }
    let raw = match std::fs::read_to_string(global_path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(MigrationError::GlobalNotFound(global_path.to_path_buf()));
        }
        Err(err) => return Err(err.into()),
    };
    let global = read_json_object(global_path)?.unwrap_or_default();

    let backup = backup_path(global_path);
    if let Some(previous) = read_json_object(&backup)? {
        if !backup_is_redundant(&previous, &global) {
            return Err(MigrationError::BackupInTheWay(backup));
        }
    }

    let result = migrate(&global);
    if result.catalog.servers.is_empty() {
        return Err(MigrationError::NothingToMigrate);
    }

    let mut report = migration_report(&result, options, env, &backup);
    if let Some(target) = &set_aside {
        report.push_str(&format!(
            "{} the existing catalog aside as {}: no migration backup goes with it.\n",
            if options.dry_run { "Would set" } else { "Set" },
            target.display()
        ));
    }
    if options.dry_run {
        return Ok(report.trim_end().to_string());
    }

    if let Some(target) = &set_aside {
        std::fs::rename(catalog_path, target)?;
    }
    std::fs::write(&backup, &raw)?;
    if let Some(parent) = catalog_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        catalog_path,
        serialize(&WorkspaceFile {
            servers: result.catalog.servers.clone(),
            extra: Map::new(),
        }),
    )?;
    std::fs::write(global_path, serialize_config(result.emptied_global.clone()))?;

    if !options.skip_env {
        for var in &result.variables {
            env.set(&var.name, var.value.expose())
                .map_err(|source| MigrationError::Env {
                    done: "Migration files are written",
                    action: "defining",
                    name: var.name.clone(),
                    remedy: "Run `kms --rollback` to undo, or define it yourself.",
                    source,
                })?;
        }
    }

    report.push_str(&format!(
        "Wrote the catalog to {}\nEmptied mcpServers in {}\n\
         Kept the original as {}: it still holds the secrets in cleartext, keep it \
         out of version control. `kms --rollback` restores from it.",
        catalog_path.display(),
        global_path.display(),
        backup.display()
    ));
    Ok(report)
}

/// The first of `path`, `path.2`, `path.3`… that does not exist.
fn next_free_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    (2..)
        .map(|n| {
            let mut name = OsString::from(path.as_os_str());
            name.push(format!(".{n}"));
            PathBuf::from(name)
        })
        .find(|candidate| !candidate.exists())
        .expect("an unbounded range yields a free name")
}

/// Whether an entry's `env` or `headers` reference one of `names`.
fn references_any(entry: &Value, names: &BTreeSet<String>) -> bool {
    ["env", "headers"].iter().any(|field| {
        entry
            .get(*field)
            .and_then(Value::as_object)
            .is_some_and(|map| {
                map.values().filter_map(Value::as_str).any(|text| {
                    names
                        .iter()
                        .any(|name| text.contains(&format!("${{{name}}}")))
                })
            })
    })
}

/// Projects whose `mcp.json` still references a variable rollback removes, with
/// the servers concerned. A project entry replaces the global one of the same
/// name, so these servers stop resolving there.
fn projects_referencing(
    roots: &[PathBuf],
    max_depth: u32,
    names: &BTreeSet<String>,
) -> Vec<(PathBuf, Vec<String>)> {
    let mut dirs = BTreeSet::new();
    for root in roots {
        dirs.extend(scan_root(root, max_depth));
    }
    dirs.into_iter()
        .filter_map(|dir| {
            let ws = read_workspace(&dir).ok()?;
            let servers: Vec<String> = ws
                .servers
                .iter()
                .filter(|(_, entry)| references_any(entry, names))
                .map(|(name, _)| name.clone())
                .collect();
            (!servers.is_empty()).then_some((dir, servers))
        })
        .collect()
}

/// `--rollback`: restore the global model from the migration backup.
///
/// The restored global keeps its current top-level keys (Kiro may have updated
/// `powers` since) and gets the backup's servers back; a server added to the
/// global after migration is kept, and wins over the backup's version. The
/// backup itself stays in place.
pub fn run_rollback(
    catalog_path: &Path,
    global_path: &Path,
    roots: &[PathBuf],
    max_depth: u32,
    dry_run: bool,
    env: &mut dyn EnvStore,
) -> Result<String, MigrationError> {
    let backup = backup_path(global_path);
    let Some(original) = read_json_object(&backup)? else {
        return Err(MigrationError::NoBackup(backup));
    };
    let current = read_json_object(global_path)?;

    let current_servers = current.as_ref().map(servers_of).unwrap_or_default();
    let mut restored = current.clone().unwrap_or_else(|| original.clone());
    let mut servers = servers_of(&original);
    for (name, entry) in &current_servers {
        servers.insert(name.clone(), entry.clone());
    }
    restored.insert("mcpServers".to_string(), Value::Object(servers.clone()));

    let variables: BTreeSet<String> = migrate(&original)
        .variables
        .into_iter()
        .map(|var| var.name)
        .collect();
    let catalog_target = next_free_path(&catalog_path.with_extension("json.rolledback"));
    let affected = projects_referencing(roots, max_depth, &variables);

    let verb = |done: &'static str, planned: &'static str| if dry_run { planned } else { done };
    let mut out = String::new();
    if dry_run {
        out.push_str("Dry run: nothing written, no variable removed.\n");
    }
    out.push_str(&format!(
        "{} {} server(s) in {}.\n",
        verb("Restored", "Would restore"),
        servers.len(),
        global_path.display()
    ));
    if !current_servers.is_empty() {
        let names: Vec<&str> = current_servers.keys().map(String::as_str).collect();
        out.push_str(&format!(
            "Kept, as they are now: {} (added to the global after migration).\n",
            names.join(", ")
        ));
    }
    if catalog_path.exists() {
        out.push_str(&format!(
            "{} the catalog aside as {}.\n",
            verb("Set", "Would set"),
            catalog_target.display()
        ));
    }
    if !variables.is_empty() {
        out.push_str(&format!("{}\n", verb("Removed:", "Would remove:")));
        for name in &variables {
            out.push_str(&format!("  - {name}\n"));
        }
    }
    if !affected.is_empty() {
        out.push_str(
            "These projects still reference those variables; their entries override \
             the global ones by name, so these servers will not start there until you \
             remove or edit them:\n",
        );
        for (dir, names) in &affected {
            out.push_str(&format!("  - {}: {}\n", dir.display(), names.join(", ")));
        }
    }
    out.push_str(&format!("The backup {} is kept.", backup.display()));

    if dry_run {
        return Ok(out);
    }

    std::fs::write(global_path, serialize_config(restored))?;
    if catalog_path.exists() {
        std::fs::rename(catalog_path, &catalog_target)?;
    }
    for name in &variables {
        env.remove(name).map_err(|source| MigrationError::Env {
            done: "The global is restored and the catalog set aside",
            action: "removing",
            name: name.clone(),
            remedy: "Remove it yourself.",
            source,
        })?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::catalog::load_catalog;
    use crate::core::env_store::MemoryEnv;

    const SECRET: &str = "a7Kd93JxQ2pL8mZ0Wf4Rt6Yb1Nc5VgHh2Xj";

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        global: PathBuf,
        catalog: PathBuf,
    }

    fn fixture(global_json: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let global = root.join("settings").join("mcp.json");
        std::fs::create_dir_all(global.parent().unwrap()).unwrap();
        std::fs::write(&global, global_json).unwrap();
        Fixture {
            catalog: root.join("mcp-catalog.json"),
            global,
            root,
            _dir: dir,
        }
    }

    fn shared_token_global() -> String {
        format!(
            r#"{{
                "mcpServers": {{
                    "proxy-1": {{ "command": "npx", "env": {{ "AUTH_HEADER": "{SECRET}" }}, "disabled": true }},
                    "proxy-2": {{ "command": "npx", "env": {{ "AUTH_HEADER": "{SECRET}" }} }},
                    "local-tool": {{ "command": "tool", "env": {{ "FASTMCP_LOG_LEVEL": "ERROR" }} }}
                }},
                "powers": {{ "p": 1 }}
            }}"#
        )
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn migrate_for_real(f: &Fixture, env: &mut MemoryEnv) -> String {
        run_migrate(&f.catalog, &f.global, MigrateOptions::default(), env).unwrap()
    }

    #[test]
    fn offers_migration_only_without_catalog_and_with_global_servers() {
        let f = fixture(&shared_token_global());
        assert!(should_offer_migration(&f.catalog, &f.global));

        let mut env = MemoryEnv::default();
        migrate_for_real(&f, &mut env);
        assert!(
            !should_offer_migration(&f.catalog, &f.global),
            "catalog exists"
        );

        std::fs::remove_file(&f.catalog).unwrap();
        assert!(
            !should_offer_migration(&f.catalog, &f.global),
            "global is empty"
        );
    }

    #[test]
    fn answer_defaults_to_yes() {
        for yes in ["", "  ", "y", "Yes", "o", "OUI\n"] {
            assert!(accepts(yes), "{yes:?}");
        }
        for no in ["n", "no", "non", "later"] {
            assert!(!accepts(no), "{no:?}");
        }
    }

    #[test]
    fn migrate_writes_backup_catalog_global_and_variables() {
        let f = fixture(&shared_token_global());
        let mut env = MemoryEnv::default();
        migrate_for_real(&f, &mut env);

        let catalog = load_catalog(&f.catalog).unwrap();
        assert_eq!(
            catalog.servers["proxy-1"]["env"]["AUTH_HEADER"],
            serde_json::json!("${KMS__PROXY_1__AUTH_HEADER}")
        );
        assert!(catalog.servers["proxy-1"].get("disabled").is_none());

        let global = read(&f.global);
        assert_eq!(global["mcpServers"], serde_json::json!({}));
        assert_eq!(global["powers"]["p"], serde_json::json!(1));

        assert!(std::fs::read_to_string(backup_path(&f.global))
            .unwrap()
            .contains(SECRET));
        assert_eq!(
            env.vars.len(),
            2,
            "one variable per server holding the secret"
        );
        assert_eq!(env.vars["KMS__PROXY_2__AUTH_HEADER"], SECRET);
        assert_eq!(env.vars["KMS__PROXY_1__AUTH_HEADER"], SECRET);
    }

    #[test]
    fn migrate_report_never_contains_a_secret() {
        let f = fixture(&shared_token_global());
        let mut env = MemoryEnv::default();
        let report = migrate_for_real(&f, &mut env);
        assert!(!report.contains(SECRET));
        assert!(report.contains("KMS__PROXY_1__AUTH_HEADER"));
    }

    #[test]
    fn dry_run_writes_nothing_and_sets_nothing() {
        let f = fixture(&shared_token_global());
        let before = std::fs::read_to_string(&f.global).unwrap();
        let mut env = MemoryEnv::default();
        let options = MigrateOptions {
            dry_run: true,
            skip_env: false,
        };
        let report = run_migrate(&f.catalog, &f.global, options, &mut env).unwrap();

        assert!(report.contains("Would define"));
        assert!(!f.catalog.exists());
        assert!(!backup_path(&f.global).exists());
        assert_eq!(std::fs::read_to_string(&f.global).unwrap(), before);
        assert!(env.vars.is_empty());
    }

    #[test]
    fn no_env_migrates_files_but_leaves_the_environment_alone() {
        let f = fixture(&shared_token_global());
        let mut env = MemoryEnv::default();
        let options = MigrateOptions {
            dry_run: false,
            skip_env: true,
        };
        let report = run_migrate(&f.catalog, &f.global, options, &mut env).unwrap();
        assert!(f.catalog.exists());
        assert!(env.vars.is_empty());
        assert!(
            report.contains("KMS__PROXY_1__AUTH_HEADER"),
            "names what to define"
        );
    }

    #[test]
    fn second_migrate_is_refused() {
        let f = fixture(&shared_token_global());
        let mut env = MemoryEnv::default();
        migrate_for_real(&f, &mut env);
        let backup_before = std::fs::read_to_string(backup_path(&f.global)).unwrap();

        let err =
            run_migrate(&f.catalog, &f.global, MigrateOptions::default(), &mut env).unwrap_err();
        assert!(matches!(err, MigrationError::AlreadyMigrated(_)));
        assert_eq!(
            std::fs::read_to_string(backup_path(&f.global)).unwrap(),
            backup_before,
            "the original backup is intact"
        );
    }

    #[test]
    fn migrate_sets_aside_a_catalog_it_did_not_create() {
        // Left over from a manual restore: a catalog, no backup, servers back in
        // the global.
        let f = fixture(&shared_token_global());
        let leftover = r#"{ "mcpServers": { "hand-made": { "command": "h" } } }"#;
        std::fs::write(&f.catalog, leftover).unwrap();
        let mut env = MemoryEnv::default();

        let report = migrate_for_real(&f, &mut env);

        let aside = f.catalog.with_extension("json.before-migrate");
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), leftover);
        assert!(report.contains("before-migrate"));
        let catalog = load_catalog(&f.catalog).unwrap();
        assert!(catalog.servers.contains_key("proxy-1"));
        assert!(!catalog.servers.contains_key("hand-made"));
        assert_eq!(env.vars["KMS__PROXY_1__AUTH_HEADER"], SECRET);
    }

    #[test]
    fn dry_run_leaves_a_leftover_catalog_in_place() {
        let f = fixture(&shared_token_global());
        std::fs::write(&f.catalog, r#"{ "mcpServers": {} }"#).unwrap();
        let mut env = MemoryEnv::default();
        let options = MigrateOptions {
            dry_run: true,
            skip_env: false,
        };
        let report = run_migrate(&f.catalog, &f.global, options, &mut env).unwrap();
        assert!(report.contains("Would set the existing catalog aside"));
        assert!(f.catalog.exists());
        assert!(!f.catalog.with_extension("json.before-migrate").exists());
    }

    #[test]
    fn migrate_refuses_to_overwrite_a_backup_holding_lost_servers() {
        // Catalog removed by hand after a migration: the global is empty, the
        // backup is the only copy of the servers.
        let f = fixture(&shared_token_global());
        let mut env = MemoryEnv::default();
        migrate_for_real(&f, &mut env);
        std::fs::remove_file(&f.catalog).unwrap();

        let err =
            run_migrate(&f.catalog, &f.global, MigrateOptions::default(), &mut env).unwrap_err();
        assert!(matches!(err, MigrationError::BackupInTheWay(_)));
        assert!(std::fs::read_to_string(backup_path(&f.global))
            .unwrap()
            .contains(SECRET));
    }

    #[test]
    fn migrate_with_empty_global_is_refused() {
        let f = fixture(r#"{ "mcpServers": {} }"#);
        let mut env = MemoryEnv::default();
        let err =
            run_migrate(&f.catalog, &f.global, MigrateOptions::default(), &mut env).unwrap_err();
        assert!(matches!(err, MigrationError::NothingToMigrate));
        assert!(!backup_path(&f.global).exists());
    }

    #[test]
    fn migrate_without_global_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = MemoryEnv::default();
        let err = run_migrate(
            &dir.path().join("catalog.json"),
            &dir.path().join("mcp.json"),
            MigrateOptions::default(),
            &mut env,
        )
        .unwrap_err();
        assert!(matches!(err, MigrationError::GlobalNotFound(_)));
    }

    #[test]
    fn rollback_restores_servers_and_undoes_the_rest() {
        let f = fixture(&shared_token_global());
        let mut env = MemoryEnv::default();
        migrate_for_real(&f, &mut env);
        env.vars.insert("KMS_UNRELATED".into(), "mine".into());

        run_rollback(&f.catalog, &f.global, &[], 2, false, &mut env).unwrap();

        let global = read(&f.global);
        assert_eq!(
            global["mcpServers"]["proxy-1"]["env"]["AUTH_HEADER"],
            serde_json::json!(SECRET)
        );
        assert_eq!(
            global["mcpServers"]["proxy-1"]["disabled"],
            serde_json::json!(true)
        );
        assert!(global["mcpServers"].get("local-tool").is_some());
        assert_eq!(global["powers"]["p"], serde_json::json!(1));

        assert!(!f.catalog.exists());
        assert!(f.catalog.with_extension("json.rolledback").exists());
        assert!(backup_path(&f.global).exists(), "backup kept");

        assert!(!env.vars.contains_key("KMS__PROXY_1__AUTH_HEADER"));
        assert!(!env.vars.contains_key("KMS__PROXY_2__AUTH_HEADER"));
        assert_eq!(
            env.vars["KMS_UNRELATED"], "mine",
            "only derived names are removed"
        );
    }

    #[test]
    fn rollback_keeps_current_powers_and_servers_added_since() {
        let f = fixture(&shared_token_global());
        let mut env = MemoryEnv::default();
        migrate_for_real(&f, &mut env);
        std::fs::write(
            &f.global,
            r#"{ "mcpServers": { "newcomer": { "command": "n" } }, "powers": { "p": 2 } }"#,
        )
        .unwrap();

        let report = run_rollback(&f.catalog, &f.global, &[], 2, false, &mut env).unwrap();

        let global = read(&f.global);
        assert_eq!(
            global["powers"]["p"],
            serde_json::json!(2),
            "current powers kept"
        );
        assert!(global["mcpServers"].get("newcomer").is_some());
        assert!(global["mcpServers"].get("proxy-2").is_some());
        assert!(report.contains("newcomer"));
    }

    #[test]
    fn rollback_reports_projects_still_referencing_removed_variables() {
        let f = fixture(&shared_token_global());
        let mut env = MemoryEnv::default();
        migrate_for_real(&f, &mut env);

        let project = f.root.join("code").join("proj");
        let ws = project.join(".kiro").join("settings");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("mcp.json"),
            r#"{ "mcpServers": {
                "proxy-1": { "command": "npx", "env": { "AUTH_HEADER": "${KMS__PROXY_1__AUTH_HEADER}" } },
                "other": { "command": "o" }
            } }"#,
        )
        .unwrap();
        let before = std::fs::read_to_string(ws.join("mcp.json")).unwrap();

        let report = run_rollback(
            &f.catalog,
            &f.global,
            &[f.root.join("code")],
            2,
            false,
            &mut env,
        )
        .unwrap();

        assert!(report.contains("proj"));
        assert!(report.contains("proxy-1"));
        assert!(!report.contains("other"));
        assert_eq!(
            std::fs::read_to_string(ws.join("mcp.json")).unwrap(),
            before,
            "project files are never modified"
        );
    }

    #[test]
    fn rollback_dry_run_changes_nothing() {
        let f = fixture(&shared_token_global());
        let mut env = MemoryEnv::default();
        migrate_for_real(&f, &mut env);
        let global_before = std::fs::read_to_string(&f.global).unwrap();

        let report = run_rollback(&f.catalog, &f.global, &[], 2, true, &mut env).unwrap();

        assert!(report.contains("Would restore"));
        assert_eq!(std::fs::read_to_string(&f.global).unwrap(), global_before);
        assert!(f.catalog.exists());
        assert!(env.vars.contains_key("KMS__PROXY_1__AUTH_HEADER"));
    }

    #[test]
    fn rollback_without_backup_is_refused() {
        let f = fixture(&shared_token_global());
        let mut env = MemoryEnv::default();
        let err = run_rollback(&f.catalog, &f.global, &[], 2, false, &mut env).unwrap_err();
        assert!(matches!(err, MigrationError::NoBackup(_)));
    }

    #[test]
    fn migrate_again_after_rollback_is_allowed() {
        let f = fixture(&shared_token_global());
        let mut env = MemoryEnv::default();
        migrate_for_real(&f, &mut env);
        run_rollback(&f.catalog, &f.global, &[], 2, false, &mut env).unwrap();

        migrate_for_real(&f, &mut env);
        assert!(f.catalog.exists());
        assert_eq!(env.vars["KMS__PROXY_1__AUTH_HEADER"], SECRET);
    }

    #[test]
    fn repeated_rollbacks_never_overwrite_a_set_aside_catalog() {
        let f = fixture(&shared_token_global());
        let mut env = MemoryEnv::default();
        for _ in 0..2 {
            migrate_for_real(&f, &mut env);
            run_rollback(&f.catalog, &f.global, &[], 2, false, &mut env).unwrap();
        }
        assert!(f.catalog.with_extension("json.rolledback").exists());
        let mut second = OsString::from(f.catalog.with_extension("json.rolledback").as_os_str());
        second.push(".2");
        assert!(PathBuf::from(second).exists());
    }
}
