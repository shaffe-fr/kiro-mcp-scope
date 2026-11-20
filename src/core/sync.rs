//! Applying the catalog to every copy of a server.
//!
//! The model is a plan, then an apply. [`plan`] reads every copy file and works
//! out, without touching anything, what each would become and why; `--check` and
//! `--sync --dry-run` stop there, and `--sync` hands the result to
//! [`apply`]. Nothing the plan carries for display holds a secret value.
//!
//! # What a sync does to one server entry
//!
//! The catalog wins on every field, with three exceptions:
//!
//! - `disabled` belongs to the project and is always kept;
//! - `autoApprove` and `disabledTools` follow a three-version rule (see
//!   [`merge_entry`]): a copy still carrying a value the catalog once had is
//!   refreshed, a copy carrying a value the catalog never had is a local
//!   customization and is kept, with a warning;
//! - in an agent spec, `env` and `headers` are kept unless `--force`, because
//!   it is not known that agents resolve `${KMS__...}` references. Differences
//!   are reported, values hidden.
//!
//! Only servers already present in a file are updated: sync never adds nor
//! removes one. Activation stays the business of `.kiro/settings/mcp.json`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::backup::{BackupStore, Recorder};
use super::config::Config;
use super::copies::{
    agent_sources_for, collect, Category, CopyFile, Include, Locations, ProjectDir,
};
use super::discovery::scan_root;
use super::jsonedit::{detect_layout, parse, render_document, replace_member};
use super::lint::{lint_servers, Warning};
use super::mask::{is_always_hidden, render_field};
use super::secrets::scan_server;
use super::state::{load_state, normalize, State, TRACKED_FIELDS};
use super::types::Catalog;
use super::workspace::workspace_mcp_path;
use super::worktrees::{families, main_worktree, same_path};

/// What to synchronize.
#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// Server names to restrict to; empty means every catalog server.
    pub servers: Vec<String>,
    /// Replace local customizations and, in agents, `env` / `headers`.
    pub force: bool,
    pub include: Include,
    pub no_worktrees: bool,
}

/// What would change for one server in one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerChange {
    pub server: String,
    /// `- field: old` / `+ field: new` lines, values masked.
    pub lines: Vec<String>,
    /// Explanations: customizations kept or replaced, `env` differences.
    pub notes: Vec<String>,
    /// Whether the entry differs after merging.
    pub changed: bool,
}

/// What would change for one file.
#[derive(Debug, Clone)]
pub struct FilePlan {
    pub file: CopyFile,
    /// Servers that change or carry a note.
    pub changes: Vec<ServerChange>,
    /// The content after the sync; `None` when nothing changes.
    pub new_text: Option<String>,
    /// Why the file could not be examined.
    pub error: Option<String>,
    /// Suspicious configuration found while reading it.
    pub warnings: Vec<Warning>,
    /// Where else the content comes from, for files something regenerates.
    pub hints: Vec<String>,
}

impl FilePlan {
    /// Whether this file would be rewritten.
    pub fn diverges(&self) -> bool {
        self.new_text.is_some()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub files: Vec<FilePlan>,
    /// Suspicious configuration in the catalog itself.
    pub catalog_warnings: Vec<Warning>,
}

impl Plan {
    /// Files that differ and that `include` puts in scope: what `--sync`
    /// writes and what `--check` fails on.
    pub fn pending(&self, include: Include) -> Vec<&FilePlan> {
        self.files
            .iter()
            .filter(|f| f.diverges() && include.covers(f.file.category))
            .collect()
    }

    /// Files that differ but that `include` leaves out: reported only.
    pub fn reported_only(&self, include: Include) -> Vec<&FilePlan> {
        self.files
            .iter()
            .filter(|f| f.diverges() && !include.covers(f.file.category))
            .collect()
    }

    pub fn errors(&self) -> Vec<&FilePlan> {
        self.files.iter().filter(|f| f.error.is_some()).collect()
    }
}

// ---------------------------------------------------------------------------
// Merging one entry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Same as the catalog (ignoring order): leave the copy as it is.
    Keep,
    /// The copy holds an older catalog value: take the current one.
    Take,
    /// A local value, and nothing is known of the past: the catalog wins.
    TakeNoHistory,
    /// A local value, replaced because of `--force`.
    TakeForced,
    /// A local customization: keep it.
    KeepOverride,
}

/// The merged entry and the report of what it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    pub entry: Map<String, Value>,
    pub lines: Vec<String>,
    pub notes: Vec<String>,
    pub changed: bool,
}

/// A command reduced to what identifies the program: no directory, no
/// extension, lower case. KiroCrew writes agent specs with the command resolved
/// to an absolute path (`C:\\Program Files\\nodejs\\npx.CMD` for `npx`), which is
/// the same program, not a divergence.
fn command_stem(command: &str) -> String {
    let name = command.rsplit(['/', '\\']).next().unwrap_or(command);
    let lower = name.to_lowercase();
    for extension in [".cmd", ".bat", ".exe", ".com", ".ps1"] {
        if let Some(stem) = lower.strip_suffix(extension) {
            return stem.to_string();
        }
    }
    lower
}

fn same_program(existing: &Value, wanted: &Value) -> bool {
    match (existing.as_str(), wanted.as_str()) {
        (Some(a), Some(b)) => command_stem(a) == command_stem(b),
        _ => false,
    }
}

fn is_tracked(field: &str) -> bool {
    TRACKED_FIELDS.contains(&field)
}

fn string_set(value: Option<&Value>) -> Vec<String> {
    normalize(value).unwrap_or_default()
}

