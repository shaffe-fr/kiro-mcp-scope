//! `--sync`, `--check`, `--init-worktree`, `--undo`, and the copy summaries of
//! `--status` and `--matrix`.
//!
//! The planning and the writing live in `core::sync`; this module reads the
//! environment, runs them and words the result. Everything printed comes from
//! masked material (see `core::mask`): a value of `env`, `headers`, a token in
//! a URL or an argument never reaches the output.

use std::path::{Path, PathBuf};

use crate::cli::args::Scope;
use crate::core::backup::{default_backup_root, BackupStore, KEEP_RUNS};
use crate::core::catalog::{default_catalog_path, load_catalog};
use crate::core::config::{default_config_path, load_config, resolve_roots, Config};
use crate::core::copies::{Category, Include, Locations};
use crate::core::state::{default_state_path, load_state, save_state};
use crate::core::sync::{
    apply, apply_init, gather, plan, plan_init_worktree, FilePlan, Plan, PlanOptions,
};
use crate::core::types::Catalog;
use crate::core::workspace::workspace_mcp_path;
use crate::core::worktrees::{linked_worktrees, main_worktree, same_path};

/// Where everything lives. Injectable, so tests never touch the real home.
#[derive(Debug, Clone)]
pub struct SyncEnv {
    pub catalog_path: PathBuf,
    pub state_path: PathBuf,
    pub backup_root: PathBuf,
    pub locations: Locations,
    pub config: Config,
    pub cwd: PathBuf,
}

impl SyncEnv {
    /// The real locations under the user's home.
    pub fn detect(cwd: &Path) -> Result<Self, String> {
        const NO_HOME: &str = "Cannot locate the home directory.";
        Ok(Self {
            catalog_path: default_catalog_path().ok_or(NO_HOME)?,
            state_path: default_state_path().ok_or(NO_HOME)?,
            backup_root: default_backup_root().ok_or(NO_HOME)?,
            locations: Locations::detect().ok_or(NO_HOME)?,
            config: default_config_path()
                .map(|path| load_config(&path))
                .unwrap_or_default(),
            cwd: cwd.to_path_buf(),
        })
    }

    fn roots(&self) -> Vec<PathBuf> {
        resolve_roots(&[], &self.config, &self.cwd)
    }

    fn store(&self) -> BackupStore {
        BackupStore::new(self.backup_root.clone())
    }
}

/// What a command prints and the exit code it asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub output: String,
    pub code: u8,
}

fn ok(output: String) -> Outcome {
    Outcome { output, code: 0 }
}

fn load(env: &SyncEnv) -> Result<Catalog, String> {
    load_catalog(&env.catalog_path).map_err(|err| err.to_string())
}

fn validate_scope(catalog: &Catalog, scope: &Scope) -> Result<(), String> {
    for name in &scope.servers {
        if !catalog.servers.contains_key(name) {
            return Err(format!("Server \"{name}\" is not in the catalog."));
        }
    }
    Ok(())
}

