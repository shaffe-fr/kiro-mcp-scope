use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kms::cli::commands;
use kms::cli::migration::{self, MigrateOptions};
use kms::cli::syncing::{self, Outcome, SyncEnv};
use kms::cli::{parse_args, Command};
use kms::core::catalog::{default_catalog_path, load_catalog, CatalogError};
use kms::core::config::{default_config_path, load_config, resolve_roots};
use kms::core::discovery::discover_projects;
use kms::core::env_store::{default_env_store, EnvStore};
use kms::core::global::default_global_path;
use kms::core::matrix::Matrix;
use kms::core::types::Catalog;
use kms::ui::app::App;
use kms::ui::matrix_view::{InitPaths, MatrixView};
use kms::ui::project_view::ProjectView;

const HELP: &str = "\
kms — catalog of MCP servers, activated project by project in Kiro

Usage:
  kms                     launch the project view (TUI)
  kms --matrix            launch the matrix view (projects x servers)
  kms --list              list catalog servers and their state here
  kms --status            like --list, plus global drift and the other copies
  kms --discover          list detected Kiro projects, worktrees under them
  kms --activate <name>   take a catalog server into this project
  kms --deactivate <name> remove a server from this project
  kms --migrate           move global servers into the catalog and their
                          secrets into KMS__* environment variables
  kms --migrate --no-env  same, without touching the environment
  kms --rollback          return to the global mcp.json from the backup
  --dry-run               with --migrate, --rollback, --sync, --init-worktree
                          or --undo: show, change nothing

Keeping copies in line with the catalog:
  kms --sync              apply the catalog's definition to every copy of a
                          server it already holds: .kiro/settings/mcp.json and
                          .ai/mcp/mcp.json of projects, worktrees and Herd
                          clones. `disabled` is kept; a local autoApprove or
                          disabledTools the catalog never had is kept and
                          reported. Backups go to ~/.kiro/kms-backups.
  kms --sync --dry-run    list the files and show the diff, write nothing
  kms --check             same comparison, no write: exit 0 aligned, 1 diverged,
                          2 a file could not be read (--strict: warnings fail)
  --strict                with --check: warnings (mcp-remote env nobody uses,
                          literal secret) make it fail too
  --server <name>         limit --sync / --check to a server (repeatable)
  --include <list>        also cover agents and/or examples: Kiro agent specs
                          (~/.kiro/agents, ~/.kiro/mates/agents) and
                          mcp.json.example files. Without it they are reported
                          but left alone; in agents, env and headers are kept
  --no-worktrees          leave git worktrees out of --sync / --check
  --force                 with --sync: replace local customizations (and the
                          env of agents); with --init-worktree: replace the
                          file; with --undo: overwrite files edited since
  kms --init-worktree [<path>]
                          write a worktree's .kiro/settings/mcp.json from its
                          parent project (current directory by default)
  kms --undo [<id>]       restore the files of a --sync (the last one by
                          default); the undo is itself backed up
  kms --undo --list       list the kept runs (the last 20)
  kms --help              show this help
  kms --version           show the version";

const NO_HOME: &str = "Cannot locate the home directory.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("{err}\n\n{HELP}");
            return ExitCode::FAILURE;
        }
    };

    // `--check` reserves 1 for "a copy diverges", so its own failures use 2.
    let failure = if matches!(command, Command::Check { .. }) {
        2
    } else {
        1
    };

    let result = match command {
        Command::Sync { .. }
        | Command::Check { .. }
        | Command::InitWorktree { .. }
        | Command::Undo { .. } => run_sync_family(command),
        other => run(other).map(|output| Outcome {
            output: output.unwrap_or_default(),
            code: 0,
        }),
    };

    match result {
        Ok(Outcome { output, code }) => {
            if !output.is_empty() {
                println!("{output}");
            }
            ExitCode::from(code)
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(failure)
        }
    }
}

/// `--sync`, `--check`, `--init-worktree` and `--undo`.
fn run_sync_family(command: Command) -> Result<Outcome, String> {
    let env = SyncEnv::detect(&project_dir()?)?;
    match command {
        Command::Sync {
            scope,
            dry_run,
            force,
        } => syncing::sync(&env, &scope, dry_run, force),
        Command::Check { scope, strict } => syncing::check(&env, &scope, strict),
        Command::InitWorktree {
            path,
            dry_run,
            force,
        } => syncing::init_worktree(&env, path.as_deref(), dry_run, force),
        Command::Undo {
            id,
            list,
            dry_run,
            force,
        } => syncing::undo(&env, id.as_deref(), list, dry_run, force),
        _ => Err("Not a synchronization command.".to_string()),
    }
}

fn catalog_path() -> Result<PathBuf, String> {
    default_catalog_path().ok_or_else(|| NO_HOME.to_string())
}

fn global_path() -> Result<PathBuf, String> {
    default_global_path().ok_or_else(|| NO_HOME.to_string())
}

fn env_store() -> Result<Box<dyn EnvStore>, String> {
    default_env_store().ok_or_else(|| NO_HOME.to_string())
}

fn project_dir() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|err| format!("Cannot read the current directory: {err}"))
}