fn verdict_for(
    name: &str,
    field: &str,
    catalog: &Map<String, Value>,
    existing: &Map<String, Value>,
    state: &State,
    force: bool,
) -> Verdict {
    let wanted = normalize(catalog.get(field));
    let current = normalize(existing.get(field));
    if current == wanted {
        Verdict::Keep
    } else if state.knows(name, field, &current) {
        Verdict::Take
    } else if force {
        Verdict::TakeForced
    } else if !state.has_history(name) {
        Verdict::TakeNoHistory
    } else {
        Verdict::KeepOverride
    }
}

fn list_text(items: &[String]) -> String {
    items.join(", ")
}

fn tracked_note(
    field: &str,
    verdict: Verdict,
    catalog: &Map<String, Value>,
    existing: &Map<String, Value>,
) -> Option<String> {
    match verdict {
        Verdict::Keep | Verdict::Take => None,
        Verdict::TakeNoHistory if existing.get(field).is_none() => Some(format!(
            "{field} added from the catalog (this copy had none; no sync history yet, so the catalog wins)"
        )),
        Verdict::TakeNoHistory => Some(format!(
            "local {field} replaced by the catalog's (no sync history yet, so the catalog wins)"
        )),
        Verdict::TakeForced => Some(format!("local {field} replaced by the catalog's (--force)")),
        Verdict::KeepOverride => {
            if field == "disabledTools" {
                let wanted = string_set(catalog.get(field));
                let have = string_set(existing.get(field));
                let open: Vec<String> = wanted
                    .into_iter()
                    .filter(|tool| !have.contains(tool))
                    .collect();
                if !open.is_empty() {
                    return Some(format!(
                        "kept a local disabledTools that blocks less than the catalog: it leaves {} \
                         tool(s) active that the catalog disables: {}. --force replaces it",
                        open.len(),
                        list_text(&open)
                    ));
                }
            }
            Some(format!(
                "kept a local {field} that differs from the catalog's. --force replaces it"
            ))
        }
    }
}

/// Describe how `env` / `headers` differ, never showing a value.
fn hidden_map_note(
    field: &str,
    existing: &Map<String, Value>,
    catalog: &Map<String, Value>,
) -> Option<String> {
    let a = existing.get(field).and_then(Value::as_object);
    let b = catalog.get(field).and_then(Value::as_object);
    if existing.get(field) == catalog.get(field) {
        return None;
    }
    let empty = Map::new();
    let a = a.unwrap_or(&empty);
    let b = b.unwrap_or(&empty);
    let only_here: Vec<&str> = a
        .keys()
        .filter(|k| !b.contains_key(*k))
        .map(String::as_str)
        .collect();
    let only_catalog: Vec<&str> = b
        .keys()
        .filter(|k| !a.contains_key(*k))
        .map(String::as_str)
        .collect();
    let differing: Vec<&str> = a
        .keys()
        .filter(|k| b.get(*k).is_some_and(|other| other != &a[*k]))
        .map(String::as_str)
        .collect();
    let mut parts = Vec::new();
    if !only_here.is_empty() {
        parts.push(format!("only here: {}", only_here.join(", ")));
    }
    if !only_catalog.is_empty() {
        parts.push(format!("only in the catalog: {}", only_catalog.join(", ")));
    }
    if !differing.is_empty() {
        parts.push(format!("different value: {}", differing.join(", ")));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "kept this file's {field} ({}; values hidden). --force replaces it",
        parts.join("; ")
    ))
}

/// Diff lines between two entries, values masked.
fn diff_lines(before: &Map<String, Value>, after: &Map<String, Value>) -> Vec<String> {
    let mut keys: Vec<&String> = before.keys().collect();
    keys.extend(after.keys().filter(|k| !before.contains_key(*k)));
    let mut lines = Vec::new();
    for key in keys {
        let (old, new) = (before.get(key), after.get(key));
        if old == new {
            continue;
        }
        if is_always_hidden(key) {
            let empty = Map::new();
            let a = old.and_then(Value::as_object).unwrap_or(&empty);
            let b = new.and_then(Value::as_object).unwrap_or(&empty);
            let mut names: Vec<&String> = a.keys().chain(b.keys()).collect();
            names.sort();
            names.dedup();
            let changed: Vec<&str> = names
                .into_iter()
                .filter(|n| a.get(*n) != b.get(*n))
                .map(String::as_str)
                .collect();
            lines.push(format!("~ {key}: {} (values hidden)", changed.join(", ")));
            continue;
        }
        if let Some(old) = old {
            lines.push(format!("- {key}: {}", render_field(key, old)));
        }
        if let Some(new) = new {
            lines.push(format!("+ {key}: {}", render_field(key, new)));
        }
    }
    lines
}