fn options_of(scope: &Scope, force: bool) -> PlanOptions {
    PlanOptions {
        servers: scope.servers.clone(),
        force,
        include: scope.include,
        no_worktrees: scope.no_worktrees,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn file_header(file: &FilePlan) -> String {
    let mut tags = vec![file.file.category.label().to_string()];
    if file.file.in_worktree {
        tags.push("worktree".to_string());
    }
    format!("{}  [{}]", file.file.path.display(), tags.join(", "))
}

fn render_file(file: &FilePlan, out: &mut String) {
    out.push_str(&file_header(file));
    out.push('\n');
    if let Some(error) = &file.error {
        out.push_str(&format!("  error: {error}\n"));
    }
    for change in &file.changes {
        out.push_str(&format!("  {}\n", change.server));
        for line in &change.lines {
            out.push_str(&format!("    {line}\n"));
        }
        for note in &change.notes {
            out.push_str(&format!("    note: {note}\n"));
        }
    }
    for hint in &file.hints {
        out.push_str(&format!("  hint: {hint}\n"));
    }
    for warning in &file.warnings {
        out.push_str(&format!(
            "  warning: {}: {}\n",
            warning.server, warning.message
        ));
    }
}

fn has_news(file: &FilePlan) -> bool {
    !file.changes.is_empty() || !file.warnings.is_empty() || file.error.is_some()
}

/// Everything about a plan, grouped by what it means for the exit code.
fn render_plan(plan: &Plan, include: Include) -> String {
    let mut out = String::new();

    if !plan.catalog_warnings.is_empty() {
        out.push_str("Catalog:\n");
        for warning in &plan.catalog_warnings {
            out.push_str(&format!(
                "  warning: {}: {}\n",
                warning.server, warning.message
            ));
        }
        out.push('\n');
    }

    let pending = plan.pending(include);
    if !pending.is_empty() {
        out.push_str("Differ from the catalog:\n");
        for file in &pending {
            render_file(file, &mut out);
        }
        out.push('\n');
    }

    let reported = plan.reported_only(include);
    if !reported.is_empty() {
        out.push_str("Differ too, reported only (add --include agents,examples to cover them):\n");
        for file in &reported {
            render_file(file, &mut out);
        }
        out.push('\n');
    }

    let others: Vec<&FilePlan> = plan
        .files
        .iter()
        .filter(|f| !f.diverges() && has_news(f))
        .collect();
    if !others.is_empty() {
        out.push_str("Also noted:\n");
        for file in others {
            render_file(file, &mut out);
        }
        out.push('\n');
    }

    out
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn warning_count(plan: &Plan) -> usize {
    plan.catalog_warnings.len() + plan.files.iter().map(|f| f.warnings.len()).sum::<usize>()
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn build_plan(
    env: &SyncEnv,
    catalog: &Catalog,
    state: &crate::core::state::State,
    scope: &Scope,
    force: bool,
) -> Plan {
    let files = gather(&env.roots(), &env.config, &env.locations);
    plan(
        catalog,
        state,
        &files,
        &env.locations,
        &options_of(scope, force),
    )
}

/// `--sync`.
pub fn sync(env: &SyncEnv, scope: &Scope, dry_run: bool, force: bool) -> Result<Outcome, String> {
    let catalog = load(env)?;
    validate_scope(&catalog, scope)?;
    let mut state = load_state(&env.state_path).map_err(|err| err.to_string())?;
    let plan = build_plan(env, &catalog, &state, scope, force);

    let mut out = render_plan(&plan, scope.include);
    let pending = plan.pending(scope.include);

    if let Some(file) = plan.errors().first() {
        if pending.is_empty() && !dry_run {
            // Nothing to write, but say why a file was skipped.
            out.push_str(&format!("Skipped {}.\n", file.file.path.display()));
        }
    }

    if dry_run {
        out.push_str(&format!(
            "Dry run: {} would be rewritten. Nothing was written.",
            plural(pending.len(), "file")
        ));
        return Ok(ok(out));
    }

    let store = env.store();
    let mut recorder = store.begin("sync");
    let written = apply(&plan, scope.include, &mut recorder).map_err(|err| {
        format!("Write failed: {err}. Earlier files were backed up; see kms --undo --list.")
    })?;
    let run = recorder.finish().map_err(|err| err.to_string())?;

    if state.record_catalog(&catalog) {
        save_state(&env.state_path, &state)
            .map_err(|err| format!("Cannot write {}: {err}", env.state_path.display()))?;
    }
    let _ = store.purge(KEEP_RUNS);

    if written.is_empty() {
        out.push_str("Nothing to do: every copy already matches the catalog.");
    } else {
        out.push_str(&format!("Updated {}.", plural(written.len(), "file")));
        if let Some(run) = run {
            out.push_str(&format!("\nBackup run {run}. Undo with: kms --undo {run}"));
        }
    }
    Ok(ok(out))
}

/// `--check`: exit 0 when aligned, 1 on divergence, 2 when a file cannot be
/// read. Warnings only count with `--strict`.
pub fn check(env: &SyncEnv, scope: &Scope, strict: bool) -> Result<Outcome, String> {
    let catalog = load(env)?;
    validate_scope(&catalog, scope)?;
    let state = load_state(&env.state_path).map_err(|err| err.to_string())?;
    let plan = build_plan(env, &catalog, &state, scope, false);

    let mut out = render_plan(&plan, scope.include);
    let pending = plan.pending(scope.include);
    let errors = plan.errors();
    let warnings = warning_count(&plan);

    let code = if !errors.is_empty() {
        2
    } else if !pending.is_empty() || (strict && warnings > 0) {
        1
    } else {
        0
    };

    if !pending.is_empty() {
        out.push_str(&format!(
            "{} differ from the catalog. Preview the fix with: kms --sync --dry-run",
            plural(pending.len(), "file")
        ));
    } else if errors.is_empty() {
        out.push_str("Every copy in scope matches the catalog.");
    }
    if !errors.is_empty() {
        out.push_str(&format!(
            "\n{} could not be read, so the result is incomplete.",
            plural(errors.len(), "file")
        ));
    }
    if warnings > 0 {
        out.push_str(&format!(
            "\n{}{}.",
            plural(warnings, "warning"),
            if strict {
                " (--strict: counted as failure)"
            } else {
                ""
            }
        ));
    }
    Ok(Outcome { output: out, code })
}

/// `--init-worktree [<path>]`: write a worktree's config from its parent.
pub fn init_worktree(
    env: &SyncEnv,
    path: Option<&str>,
    dry_run: bool,
    force: bool,
) -> Result<Outcome, String> {
    let catalog = load(env)?;
    let state = load_state(&env.state_path).map_err(|err| err.to_string())?;
    let target = match path {
        Some(raw) => {
            let expanded = crate::core::config::expand_home(raw);
            if expanded.is_absolute() {
                expanded
            } else {
                env.cwd.join(expanded)
            }
        }
        None => env.cwd.clone(),
    };

    let init =
        plan_init_worktree(&catalog, &state, &target, force).map_err(|err| err.to_string())?;

    let mut out = format!(
        "{} {}\n  from the parent project {}\n",
        if dry_run { "Would write" } else { "Wrote" },
        init.target.display(),
        init.parent.display()
    );
    if !init.catalog_servers.is_empty() {
        out.push_str(&format!(
            "  catalog servers (catalog values, disabled kept): {}\n",
            init.catalog_servers.join(", ")
        ));
    }
    if !init.outside_servers.is_empty() {
        out.push_str(&format!(
            "  copied as they are (not in the catalog): {}\n",
            init.outside_servers.join(", ")
        ));
    }
    for note in &init.notes {
        out.push_str(&format!("  note: {note}\n"));
    }

    if dry_run {
        out.push_str("Dry run: nothing was written.");
        return Ok(ok(out));
    }

    let store = env.store();
    let mut recorder = store.begin("init-worktree");
    apply_init(&init, &mut recorder).map_err(|err| format!("Write failed: {err}"))?;
    if let Some(run) = recorder.finish().map_err(|err| err.to_string())? {
        out.push_str(&format!("Backup run {run}. Undo with: kms --undo {run}"));
    } else {
        out.push_str("The file already had this content: nothing changed.");
    }
    Ok(ok(out))
}

/// `--undo [<id>]` and `--undo --list`.
pub fn undo(
    env: &SyncEnv,
    id: Option<&str>,
    list: bool,
    dry_run: bool,
    force: bool,
) -> Result<Outcome, String> {
    let store = env.store();

    if list {
        let runs = store.runs();
        if runs.is_empty() {
            return Ok(ok("No backup runs yet.".to_string()));
        }
        let mut out = String::new();
        for run in runs {
            out.push_str(&format!(
                "{}  {}  {}  {}\n",
                run.id,
                run.created,
                run.kind,
                plural(run.entries.len(), "file")
            ));
        }
        return Ok(ok(out.trim_end().to_string()));
    }

    let report = store
        .undo(id, force, dry_run)
        .map_err(|err| err.to_string())?;

    let mut out = format!(
        "{} run {}:\n",
        if dry_run { "Would undo" } else { "Undid" },
        report.run
    );
    for path in &report.restored {
        out.push_str(&format!("  restored {}\n", path.display()));
    }
    for (path, reason) in &report.skipped {
        out.push_str(&format!(
            "  skipped  {} ({reason}{})\n",
            path.display(),
            if reason.contains("modified") {
                "; --force overwrites it"
            } else {
                ""
            }
        ));
    }
    if report.restored.is_empty() && report.skipped.is_empty() {
        out.push_str("  nothing to restore\n");
    }
    if let Some(run) = &report.undo_run {
        out.push_str(&format!("What this undo replaced is saved as run {run}."));
    }
    Ok(ok(out.trim_end().to_string()))
}

// ---------------------------------------------------------------------------
// Summaries for --status and --matrix
// ---------------------------------------------------------------------------

fn summary_lines(plan: &Plan, skip: impl Fn(&FilePlan) -> bool) -> Vec<String> {
    let mut lines = Vec::new();
    for file in plan.files.iter().filter(|f| f.diverges() && !skip(f)) {
        let servers: Vec<&str> = file
            .changes
            .iter()
            .filter(|c| c.changed)
            .map(|c| c.server.as_str())
            .collect();
        lines.push(format!(
            "{}  [{}]  {}",
            file.file.path.display(),
            file.file.category.label(),
            servers.join(", ")
        ));
    }
    lines
}

/// The copies around the current project: its worktrees, Herd clones, agents,
/// examples. `--status` appends this.
pub fn status_section(env: &SyncEnv) -> Result<String, String> {
    let catalog = load(env)?;
    let state = load_state(&env.state_path).unwrap_or_default();

    let main = main_worktree(&env.cwd);
    let mut dirs = vec![crate::core::copies::ProjectDir {
        dir: env.cwd.clone(),
        worktree: main.as_ref().is_some_and(|m| !same_path(m, &env.cwd)),
    }];
    if let Some(main) = &main {
        if !same_path(main, &env.cwd) {
            dirs.push(crate::core::copies::ProjectDir {
                dir: main.clone(),
                worktree: false,
            });
        }
        for worktree in linked_worktrees(main) {
            if !same_path(&worktree, &env.cwd) {
                dirs.push(crate::core::copies::ProjectDir {
                    dir: worktree,
                    worktree: true,
                });
            }
        }
    }
    let files = crate::core::copies::collect(&dirs, &env.locations);
    let result = plan(
        &catalog,
        &state,
        &files,
        &env.locations,
        &PlanOptions::default(),
    );

    let own = workspace_mcp_path(&env.cwd);
    let lines = summary_lines(&result, |f| same_path(&f.file.path, &own));
    let warnings = warning_count(&result);

    let mut out = String::new();
    if !lines.is_empty() {
        out.push_str("Other copies that differ from the catalog:\n");
        for line in &lines {
            out.push_str(&format!("  {line}\n"));
        }
        out.push_str("Run kms --check for the details and kms --sync to align them.\n");
    }
    if warnings > 0 {
        out.push_str(&format!(
            "{} in this project's family and the shared places: run kms --check.\n",
            plural(warnings, "warning")
        ));
    }
    Ok(out.trim_end().to_string())
}

/// One line per category of diverging copy the matrix grid does not show.
pub fn matrix_summary(env: &SyncEnv, catalog: &Catalog) -> Vec<String> {
    let state = load_state(&env.state_path).unwrap_or_default();
    let result = build_plan(env, catalog, &state, &Scope::default(), false);
    let mut counts: std::collections::BTreeMap<Category, usize> = std::collections::BTreeMap::new();
    for file in result.files.iter().filter(|f| f.diverges()) {
        if file.file.category != Category::Settings {
            *counts.entry(file.file.category).or_default() += 1;
        }
    }
    if counts.is_empty() {
        return Vec::new();
    }
    let parts: Vec<String> = counts
        .into_iter()
        .map(|(category, n)| format!("{n} {}", category.label()))
        .collect();
    vec![format!(
        "Copies outside the grid that differ: {} (kms --check)",
        parts.join(", ")
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::worktrees::testing::{linked, main_repo};

    const SECRET: &str = "sk-live-9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c";
    const V1: &str = "https://mcp.atlassian.com/v1/mcp";
    const V2: &str = "https://mcp.atlassian.com/v2/mcp?tools=all";

    struct World {
        _dir: tempfile::TempDir,
        env: SyncEnv,
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn server_json(url: &str, disabled: bool) -> String {
        format!(
            "{{\n  \"mcpServers\": {{\n    \"rovo\": {{\n      \"command\": \"npx\",\n      \"args\": [\n        \"mcp-remote@latest\",\n        \"{url}\"\n      ],\n{}      \"autoApprove\": [\"getJiraIssue\"],\n      \"disabledTools\": [\"createThing\"]\n    }}\n  }}\n}}\n",
            if disabled { "      \"disabled\": true,\n" } else { "" }
        )
    }

    /// The scenario from the field: the catalog moves a server from v1 to v2
    /// while copies sit in a project, a worktree, Boost's config, a Herd
    /// clone, an agent and an example.
    fn world() -> World {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let home = root.join("home");
        let sites = root.join("sites");
        let proj = sites.join("proj");
        main_repo(&proj);
        std::fs::create_dir_all(proj.join(".kiro")).unwrap();
        let wt = proj.join(".worktrees").join("feat");
        linked(&proj, &wt, "feat");

        write(
            &proj.join(".kiro/settings/mcp.json"),
            &server_json(V1, true),
        );
        write(&wt.join(".kiro/settings/mcp.json"), &server_json(V1, false));
        write(&proj.join(".ai/mcp/mcp.json"), &server_json(V1, false));
        write(
            &proj.join(".kiro/settings/mcp.json.example"),
            &server_json(V1, false),
        );
        let locations = Locations::for_home(&home);
        write(
            &locations.herd_clones.join("4273542e/proj/.ai/mcp/mcp.json"),
            &server_json(V1, false),
        );
        write(
            &home.join(".kiro/agents/kirocrew.json"),
            &format!(
                "{{\"mcpServers\":{{\"rovo\":{{\"command\":\"npx\",\"args\":[\"mcp-remote@latest\",\"{V1}\"],\"env\":{{\"API_TOKEN\":\"{SECRET}\"}}}}}}}}"
            ),
        );
        write(
            &home.join(".kiro/mcp-catalog.json"),
            &format!(
                "{{\"mcpServers\":{{\"rovo\":{{\"command\":\"npx\",\"args\":[\"mcp-remote@latest\",\"{V2}\"],\"autoApprove\":[\"getJiraIssue\"],\"disabledTools\":[\"createThing\"]}}}}}}"
            ),
        );

        let env = SyncEnv {
            catalog_path: home.join(".kiro/mcp-catalog.json"),
            state_path: home.join(".kiro/mcp-catalog.state.json"),
            backup_root: home.join(".kiro/kms-backups"),
            locations,
            config: Config {
                roots: vec![sites.to_string_lossy().into_owned()],
                max_depth: 2,
            },
            cwd: proj,
        };
        World { _dir: dir, env }
    }

    fn all_files(world: &World) -> Vec<PathBuf> {
        let plan_files = gather(&world.env.roots(), &world.env.config, &world.env.locations);
        plan_files.into_iter().map(|f| f.path).collect()
    }

    fn snapshot(world: &World) -> Vec<(PathBuf, String)> {
        all_files(world)
            .into_iter()
            .map(|p| {
                let text = std::fs::read_to_string(&p).unwrap();
                (p, text)
            })
            .collect()
    }

    #[test]
    fn end_to_end_url_change_dry_run_then_real() {
        let w = world();
        let before = snapshot(&w);
        assert_eq!(before.len(), 6);

        // The dry run lists every copy in scope and writes nothing.
        let dry = sync(&w.env, &Scope::default(), true, false).unwrap();
        assert_eq!(dry.code, 0);
        for (path, _) in &before {
            assert!(
                dry.output.contains(&*path.to_string_lossy()),
                "lists {}",
                path.display()
            );
        }
        assert!(dry
            .output
            .contains(&format!("- args: [\"mcp-remote@latest\",\"{V1}\"]")));
        assert!(dry
            .output
            .contains(&format!("+ args: [\"mcp-remote@latest\",\"{V2}\"]")));
        assert!(dry.output.contains("Dry run: 4 files would be rewritten"));
        assert!(!dry.output.contains(SECRET));
        assert_eq!(snapshot(&w), before, "dry run changes no file");
        assert!(!w.env.backup_root.exists());
        assert!(
            !w.env.state_path.exists(),
            "read-only runs never write the state"
        );

        // The real run.
        let real = sync(&w.env, &Scope::default(), false, false).unwrap();
        assert!(real.output.contains("Updated 4 files."), "{}", real.output);
        assert!(!real.output.contains(SECRET));
        assert!(
            w.env.state_path.is_file(),
            "a real sync records the catalog history"
        );

        for (path, original) in &before {
            let text = std::fs::read_to_string(path).unwrap();
            let data: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
            let url = data["mcpServers"]["rovo"]["args"][1].as_str().unwrap();
            let name = path.to_string_lossy();
            if name.contains("agents") || name.ends_with(".example") {
                assert_eq!(&text, original, "{name} is reported only without --include");
            } else {
                assert_eq!(url, V2, "{name}");
            }
        }

        let project = std::fs::read_to_string(w.env.cwd.join(".kiro/settings/mcp.json")).unwrap();
        assert!(project.contains("\"disabled\": true"), "disabled kept");
        let worktree =
            std::fs::read_to_string(w.env.cwd.join(".worktrees/feat/.kiro/settings/mcp.json"))
                .unwrap();
        assert!(
            !worktree.contains("\"disabled\":"),
            "a worktree that had none gets none"
        );

        // Backups exist, one per rewritten file, outside the projects.
        let runs = BackupStore::new(w.env.backup_root.clone()).runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].entries.len(), 4);
        assert!(runs[0].entries.iter().all(|e| e.backup.is_some()));
        assert!(!w.env.cwd.join(".kiro/settings/mcp.json.bak").exists());

        // Everything in scope is now aligned.
        let again = check(&w.env, &Scope::default(), false).unwrap();
        assert_eq!(again.code, 0, "{}", again.output);
        let nothing = sync(&w.env, &Scope::default(), false, false).unwrap();
        assert!(nothing.output.contains("Nothing to do"));
        assert_eq!(
            BackupStore::new(w.env.backup_root.clone()).runs().len(),
            1,
            "a no-op sync makes no backup run"
        );
    }

    #[test]
    fn check_exits_nonzero_on_divergence_and_two_on_unreadable() {
        let w = world();
        let diverged = check(&w.env, &Scope::default(), false).unwrap();
        assert_eq!(diverged.code, 1);
        assert!(diverged.output.contains("4 files differ from the catalog"));
        assert!(
            diverged.output.contains("reported only"),
            "agent and example are listed even though out of scope: {}",
            diverged.output
        );
        assert!(
            diverged.output.contains("hint:"),
            "agents say where the source is"
        );
        assert!(!diverged.output.contains(SECRET));

        write(&w.env.cwd.join(".ai/mcp/mcp.json"), "{ broken");
        let broken = check(&w.env, &Scope::default(), false).unwrap();
        assert_eq!(broken.code, 2);
    }

    #[test]
    fn include_brings_agents_and_examples_into_the_scope() {
        let w = world();
        let scope = Scope {
            include: Include {
                agents: true,
                examples: true,
            },
            ..Scope::default()
        };
        let real = sync(&w.env, &scope, false, false).unwrap();
        assert!(real.output.contains("Updated 6 files."), "{}", real.output);

        let agent =
            std::fs::read_to_string(w.env.locations.home.join(".kiro/agents/kirocrew.json"))
                .unwrap();
        assert!(agent.contains(SECRET), "env is kept without --force");
        assert!(agent.contains(V2));
        assert_eq!(check(&w.env, &scope, false).unwrap().code, 0);
    }

    #[test]
    fn no_worktrees_and_server_filters_narrow_the_scope() {
        let w = world();
        let scope = Scope {
            no_worktrees: true,
            ..Scope::default()
        };
        let dry = sync(&w.env, &scope, true, false).unwrap();
        assert!(!dry.output.contains(".worktrees"));
        assert!(dry.output.contains("Dry run: 3 files"));

        let unknown = Scope {
            servers: vec!["ghost".into()],
            ..Scope::default()
        };
        assert!(sync(&w.env, &unknown, true, false)
            .unwrap_err()
            .contains("not in the catalog"));
    }

    #[test]
    fn undo_restores_and_can_be_listed() {
        let w = world();
        let before = snapshot(&w);
        sync(&w.env, &Scope::default(), false, false).unwrap();
        assert_ne!(snapshot(&w), before);

        let listing = undo(&w.env, None, true, false, false).unwrap();
        assert!(listing.output.contains("sync"));
        assert!(listing.output.contains("4 files"));

        let report = undo(&w.env, None, false, false, false).unwrap();
        assert!(report.output.contains("restored"), "{}", report.output);
        assert_eq!(
            snapshot(&w),
            before,
            "every file is back as it was, byte for byte"
        );

        let again = undo(&w.env, None, true, false, false).unwrap();
        assert!(
            again.output.contains("undo"),
            "the undo is itself a run: {}",
            again.output
        );
    }

    #[test]
    fn a_local_customization_is_kept_and_flagged_after_the_first_sync() {
        let w = world();
        // A first sync records the catalog's history.
        sync(&w.env, &Scope::default(), false, false).unwrap();
        // Someone then customizes one worktree.
        let wt_file = w.env.cwd.join(".worktrees/feat/.kiro/settings/mcp.json");
        let text = std::fs::read_to_string(&wt_file)
            .unwrap()
            .replace("createThing", "somethingElse");
        std::fs::write(&wt_file, &text).unwrap();

        let result = check(&w.env, &Scope::default(), false).unwrap();
        assert_eq!(
            result.code, 0,
            "a kept customization is not a divergence: {}",
            result.output
        );
        assert!(
            result.output.contains("blocks less than the catalog"),
            "{}",
            result.output
        );
        assert!(result.output.contains("createThing"));

        sync(&w.env, &Scope::default(), false, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&wt_file).unwrap(),
            text,
            "left alone"
        );

        let forced = sync(&w.env, &Scope::default(), false, true).unwrap();
        assert!(
            forced.output.contains("Updated 1 file."),
            "{}",
            forced.output
        );
        assert!(std::fs::read_to_string(&wt_file)
            .unwrap()
            .contains("createThing"));
    }

    #[test]
    fn init_worktree_dry_run_then_real_and_refusal() {
        let w = world();
        let wt = w.env.cwd.join(".worktrees/feat");
        std::fs::remove_file(wt.join(".kiro/settings/mcp.json")).unwrap();

        let dry = init_worktree(&w.env, Some(&wt.to_string_lossy()), true, false).unwrap();
        assert!(dry.output.contains("Would write"));
        assert!(dry.output.contains("rovo"));
        assert!(!wt.join(".kiro/settings/mcp.json").exists());

        let real = init_worktree(&w.env, Some(&wt.to_string_lossy()), false, false).unwrap();
        assert!(real.output.contains("Backup run"));
        let written = std::fs::read_to_string(wt.join(".kiro/settings/mcp.json")).unwrap();
        assert!(
            written.contains("\"disabled\": true"),
            "the parent's disabled is copied"
        );
        assert!(
            written.contains("/v2/mcp"),
            "catalog values, not the parent's v1"
        );

        let refused = init_worktree(&w.env, Some(&wt.to_string_lossy()), false, false).unwrap_err();
        assert!(refused.contains("already exists"));
        assert!(init_worktree(&w.env, Some(&wt.to_string_lossy()), false, true).is_ok());
    }

    #[test]
    fn init_worktree_defaults_to_the_current_directory() {
        let mut w = world();
        let wt = w.env.cwd.join(".worktrees/feat");
        std::fs::remove_file(wt.join(".kiro/settings/mcp.json")).unwrap();
        w.env.cwd = wt.clone();
        assert!(init_worktree(&w.env, None, false, false).is_ok());
        assert!(wt.join(".kiro/settings/mcp.json").is_file());
    }

    #[test]
    fn status_section_names_the_family_and_the_shared_places() {
        let w = world();
        let section = status_section(&w.env).unwrap();
        assert!(section.contains("Other copies that differ"));
        assert!(section.contains(".worktrees"));
        assert!(section.contains("[boost]"));
        assert!(section.contains("[agent]"));
        assert!(section.contains("[example]"));
        assert!(!section.contains(SECRET));

        let own = w.env.cwd.join(".kiro").join("settings").join("mcp.json");
        assert!(
            !section.contains(&format!("{}  [settings]", own.display())),
            "the current project's own file is already in --status"
        );
    }

    #[test]
    fn matrix_summary_counts_what_the_grid_does_not_show() {
        let w = world();
        let catalog = load_catalog(&w.env.catalog_path).unwrap();
        let lines = matrix_summary(&w.env, &catalog);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("2 boost"), "{}", lines[0]);
        assert!(lines[0].contains("1 agent"));
        assert!(lines[0].contains("1 example"));
        assert!(!lines[0].contains("settings"));
    }

    #[test]
    fn warnings_fail_a_strict_check_only() {
        let w = world();
        // Align everything in scope first.
        let scope = Scope {
            include: Include {
                agents: true,
                examples: true,
            },
            ..Scope::default()
        };
        sync(&w.env, &scope, false, false).unwrap();
        let relaxed = check(&w.env, &scope, false).unwrap();
        assert_eq!(relaxed.code, 0);
        assert!(
            relaxed.output.contains("env.API_TOKEN"),
            "plaintext secret flagged by key"
        );
        assert!(!relaxed.output.contains(SECRET));

        let strict = check(&w.env, &scope, true).unwrap();
        assert_eq!(strict.code, 1);
    }
}