fn run(command: Command) -> Result<Option<String>, String> {
    match command {
        Command::Help => Ok(Some(HELP.to_string())),
        Command::Version => Ok(Some(format!("kms {}", env!("CARGO_PKG_VERSION")))),
        Command::List => commands::list(&catalog_path()?, &project_dir()?)
            .map(Some)
            .map_err(|e| e.to_string()),
        Command::Status => {
            let cwd = project_dir()?;
            let mut out = commands::status(&catalog_path()?, &cwd, &global_path()?)
                .map_err(|e| e.to_string())?;
            if let Ok(section) = SyncEnv::detect(&cwd).and_then(|env| syncing::status_section(&env))
            {
                if !section.is_empty() {
                    out.push_str("\n\n");
                    out.push_str(&section);
                }
            }
            Ok(Some(out))
        }
        Command::Activate { name } => {
            let message = commands::activate(&catalog_path()?, &project_dir()?, &name)
                .map_err(|e| e.to_string())?;
            if let Ok(catalog) = load_catalog(&catalog_path()?) {
                kms::core::state::record_best_effort(&catalog);
            }
            Ok(Some(message))
        }
        Command::Deactivate { name } => {
            commands::deactivate(&catalog_path()?, &project_dir()?, &name)
                .map(Some)
                .map_err(|e| e.to_string())
        }
        Command::Discover => {
            let config = default_config_path().ok_or(NO_HOME)?;
            commands::discover(&catalog_path()?, &config, &[], &project_dir()?)
                .map(Some)
                .map_err(|e| e.to_string())
        }
        Command::Migrate { dry_run, skip_env } => migration::run_migrate(
            &catalog_path()?,
            &global_path()?,
            MigrateOptions { dry_run, skip_env },
            env_store()?.as_mut(),
        )
        .map(Some)
        .map_err(|e| e.to_string()),
        Command::Rollback { dry_run } => {
            let cwd = project_dir()?;
            let config = default_config_path()
                .map(|path| load_config(&path))
                .unwrap_or_default();
            let roots = resolve_roots(&[], &config, &cwd);
            migration::run_rollback(
                &catalog_path()?,
                &global_path()?,
                &roots,
                config.max_depth,
                dry_run,
                env_store()?.as_mut(),
            )
            .map(Some)
            .map_err(|e| e.to_string())
        }
        Command::Sync { .. }
        | Command::Check { .. }
        | Command::InitWorktree { .. }
        | Command::Undo { .. } => Err("Not a plain command.".to_string()),
        Command::Tui => {
            let app = build_app()?;
            kms::ui::app::run(app).map_err(|e| e.to_string())?;
            Ok(None)
        }
        Command::Matrix => {
            let app = build_app()?.starting_on_matrix();
            kms::ui::app::run(app).map_err(|e| e.to_string())?;
            Ok(None)
        }
    }
}

/// Build the TUI app: the project view for the current directory plus, when
/// projects are discovered, the matrix view. Both are held together so their
/// edits persist across a Tab switch.
fn build_app() -> Result<App, String> {
    let catalog_path = catalog_path()?;
    let catalog = match load_catalog(&catalog_path) {
        Ok(catalog) => catalog,
        Err(err @ CatalogError::NotFound(_)) => {
            if !offer_migration(&catalog_path)? {
                return Err(err.to_string());
            }
            load_catalog(&catalog_path).map_err(|e| e.to_string())?
        }
        Err(err) => return Err(err.to_string()),
    };
    let cwd = project_dir()?;
    let project = ProjectView::load(cwd.clone(), catalog.clone()).map_err(|e| e.to_string())?;
    let matrix = build_matrix(&catalog, &cwd);
    Ok(App::new(project, matrix))
}

/// On first launch, with no catalog and servers still in the global, show what
/// `--migrate` would do and offer to run it. Only on a terminal: a script gets
/// the plain "catalog not found" error. Returns whether the migration ran.
fn offer_migration(catalog_path: &Path) -> Result<bool, String> {
    let global = global_path()?;
    if !std::io::stdin().is_terminal() || !migration::should_offer_migration(catalog_path, &global)
    {
        return Ok(false);
    }
    let mut env = env_store()?;
    let preview = migration::run_migrate(
        catalog_path,
        &global,
        MigrateOptions {
            dry_run: true,
            skip_env: false,
        },
        env.as_mut(),
    )
    .map_err(|e| e.to_string())?;

    println!("No catalog yet, and the global mcp.json still defines servers.");
    println!("This is what `kms --migrate` would do:\n\n{preview}\n");
    if !ask("Migrate now? [Y/n] ")? {
        return Ok(false);
    }
    let report = migration::run_migrate(
        catalog_path,
        &global,
        MigrateOptions::default(),
        env.as_mut(),
    )
    .map_err(|e| e.to_string())?;
    println!("\n{report}\n");
    ask("Press Enter to open kms. ")?;
    Ok(true)
}

fn ask(prompt: &str) -> Result<bool, String> {
    print!("{prompt}");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    Ok(migration::accepts(&answer))
}

/// Discover projects under the resolved roots and build the matrix view, or
/// `None` when nothing is found.
fn build_matrix(catalog: &Catalog, cwd: &Path) -> Option<MatrixView> {
    let config_path = default_config_path()?;
    let config = load_config(&config_path);
    let roots = resolve_roots(&[], &config, cwd);
    let projects = discover_projects(catalog, &roots, config.max_depth);
    if projects.is_empty() {
        return None;
    }
    let matrix = Matrix::build(catalog, &projects);
    let mut view = MatrixView::new(catalog.clone(), matrix);
    if let Ok(env) = SyncEnv::detect(cwd) {
        view.outside = syncing::matrix_summary(&env, catalog);
        view = view.with_init_paths(InitPaths {
            state: env.state_path.clone(),
            backups: env.backup_root.clone(),
        });
    }
    Some(view)
}