/// Merge the catalog's definition of a server into an existing copy.
///
/// Key order of the copy is kept; fields only the catalog has are appended in
/// catalog order; fields only the copy has are dropped, except `disabled`.
///
/// For `autoApprove` and `disabledTools` the value is normalized (sorted,
/// deduplicated) before being compared with the catalog's and with the history
/// of what the catalog said. Equal: untouched. A past catalog value: refreshed.
/// Anything else is a local customization, kept and reported, unless `force`.
/// With no history at all (first run) the catalog wins and the replacement is
/// reported.
pub fn merge_entry(
    name: &str,
    catalog: &Map<String, Value>,
    existing: &Map<String, Value>,
    category: Category,
    state: &State,
    force: bool,
) -> Merged {
    let keep_hidden = category == Category::Agent && !force;
    let mut notes = Vec::new();

    let mut verdicts: Vec<(&str, Verdict)> = Vec::new();
    for field in TRACKED_FIELDS {
        let verdict = verdict_for(name, field, catalog, existing, state, force);
        if let Some(note) = tracked_note(field, verdict, catalog, existing) {
            notes.push(note);
        }
        verdicts.push((field, verdict));
    }
    let takes = |field: &str| {
        verdicts.iter().any(|(f, v)| {
            *f == field
                && matches!(
                    v,
                    Verdict::Take | Verdict::TakeNoHistory | Verdict::TakeForced
                )
        })
    };

    if keep_hidden {
        for field in ["env", "headers"] {
            if let Some(note) = hidden_map_note(field, existing, catalog) {
                notes.push(note);
            }
        }
    }

    let mut entry = Map::new();
    for (key, value) in existing {
        if key == "disabled" {
            entry.insert(key.clone(), value.clone());
        } else if is_tracked(key) {
            if takes(key) {
                if let Some(wanted) = catalog.get(key) {
                    entry.insert(key.clone(), wanted.clone());
                }
            } else {
                entry.insert(key.clone(), value.clone());
            }
        } else if (keep_hidden && is_always_hidden(key))
            || (category == Category::Agent
                && key == "command"
                && catalog
                    .get(key)
                    .is_some_and(|wanted| same_program(value, wanted)))
        {
            entry.insert(key.clone(), value.clone());
        } else if let Some(wanted) = catalog.get(key) {
            entry.insert(key.clone(), wanted.clone());
        }
    }
    for (key, wanted) in catalog {
        if entry.contains_key(key) || existing.contains_key(key) || key == "disabled" {
            continue;
        }
        let skip = if is_tracked(key) {
            !takes(key)
        } else {
            keep_hidden && is_always_hidden(key)
        };
        if !skip {
            entry.insert(key.clone(), wanted.clone());
        }
    }

    let changed = &entry != existing;
    let lines = if changed {
        diff_lines(existing, &entry)
    } else {
        Vec::new()
    };
    Merged {
        entry,
        lines,
        notes,
        changed,
    }
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

fn plan_file(
    file: &CopyFile,
    catalog: &Catalog,
    state: &State,
    locations: &Locations,
    options: &PlanOptions,
) -> FilePlan {
    let mut plan = FilePlan {
        file: file.clone(),
        changes: Vec::new(),
        new_text: None,
        error: None,
        warnings: Vec::new(),
        hints: Vec::new(),
    };

    let original = match std::fs::read_to_string(&file.path) {
        Ok(text) => text,
        Err(err) => {
            plan.error = Some(format!("cannot read ({})", err.kind()));
            return plan;
        }
    };
    let data = match parse(&original) {
        Ok(data) => data,
        Err(message) => {
            plan.error = Some(message);
            return plan;
        }
    };
    let Some(servers) = data.get("mcpServers").and_then(Value::as_object) else {
        return plan;
    };

    if file.category != Category::Example {
        plan.warnings = lint_servers(servers);
    }

    let layout = detect_layout(&original);
    let mut text = original.clone();
    // Agent servers grouped by where else KiroCrew could read them from.
    let mut source_groups: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();

    for (name, existing) in servers {
        let Some(existing) = existing.as_object() else {
            continue;
        };
        if !options.servers.is_empty() && !options.servers.contains(name) {
            continue;
        }
        let Some(wanted) = catalog.servers.get(name).and_then(Value::as_object) else {
            continue;
        };

        if !scan_server(name, wanted).is_empty() {
            plan.changes.push(ServerChange {
                server: name.clone(),
                lines: Vec::new(),
                notes: vec![
                    "the catalog entry holds a literal secret, so it is not propagated. \
                     Replace it with a ${VAR} reference in the catalog"
                        .to_string(),
                ],
                changed: false,
            });
            continue;
        }

        let merged = merge_entry(name, wanted, existing, file.category, state, options.force);
        if merged.changed {
            match replace_member(
                &text,
                &["mcpServers", name],
                &Value::Object(merged.entry.clone()),
                &layout,
            ) {
                Some(next) => text = next,
                None => {
                    plan.error = Some(format!("cannot locate server \"{name}\" in the text"));
                    return plan;
                }
            }
        }
        if merged.changed || !merged.notes.is_empty() {
            if file.category == Category::Agent {
                let sources = agent_sources_for(locations, name);
                let key = match (sources.global, sources.crew_store) {
                    (true, true) => "~/.kiro/settings/mcp.json and ~/.kiro/crew/mcp.json",
                    (true, false) => "~/.kiro/settings/mcp.json",
                    (false, true) => "~/.kiro/crew/mcp.json",
                    (false, false) => "",
                };
                source_groups
                    .entry(key.to_string())
                    .or_default()
                    .push(name.clone());
            }
            plan.changes.push(ServerChange {
                server: name.clone(),
                lines: merged.lines,
                notes: merged.notes,
                changed: merged.changed,
            });
        }
    }

    for (source, names) in source_groups {
        plan.hints.push(if source.is_empty() {
            format!(
                "defined only in this agent spec (not in ~/.kiro/settings/mcp.json nor \
                 ~/.kiro/crew/mcp.json), so rewriting it is the whole fix: {}",
                names.join(", ")
            )
        } else {
            format!(
                "also defined in {source}: KiroCrew can re-render this agent from there, so fix \
                 it at that source rather than relying on this rewrite: {}",
                names.join(", ")
            )
        });
    }

    if text != original {
        plan.new_text = Some(text);
    }
    plan
}

/// Work out what a sync would do, without writing anything.
pub fn plan(
    catalog: &Catalog,
    state: &State,
    files: &[CopyFile],
    locations: &Locations,
    options: &PlanOptions,
) -> Plan {
    let files = files
        .iter()
        .filter(|file| !(options.no_worktrees && file.in_worktree))
        .map(|file| plan_file(file, catalog, state, locations, options))
        .collect();
    Plan {
        files,
        catalog_warnings: lint_servers(&catalog.servers),
    }
}

/// The directories to examine: every project under the roots and the
/// worktrees linked to it, wherever they live.
pub fn project_dirs(roots: &[PathBuf], config: &Config) -> Vec<ProjectDir> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for root in roots {
        candidates.extend(scan_root(root, config.max_depth));
    }
    candidates.sort();
    candidates.dedup();

    let mut dirs = Vec::new();
    for family in families(&candidates) {
        dirs.push(ProjectDir {
            dir: family.parent.clone(),
            worktree: false,
        });
        for worktree in family.worktrees {
            dirs.push(ProjectDir {
                dir: worktree,
                worktree: true,
            });
        }
    }
    dirs
}

/// Every copy file in scope for a command, from the roots.
pub fn gather(roots: &[PathBuf], config: &Config, locations: &Locations) -> Vec<CopyFile> {
    collect(&project_dirs(roots, config), locations)
}

/// Write what the plan says, through the recorder (backup, then atomic
/// write). Only files `include` covers are written. Returns the paths written.
pub fn apply(
    plan: &Plan,
    include: Include,
    recorder: &mut Recorder,
) -> std::io::Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for file in plan.pending(include) {
        if let Some(text) = &file.new_text {
            if recorder.write(&file.file.path, text)? {
                written.push(file.file.path.clone());
            }
        }
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// Initializing a worktree from its parent
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("{0} is not a linked git worktree.")]
    NotAWorktree(PathBuf),

    #[error("The parent project {0} has no .kiro/settings/mcp.json to copy.")]
    ParentHasNoConfig(PathBuf),

    #[error("{0} already exists. Use --force to replace it (a backup is kept).")]
    AlreadyConfigured(PathBuf),

    #[error("Cannot read {path}: {message}")]
    Unreadable { path: PathBuf, message: String },
}

/// What `--init-worktree` would write.
#[derive(Debug, Clone)]
pub struct InitPlan {
    pub parent: PathBuf,
    pub target: PathBuf,
    pub text: String,
    /// Catalog servers copied, with the catalog's definition.
    pub catalog_servers: Vec<String>,
    /// Servers the catalog does not know (e.g. `laravel-boost`), copied as is.
    pub outside_servers: Vec<String>,
    pub notes: Vec<String>,
}

/// Plan the initial `.kiro/settings/mcp.json` of a worktree from its parent.
///
/// The set of servers is the parent's, `disabled` included. For servers of the
/// catalog the values come from the catalog, with local customizations the
/// three-version rule recognizes; the others are copied as they are.
pub fn plan_init_worktree(
    catalog: &Catalog,
    state: &State,
    target: &Path,
    force: bool,
) -> Result<InitPlan, InitError> {
    let parent = match main_worktree(target) {
        Some(main) if !same_path(&main, target) => main,
        _ => return Err(InitError::NotAWorktree(target.to_path_buf())),
    };

    let source = workspace_mcp_path(&parent);
    let raw = match std::fs::read_to_string(&source) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(InitError::ParentHasNoConfig(parent));
        }
        Err(err) => {
            return Err(InitError::Unreadable {
                path: source,
                message: err.kind().to_string(),
            })
        }
    };
    let data = parse(&raw).map_err(|message| InitError::Unreadable {
        path: source.clone(),
        message,
    })?;

    let destination = workspace_mcp_path(target);
    if destination.exists() && !force {
        return Err(InitError::AlreadyConfigured(destination));
    }

    let empty = Map::new();
    let servers = data
        .get("mcpServers")
        .and_then(Value::as_object)
        .unwrap_or(&empty);

    let mut out = Map::new();
    let mut catalog_servers = Vec::new();
    let mut outside_servers = Vec::new();
    let mut notes = Vec::new();
    for (name, entry) in servers {
        match (
            catalog.servers.get(name).and_then(Value::as_object),
            entry.as_object(),
        ) {
            (Some(wanted), Some(existing)) => {
                let merged = merge_entry(name, wanted, existing, Category::Settings, state, false);
                for note in merged.notes {
                    notes.push(format!("{name}: {note}"));
                }
                out.insert(name.clone(), Value::Object(merged.entry));
                catalog_servers.push(name.clone());
            }
            _ => {
                out.insert(name.clone(), entry.clone());
                outside_servers.push(name.clone());
            }
        }
    }

    let mut document = Map::new();
    document.insert("mcpServers".to_string(), Value::Object(out));
    let text = render_document(&Value::Object(document), &detect_layout(&raw));

    Ok(InitPlan {
        parent,
        target: destination,
        text,
        catalog_servers,
        outside_servers,
        notes,
    })
}

/// Write the plan through the recorder. Returns whether a file was written.
pub fn apply_init(plan: &InitPlan, recorder: &mut Recorder) -> std::io::Result<bool> {
    recorder.write(&plan.target, &plan.text)
}

/// Plan and write a worktree's config in one go, with a backup run. For the
/// TUI's explicit "initialize from parent" action; never replaces a file.
pub fn init_worktree_now(
    catalog: &Catalog,
    state_path: &Path,
    backup_root: &Path,
    target: &Path,
) -> Result<InitPlan, String> {
    let state = load_state(state_path).map_err(|err| err.to_string())?;
    let plan = plan_init_worktree(catalog, &state, target, false).map_err(|err| err.to_string())?;
    let mut recorder = BackupStore::new(backup_root.to_path_buf()).begin("init-worktree");
    apply_init(&plan, &mut recorder).map_err(|err| format!("Write failed: {err}"))?;
    recorder.finish().map_err(|err| err.to_string())?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::backup::BackupStore;
    use crate::core::worktrees::testing::{linked, main_repo};
    use serde_json::json;

    const SECRET: &str = "sk-live-9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c";

    fn obj(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn catalog_of(value: Value) -> Catalog {
        Catalog {
            servers: obj(value),
        }
    }

    fn rovo_catalog() -> Catalog {
        catalog_of(json!({
            "rovo": {
                "command": "npx",
                "args": ["mcp-remote@latest", "https://mcp.atlassian.com/v2/mcp?tools=all"],
                "autoApprove": ["getJiraIssue", "search"],
                "disabledTools": ["deleteThing", "createThing"]
            }
        }))
    }

    fn state_after(catalog: &Catalog) -> State {
        let mut state = State::default();
        state.record_catalog(catalog);
        state
    }

    // -- merge_entry ------------------------------------------------------

    #[test]
    fn the_catalog_wins_but_disabled_and_key_order_stay() {
        let catalog = obj(json!({ "url": "https://new", "autoApprove": ["a"] }));
        let existing = obj(json!({ "disabled": true, "url": "https://old", "autoApprove": ["a"] }));
        let merged = merge_entry(
            "s",
            &catalog,
            &existing,
            Category::Settings,
            &State::default(),
            false,
        );
        assert!(merged.changed);
        let keys: Vec<&String> = merged.entry.keys().collect();
        assert_eq!(keys, ["disabled", "url", "autoApprove"]);
        assert_eq!(merged.entry["disabled"], json!(true));
        assert_eq!(merged.entry["url"], json!("https://new"));
        assert_eq!(
            merged.lines,
            vec!["- url: \"https://old\"", "+ url: \"https://new\""]
        );
    }

    #[test]
    fn a_copy_without_disabled_does_not_gain_the_catalogs() {
        let catalog = obj(json!({ "url": "u", "disabled": true }));
        let existing = obj(json!({ "url": "u" }));
        let merged = merge_entry(
            "s",
            &catalog,
            &existing,
            Category::Settings,
            &State::default(),
            false,
        );
        assert!(!merged.changed);
        assert!(!merged.entry.contains_key("disabled"));
    }

    #[test]
    fn a_reordered_list_is_not_a_change() {
        let catalog = obj(json!({ "command": "x", "autoApprove": ["a", "b"] }));
        let existing = obj(json!({ "command": "x", "autoApprove": ["b", "a"] }));
        let merged = merge_entry(
            "s",
            &catalog,
            &existing,
            Category::Settings,
            &State::default(),
            false,
        );
        assert!(!merged.changed);
        assert_eq!(
            merged.entry["autoApprove"],
            json!(["b", "a"]),
            "copy left as it was"
        );
    }

    #[test]
    fn an_older_catalog_value_is_refreshed() {
        let old =
            catalog_of(json!({ "s": { "command": "x", "disabledTools": ["editJiraIssue"] } }));
        let mut state = state_after(&old);
        let new = catalog_of(json!({ "s": { "command": "x", "disabledTools": ["editJira"] } }));
        state.record_catalog(&new);

        let existing = obj(json!({ "command": "x", "disabledTools": ["editJiraIssue"] }));
        let merged = merge_entry(
            "s",
            &new.servers["s"].as_object().unwrap().clone(),
            &existing,
            Category::Settings,
            &state,
            false,
        );
        assert!(merged.changed);
        assert_eq!(merged.entry["disabledTools"], json!(["editJira"]));
        assert!(merged.notes.is_empty(), "a refresh is not a customization");
    }

    #[test]
    fn a_local_customization_is_kept_and_a_weaker_block_is_called_out() {
        let catalog = rovo_catalog();
        let state = state_after(&catalog);
        let wanted = catalog.servers["rovo"].as_object().unwrap().clone();
        let existing = obj(json!({
            "command": "npx",
            "args": ["mcp-remote@latest", "https://mcp.atlassian.com/v1/mcp"],
            "autoApprove": ["getJiraIssue", "search"],
            "disabledTools": ["deleteThing", "somethingElse"]
        }));

        let merged = merge_entry(
            "rovo",
            &wanted,
            &existing,
            Category::Settings,
            &state,
            false,
        );
        assert_eq!(
            merged.entry["disabledTools"],
            json!(["deleteThing", "somethingElse"])
        );
        assert_eq!(
            merged.entry["args"], wanted["args"],
            "other fields still follow the catalog"
        );
        let note = merged.notes.join(" | ");
        assert!(note.contains("blocks less than the catalog"), "{note}");
        assert!(
            note.contains("createThing"),
            "names the tool left active: {note}"
        );
        assert!(!note.contains("somethingElse"));
    }

    #[test]
    fn a_stronger_local_block_is_kept_without_the_weaker_warning() {
        let catalog = rovo_catalog();
        let state = state_after(&catalog);
        let wanted = catalog.servers["rovo"].as_object().unwrap().clone();
        let existing = obj(json!({
            "command": "npx",
            "args": wanted["args"].clone(),
            "autoApprove": wanted["autoApprove"].clone(),
            "disabledTools": ["deleteThing", "createThing", "extra"]
        }));
        let merged = merge_entry(
            "rovo",
            &wanted,
            &existing,
            Category::Settings,
            &state,
            false,
        );
        assert!(!merged.changed);
        let note = merged.notes.join(" | ");
        assert!(note.contains("kept a local disabledTools"), "{note}");
        assert!(!note.contains("blocks less"));
    }

    #[test]
    fn force_replaces_a_customization() {
        let catalog = rovo_catalog();
        let state = state_after(&catalog);
        let wanted = catalog.servers["rovo"].as_object().unwrap().clone();
        let existing = obj(json!({
            "command": "npx",
            "args": wanted["args"].clone(),
            "disabledTools": ["mine"]
        }));
        let merged = merge_entry("rovo", &wanted, &existing, Category::Settings, &state, true);
        assert_eq!(merged.entry["disabledTools"], wanted["disabledTools"]);
        assert!(merged.notes.iter().any(|n| n.contains("--force")));
    }

    #[test]
    fn with_no_history_the_catalog_wins_and_says_so() {
        let catalog = rovo_catalog();
        let wanted = catalog.servers["rovo"].as_object().unwrap().clone();
        let existing = obj(json!({
            "command": "npx",
            "args": wanted["args"].clone(),
            "disabledTools": ["mine"]
        }));
        let merged = merge_entry(
            "rovo",
            &wanted,
            &existing,
            Category::Settings,
            &State::default(),
            false,
        );
        assert_eq!(merged.entry["disabledTools"], wanted["disabledTools"]);
        assert!(merged
            .notes
            .iter()
            .any(|n| n.contains("no sync history yet")));
    }

    #[test]
    fn an_agents_resolved_command_is_the_same_program() {
        let catalog = obj(json!({ "command": "npx", "args": ["a"] }));
        let existing = obj(json!({
            "command": "C:\\Program Files\\nodejs\\npx.CMD",
            "args": ["a"]
        }));
        let merged = merge_entry(
            "s",
            &catalog,
            &existing,
            Category::Agent,
            &State::default(),
            false,
        );
        assert!(!merged.changed, "{:?}", merged.lines);

        // Anywhere else it is a real difference.
        let merged = merge_entry(
            "s",
            &catalog,
            &existing,
            Category::Settings,
            &State::default(),
            false,
        );
        assert_eq!(merged.entry["command"], json!("npx"));

        // A different program is still a difference for an agent.
        let other = obj(json!({ "command": "C:\\bin\\node.exe", "args": ["a"] }));
        let merged = merge_entry(
            "s",
            &catalog,
            &other,
            Category::Agent,
            &State::default(),
            false,
        );
        assert_eq!(merged.entry["command"], json!("npx"));
    }

    #[test]
    fn agents_keep_env_and_report_the_difference_without_values() {
        let catalog = obj(json!({ "command": "npx", "env": { "TOKEN": "${KMS__X}" } }));
        let existing = obj(json!({
            "command": "npx-old",
            "env": { "TOKEN": SECRET, "EXTRA": "1" }
        }));
        let merged = merge_entry(
            "s",
            &catalog,
            &existing,
            Category::Agent,
            &State::default(),
            false,
        );
        assert_eq!(merged.entry["env"], existing["env"], "env preserved");
        assert_eq!(merged.entry["command"], json!("npx"));
        let report = format!("{:?} {:?}", merged.lines, merged.notes);
        assert!(!report.contains(SECRET));
        assert!(report.contains("EXTRA"));
        assert!(report.contains("TOKEN"));

        let forced = merge_entry(
            "s",
            &catalog,
            &existing,
            Category::Agent,
            &State::default(),
            true,
        );
        assert_eq!(forced.entry["env"], catalog["env"]);
        let report = format!("{:?} {:?}", forced.lines, forced.notes);
        assert!(!report.contains(SECRET), "diff lines hide env values too");
    }

    // -- planning and applying over a fake tree --------------------------

    struct Tree {
        _dir: tempfile::TempDir,
        root: PathBuf,
        locations: Locations,
        files: Vec<CopyFile>,
        catalog: Catalog,
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// Project, worktree, `.ai/mcp`, Herd clone, agent, `.example` — each
    /// carrying the v1 URL of the `rovo` server.
    fn tree() -> Tree {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let home = root.join("home");
        let proj = root.join("proj");
        main_repo(&proj);
        let wt = proj.join(".worktrees").join("feat");
        linked(&proj, &wt, "feat");

        let v1 = |extra: &str| {
            format!(
                "{{\n  \"mcpServers\": {{\n    \"rovo\": {{\n      \"command\": \"npx\",\n      \"args\": [\n        \"mcp-remote@latest\",\n        \"https://mcp.atlassian.com/v1/mcp\"\n      ],\n{extra}      \"autoApprove\": [\"getJiraIssue\", \"search\"],\n      \"disabledTools\": [\"deleteThing\", \"createThing\"]\n    }},\n    \"laravel-boost\": {{ \"command\": \"php\", \"args\": [\"artisan\", \"boost:mcp\"] }}\n  }}\n}}\n"
            )
        };

        write(
            &proj.join(".kiro/settings/mcp.json"),
            &v1("      \"disabled\": true,\n"),
        );
        write(&wt.join(".kiro/settings/mcp.json"), &v1(""));
        write(&proj.join(".ai/mcp/mcp.json"), &v1(""));
        write(&proj.join(".kiro/settings/mcp.json.example"), &v1(""));
        let locations = Locations::for_home(&home);
        write(
            &locations.herd_clones.join("4273542e/site/.ai/mcp/mcp.json"),
            &v1(""),
        );
        write(
            &home.join(".kiro/agents/kirocrew.json"),
            &format!(
                "{{\"name\":\"kirocrew\",\"mcpServers\":{{\"rovo\":{{\"command\":\"npx\",\"args\":[\"mcp-remote@latest\",\"https://mcp.atlassian.com/v1/mcp\"],\"env\":{{\"API_TOKEN\":\"{SECRET}\"}},\"autoApprove\":[\"getJiraIssue\",\"search\"],\"disabledTools\":[\"deleteThing\",\"createThing\"]}}}}}}"
            ),
        );
        write(
            &home.join(".kiro/agents/kirocrew.json.bak"),
            "{ not even json",
        );

        let catalog = rovo_catalog();
        let files = collect(
            &[
                ProjectDir {
                    dir: proj.clone(),
                    worktree: false,
                },
                ProjectDir {
                    dir: wt,
                    worktree: true,
                },
            ],
            &locations,
        );
        Tree {
            _dir: dir,
            root,
            locations,
            files,
            catalog,
        }
    }

    fn all_text(plan: &Plan) -> String {
        let mut out = String::new();
        for file in &plan.files {
            out.push_str(&format!(
                "{:?}",
                (&file.changes, &file.hints, &file.warnings, &file.error)
            ));
        }
        out.push_str(&format!("{:?}", plan.catalog_warnings));
        out
    }

    #[test]
    fn plan_lists_every_copy_and_changes_nothing() {
        let t = tree();
        let before: Vec<String> = t
            .files
            .iter()
            .map(|f| std::fs::read_to_string(&f.path).unwrap())
            .collect();

        let plan = plan(
            &t.catalog,
            &State::default(),
            &t.files,
            &t.locations,
            &PlanOptions::default(),
        );

        let categories: Vec<Category> = plan.files.iter().map(|f| f.file.category).collect();
        for expected in [
            Category::Settings,
            Category::Boost,
            Category::Example,
            Category::Agent,
        ] {
            assert!(categories.contains(&expected), "{expected:?} found");
        }
        assert_eq!(
            plan.files.len(),
            6,
            "project, worktree, boost x2 (project, Herd clone), example, agent"
        );
        assert!(
            plan.files.iter().all(|f| f.diverges()),
            "all copies carry v1"
        );
        assert!(
            plan.files.iter().all(|f| f.error.is_none()),
            "the .bak is not examined"
        );

        let after: Vec<String> = t
            .files
            .iter()
            .map(|f| std::fs::read_to_string(&f.path).unwrap())
            .collect();
        assert_eq!(before, after, "planning never writes");
        assert!(
            !all_text(&plan).contains(SECRET),
            "no secret in anything displayable"
        );
    }

    #[test]
    fn scope_follows_include_and_no_worktrees() {
        let t = tree();
        let plan_all = plan(
            &t.catalog,
            &State::default(),
            &t.files,
            &t.locations,
            &PlanOptions::default(),
        );

        let default = plan_all.pending(Include::default());
        assert!(default
            .iter()
            .all(|f| matches!(f.file.category, Category::Settings | Category::Boost)));
        assert_eq!(default.len(), 4);
        assert_eq!(
            plan_all.reported_only(Include::default()).len(),
            2,
            "example and agent"
        );

        let everything = Include {
            agents: true,
            examples: true,
        };
        assert_eq!(plan_all.pending(everything).len(), 6);

        let options = PlanOptions {
            no_worktrees: true,
            ..Default::default()
        };
        let no_wt = plan(
            &t.catalog,
            &State::default(),
            &t.files,
            &t.locations,
            &options,
        );
        assert_eq!(no_wt.files.len(), 5);
        assert!(no_wt.files.iter().all(|f| !f.file.in_worktree));
    }

    #[test]
    fn only_named_servers_are_touched() {
        let t = tree();
        let options = PlanOptions {
            servers: vec!["nope".into()],
            ..Default::default()
        };
        let plan = plan(
            &t.catalog,
            &State::default(),
            &t.files,
            &t.locations,
            &options,
        );
        assert!(plan.files.iter().all(|f| !f.diverges()));
    }

    #[test]
    fn apply_rewrites_only_the_changed_span_with_backups() {
        let t = tree();
        let plan = plan(
            &t.catalog,
            &State::default(),
            &t.files,
            &t.locations,
            &PlanOptions::default(),
        );
        let store = BackupStore::new(t.root.join("backups"));
        let mut recorder = store.begin("sync");
        let written = apply(&plan, Include::default(), &mut recorder).unwrap();
        let id = recorder.finish().unwrap().unwrap();
        assert_eq!(written.len(), 4);

        for file in &t.files {
            let text = std::fs::read_to_string(&file.path).unwrap();
            let data = parse(&text).expect("still valid JSON");
            let in_scope = matches!(file.category, Category::Settings | Category::Boost);
            let url = data["mcpServers"]["rovo"]["args"][1]
                .as_str()
                .unwrap()
                .to_string();
            if in_scope {
                assert_eq!(
                    url,
                    "https://mcp.atlassian.com/v2/mcp?tools=all",
                    "{}",
                    file.path.display()
                );
                assert_eq!(
                    data["mcpServers"]["laravel-boost"]["command"],
                    json!("php"),
                    "other servers untouched"
                );
            } else {
                assert!(url.contains("/v1/"), "{} left alone", file.path.display());
            }
        }

        let project = std::fs::read_to_string(t.root.join("proj/.kiro/settings/mcp.json")).unwrap();
        assert!(project.contains("\"disabled\": true"), "disabled kept");
        assert!(
            project.contains("\"laravel-boost\": { \"command\": \"php\""),
            "neighbouring formatting kept byte for byte"
        );

        let manifest = store.runs().into_iter().find(|m| m.id == id).unwrap();
        assert_eq!(manifest.entries.len(), 4);
        assert!(manifest.entries.iter().all(|e| e.backup.is_some()));
    }

    #[test]
    fn a_second_sync_is_a_no_op() {
        let t = tree();
        let options = PlanOptions::default();
        let state = State::default();
        let first = plan(&t.catalog, &state, &t.files, &t.locations, &options);
        let store = BackupStore::new(t.root.join("backups"));
        let mut recorder = store.begin("sync");
        apply(
            &first,
            Include {
                agents: true,
                examples: true,
            },
            &mut recorder,
        )
        .unwrap();
        recorder.finish().unwrap();

        let again = plan(
            &t.catalog,
            &state_after(&t.catalog),
            &t.files,
            &t.locations,
            &options,
        );
        let still = again.pending(Include {
            agents: true,
            examples: true,
        });
        assert!(
            still.is_empty(),
            "nothing left to do: {:?}",
            still.iter().map(|f| &f.file.path).collect::<Vec<_>>()
        );

        let mut recorder = store.begin("sync");
        let written = apply(
            &again,
            Include {
                agents: true,
                examples: true,
            },
            &mut recorder,
        )
        .unwrap();
        assert!(written.is_empty());
        assert_eq!(recorder.finish().unwrap(), None, "no run, no backup");
    }

    #[test]
    fn agents_keep_their_secret_and_explain_where_the_source_is() {
        let t = tree();
        write(
            &t.locations.home.join(".kiro/crew/mcp.json"),
            r#"{ "mcpServers": { "rovo": { "command": "npx" } } }"#,
        );
        let plan = plan(
            &t.catalog,
            &State::default(),
            &t.files,
            &t.locations,
            &PlanOptions::default(),
        );
        let agent = plan
            .files
            .iter()
            .find(|f| f.file.category == Category::Agent)
            .unwrap();
        assert!(
            agent
                .hints
                .iter()
                .any(|h| h.contains("~/.kiro/crew/mcp.json")),
            "{:?}",
            agent.hints
        );
        assert!(
            agent
                .warnings
                .iter()
                .any(|w| w.message.contains("env.API_TOKEN")),
            "plaintext secret flagged by key"
        );

        let store = BackupStore::new(t.root.join("backups"));
        let mut recorder = store.begin("sync");
        apply(
            &plan,
            Include {
                agents: true,
                examples: false,
            },
            &mut recorder,
        )
        .unwrap();
        recorder.finish().unwrap();
        let text = std::fs::read_to_string(&agent.file.path).unwrap();
        assert!(text.contains(SECRET), "env kept without --force");
        assert!(text.contains("/v2/mcp?tools=all"));
        assert!(!all_text(&plan).contains(SECRET));
    }

    #[test]
    fn a_catalog_secret_is_never_propagated() {
        let t = tree();
        let leaky = catalog_of(json!({
            "rovo": { "command": "npx", "env": { "API_TOKEN": SECRET } }
        }));
        let plan = plan(
            &leaky,
            &State::default(),
            &t.files,
            &t.locations,
            &PlanOptions::default(),
        );
        assert!(plan.files.iter().all(|f| !f.diverges()));
        assert!(plan
            .files
            .iter()
            .flat_map(|f| &f.changes)
            .all(|c| c.notes[0].contains("literal secret")));
        assert!(!all_text(&plan).contains(SECRET));
    }

    #[test]
    fn unreadable_files_are_reported_not_fatal() {
        let t = tree();
        let broken = t.root.join("broken.json");
        write(&broken, "{ nope");
        let files = vec![CopyFile {
            path: broken,
            category: Category::Boost,
            in_worktree: false,
        }];
        let plan = plan(
            &t.catalog,
            &State::default(),
            &files,
            &t.locations,
            &PlanOptions::default(),
        );
        assert_eq!(plan.errors().len(), 1);
        assert!(plan.errors()[0]
            .error
            .as_ref()
            .unwrap()
            .contains("invalid JSON"));
    }

    // -- init worktree -------------------------------------------------------

    #[test]
    fn init_copies_the_parent_set_with_catalog_values_and_disabled() {
        let t = tree();
        let wt = t.root.join("proj/.worktrees/feat");
        std::fs::remove_file(wt.join(".kiro/settings/mcp.json")).unwrap();
        // The catalog moved on since the parent's copy was written.
        let mut state = State::default();
        state.record_catalog(&t.catalog);

        let init = plan_init_worktree(&t.catalog, &state, &wt, false).unwrap();
        assert_eq!(init.catalog_servers, vec!["rovo"]);
        assert_eq!(init.outside_servers, vec!["laravel-boost"]);

        let data = parse(&init.text).unwrap();
        assert_eq!(
            data["mcpServers"]["rovo"]["disabled"],
            json!(true),
            "disabled copied"
        );
        assert_eq!(
            data["mcpServers"]["rovo"]["args"][1],
            json!("https://mcp.atlassian.com/v2/mcp?tools=all"),
            "catalog value, not the parent's stale one"
        );
        assert_eq!(
            data["mcpServers"]["laravel-boost"]["args"][1],
            json!("boost:mcp")
        );

        let store = BackupStore::new(t.root.join("backups"));
        let mut recorder = store.begin("init-worktree");
        assert!(apply_init(&init, &mut recorder).unwrap());
        let id = recorder.finish().unwrap().unwrap();
        assert!(wt.join(".kiro/settings/mcp.json").is_file());

        store.undo(Some(&id), false, false).unwrap();
        assert!(
            !wt.join(".kiro/settings/mcp.json").exists(),
            "undo removes the created file"
        );
    }

    #[test]
    fn init_refuses_an_existing_config_unless_forced_and_non_worktrees() {
        let t = tree();
        let wt = t.root.join("proj/.worktrees/feat");
        assert!(matches!(
            plan_init_worktree(&t.catalog, &State::default(), &wt, false).unwrap_err(),
            InitError::AlreadyConfigured(_)
        ));
        assert!(plan_init_worktree(&t.catalog, &State::default(), &wt, true).is_ok());
        assert!(matches!(
            plan_init_worktree(&t.catalog, &State::default(), &t.root.join("proj"), false)
                .unwrap_err(),
            InitError::NotAWorktree(_)
        ));
    }

    #[test]
    fn init_needs_a_parent_config() {
        let t = tree();
        std::fs::remove_file(t.root.join("proj/.kiro/settings/mcp.json")).unwrap();
        let wt = t.root.join("proj/.worktrees/feat");
        std::fs::remove_file(wt.join(".kiro/settings/mcp.json")).unwrap();
        assert!(matches!(
            plan_init_worktree(&t.catalog, &State::default(), &wt, false).unwrap_err(),
            InitError::ParentHasNoConfig(_)
        ));
    }
}
