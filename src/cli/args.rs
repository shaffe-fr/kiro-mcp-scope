//! Hand-rolled argument parsing.
//!
//! The surface is a handful of subcommands expressed as flags, plus a couple of
//! options. No `clap`: the shape is fixed and small, and dependency weight is
//! what the whole port is about.

use thiserror::Error;

use crate::core::copies::Include;

/// What a synchronization or a check looks at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scope {
    /// Servers to restrict to; empty means every catalog server.
    pub servers: Vec<String>,
    /// Opt-in categories (`--include agents,examples`).
    pub include: Include,
    /// `--no-worktrees`: leave git worktrees out.
    pub no_worktrees: bool,
}

/// One parsed invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// No arguments: launch the project TUI.
    Tui,
    /// `--matrix`: launch the matrix TUI.
    Matrix,
    /// `--list`: print catalog servers and their state in the current project.
    List,
    /// `--status`: like list, plus the global-drift warning.
    Status,
    /// `--discover`: print detected Kiro projects.
    Discover,
    /// `--activate <name>`: take a catalog server into the current project.
    Activate { name: String },
    /// `--deactivate <name>`: remove a server from the current project.
    Deactivate { name: String },
    /// `--migrate [--dry-run] [--no-env]`: migrate the global config into the
    /// catalog, secrets into `KMS__…` variables unless `--no-env`.
    Migrate { dry_run: bool, skip_env: bool },
    /// `--rollback [--dry-run]`: return to the global model from the backup.
    Rollback { dry_run: bool },
    /// `--sync`: apply the catalog's definitions to every copy.
    Sync {
        scope: Scope,
        dry_run: bool,
        force: bool,
    },
    /// `--check`: exit non-zero when a copy diverges from the catalog.
    Check { scope: Scope, strict: bool },
    /// `--init-worktree [<path>]`: write a worktree's config from its parent.
    InitWorktree {
        path: Option<String>,
        dry_run: bool,
        force: bool,
    },
    /// `--undo [<id>]` or `--undo --list`: go back to before a sync.
    Undo {
        id: Option<String>,
        list: bool,
        dry_run: bool,
        force: bool,
    },
    /// `--help`.
    Help,
    /// `--version`.
    Version,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("Unknown option: {0}")]
    Unknown(String),

    #[error("Option {option} requires a value.")]
    MissingValue { option: String },

    #[error("Unexpected argument: {0}")]
    Unexpected(String),

    #[error("{0} cannot be combined with other commands.")]
    Conflicting(String),

    #[error("{option} only applies to {applies_to}.")]
    NotApplicable {
        option: &'static str,
        applies_to: &'static str,
    },

    #[error("{option}: {message}")]
    Invalid {
        option: &'static str,
        message: String,
    },
}

/// Qualifiers collected while reading the arguments, checked against the
/// command once it is known.
#[derive(Default)]
struct Flags {
    dry_run: bool,
    no_env: bool,
    force: bool,
    strict: bool,
    list: bool,
    no_worktrees: bool,
    include: Option<Include>,
    servers: Vec<String>,
}

/// An optional value: taken only when it does not look like an option.
fn optional_value<'a>(
    iter: &mut std::iter::Peekable<std::slice::Iter<'a, String>>,
) -> Option<String> {
    match iter.peek() {
        Some(next) if !next.starts_with("--") => iter.next().cloned(),
        _ => None,
    }
}

fn required_value<'a>(
    iter: &mut std::iter::Peekable<std::slice::Iter<'a, String>>,
    option: &str,
) -> Result<String, ParseError> {
    iter.next()
        .cloned()
        .ok_or_else(|| ParseError::MissingValue {
            option: option.to_string(),
        })
}

fn reject(
    flag_set: bool,
    option: &'static str,
    applies_to: &'static str,
) -> Result<(), ParseError> {
    if flag_set {
        Err(ParseError::NotApplicable { option, applies_to })
    } else {
        Ok(())
    }
}

/// Parse arguments (excluding the program name).
pub fn parse_args(args: &[String]) -> Result<Command, ParseError> {
    let mut iter = args.iter().peekable();
    let mut command: Option<Command> = None;
    let mut flags = Flags::default();

    fn set(slot: &mut Option<Command>, candidate: Command, label: &str) -> Result<(), ParseError> {
        if slot.is_some() {
            return Err(ParseError::Conflicting(label.to_string()));
        }
        *slot = Some(candidate);
        Ok(())
    }

    let scope_default = || Scope::default();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            // `--list` is a command of its own and also qualifies `--undo`, so
            // it is resolved once everything has been read.
            "--list" => flags.list = true,
            "--status" => set(&mut command, Command::Status, "--status")?,
            "--discover" => set(&mut command, Command::Discover, "--discover")?,
            "--matrix" => set(&mut command, Command::Matrix, "--matrix")?,
            "--migrate" => set(
                &mut command,
                Command::Migrate {
                    dry_run: false,
                    skip_env: false,
                },
                "--migrate",
            )?,
            "--rollback" => set(
                &mut command,
                Command::Rollback { dry_run: false },
                "--rollback",
            )?,
            "--sync" => set(
                &mut command,
                Command::Sync {
                    scope: scope_default(),
                    dry_run: false,
                    force: false,
                },
                "--sync",
            )?,
            "--check" => set(
                &mut command,
                Command::Check {
                    scope: scope_default(),
                    strict: false,
                },
                "--check",
            )?,
            "--init-worktree" => {
                let path = optional_value(&mut iter);
                set(
                    &mut command,
                    Command::InitWorktree {
                        path,
                        dry_run: false,
                        force: false,
                    },
                    "--init-worktree",
                )?;
            }
            "--undo" => {
                let id = optional_value(&mut iter);
                set(
                    &mut command,
                    Command::Undo {
                        id,
                        list: false,
                        dry_run: false,
                        force: false,
                    },
                    "--undo",
                )?;
            }
            "--dry-run" => flags.dry_run = true,
            "--no-env" => flags.no_env = true,
            "--force" => flags.force = true,
            "--strict" => flags.strict = true,
            "--no-worktrees" => flags.no_worktrees = true,
            "--include" => {
                let value = required_value(&mut iter, "--include")?;
                let parsed = Include::parse(&value).map_err(|message| ParseError::Invalid {
                    option: "--include",
                    message,
                })?;
                let merged = flags.include.unwrap_or_default();
                flags.include = Some(Include {
                    agents: merged.agents || parsed.agents,
                    examples: merged.examples || parsed.examples,
                });
            }
            "--server" => flags.servers.push(required_value(&mut iter, "--server")?),
            "--help" | "-h" => set(&mut command, Command::Help, "--help")?,
            "--version" | "-V" => set(&mut command, Command::Version, "--version")?,
            "--activate" => {
                let name = required_value(&mut iter, "--activate")?;
                set(&mut command, Command::Activate { name }, "--activate")?;
            }
            "--deactivate" => {
                let name = required_value(&mut iter, "--deactivate")?;
                set(&mut command, Command::Deactivate { name }, "--deactivate")?;
            }
            other if other.starts_with('-') => {
                return Err(ParseError::Unknown(other.to_string()));
            }
            other => return Err(ParseError::Unexpected(other.to_string())),
        }
    }

    finish(command, flags)
}

/// Check every qualifier against the command and fill it in.
fn finish(command: Option<Command>, flags: Flags) -> Result<Command, ParseError> {
    const DRY: &str = "--migrate, --rollback, --sync, --init-worktree and --undo";

    let command = match command {
        None if flags.list => Command::List,
        None => Command::Tui,
        Some(Command::Undo { id, .. }) if flags.list => {
            if id.is_some() {
                return Err(ParseError::Conflicting("--list".to_string()));
            }
            Command::Undo {
                id,
                list: true,
                dry_run: false,
                force: false,
            }
        }
        Some(_) if flags.list => return Err(ParseError::Conflicting("--list".to_string())),
        Some(command) => command,
    };

    let takes_scope = matches!(command, Command::Sync { .. } | Command::Check { .. });
    let takes_dry = matches!(
        command,
        Command::Migrate { .. }
            | Command::Rollback { .. }
            | Command::Sync { .. }
            | Command::InitWorktree { .. }
            | Command::Undo { .. }
    );
    let takes_force = matches!(
        command,
        Command::Sync { .. } | Command::InitWorktree { .. } | Command::Undo { .. }
    );

    reject(flags.dry_run && !takes_dry, "--dry-run", DRY)?;
    reject(
        flags.no_env && !matches!(command, Command::Migrate { .. }),
        "--no-env",
        "--migrate",
    )?;
    reject(
        flags.force && !takes_force,
        "--force",
        "--sync, --init-worktree and --undo",
    )?;
    reject(
        flags.strict && !matches!(command, Command::Check { .. }),
        "--strict",
        "--check",
    )?;
    reject(
        flags.no_worktrees && !takes_scope,
        "--no-worktrees",
        "--sync and --check",
    )?;
    reject(
        flags.include.is_some() && !takes_scope,
        "--include",
        "--sync and --check",
    )?;
    reject(
        !flags.servers.is_empty() && !takes_scope,
        "--server",
        "--sync and --check",
    )?;
    let scope = Scope {
        servers: flags.servers,
        include: flags.include.unwrap_or_default(),
        no_worktrees: flags.no_worktrees,
    };

    Ok(match command {
        Command::Migrate { .. } => Command::Migrate {
            dry_run: flags.dry_run,
            skip_env: flags.no_env,
        },
        Command::Rollback { .. } => Command::Rollback {
            dry_run: flags.dry_run,
        },
        Command::Sync { .. } => Command::Sync {
            scope,
            dry_run: flags.dry_run,
            force: flags.force,
        },
        Command::Check { .. } => Command::Check {
            scope,
            strict: flags.strict,
        },
        Command::InitWorktree { path, .. } => Command::InitWorktree {
            path,
            dry_run: flags.dry_run,
            force: flags.force,
        },
        Command::Undo { id, list, .. } => Command::Undo {
            id,
            list,
            dry_run: flags.dry_run,
            force: flags.force,
        },
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, ParseError> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_args(&owned)
    }

    #[test]
    fn no_args_is_tui() {
        assert_eq!(parse(&[]).unwrap(), Command::Tui);
    }

    #[test]
    fn recognizes_simple_flags() {
        assert_eq!(parse(&["--list"]).unwrap(), Command::List);
        assert_eq!(parse(&["--status"]).unwrap(), Command::Status);
        assert_eq!(parse(&["--discover"]).unwrap(), Command::Discover);
        assert_eq!(parse(&["--matrix"]).unwrap(), Command::Matrix);
    }

    #[test]
    fn activate_takes_a_name() {
        assert_eq!(
            parse(&["--activate", "my-server"]).unwrap(),
            Command::Activate {
                name: "my-server".into()
            }
        );
    }

    #[test]
    fn activate_without_name_errors() {
        assert_eq!(
            parse(&["--activate"]).unwrap_err(),
            ParseError::MissingValue {
                option: "--activate".into()
            }
        );
    }

    #[test]
    fn migrate_options() {
        assert_eq!(
            parse(&["--migrate"]).unwrap(),
            Command::Migrate {
                dry_run: false,
                skip_env: false
            }
        );
        assert_eq!(
            parse(&["--migrate", "--dry-run", "--no-env"]).unwrap(),
            Command::Migrate {
                dry_run: true,
                skip_env: true
            }
        );
    }

    #[test]
    fn rollback_options() {
        assert_eq!(
            parse(&["--rollback"]).unwrap(),
            Command::Rollback { dry_run: false }
        );
        assert_eq!(
            parse(&["--dry-run", "--rollback"]).unwrap(),
            Command::Rollback { dry_run: true }
        );
    }

    #[test]
    fn qualifiers_outside_their_command_error() {
        assert!(matches!(
            parse(&["--list", "--dry-run"]).unwrap_err(),
            ParseError::NotApplicable {
                option: "--dry-run",
                ..
            }
        ));
        assert!(matches!(
            parse(&["--rollback", "--no-env"]).unwrap_err(),
            ParseError::NotApplicable {
                option: "--no-env",
                ..
            }
        ));
        assert!(matches!(
            parse(&["--no-env"]).unwrap_err(),
            ParseError::NotApplicable {
                option: "--no-env",
                ..
            }
        ));
    }

    #[test]
    fn conflicting_commands_error() {
        assert!(matches!(
            parse(&["--list", "--status"]).unwrap_err(),
            ParseError::Conflicting(_)
        ));
    }

    #[test]
    fn unknown_flag_errors() {
        assert!(matches!(
            parse(&["--nope"]).unwrap_err(),
            ParseError::Unknown(_)
        ));
    }

    #[test]
    fn sync_collects_its_scope() {
        assert_eq!(
            parse(&[
                "--sync",
                "--server",
                "a",
                "--server",
                "b",
                "--include",
                "agents",
                "--include",
                "examples",
                "--no-worktrees",
                "--dry-run",
                "--force"
            ])
            .unwrap(),
            Command::Sync {
                scope: Scope {
                    servers: vec!["a".into(), "b".into()],
                    include: Include {
                        agents: true,
                        examples: true
                    },
                    no_worktrees: true,
                },
                dry_run: true,
                force: true,
            }
        );
        assert_eq!(
            parse(&["--sync"]).unwrap(),
            Command::Sync {
                scope: Scope::default(),
                dry_run: false,
                force: false
            }
        );
    }

    #[test]
    fn include_accepts_a_comma_list_and_rejects_unknown_values() {
        let Command::Check { scope, .. } =
            parse(&["--check", "--include", "agents,examples"]).unwrap()
        else {
            panic!("expected --check");
        };
        assert!(scope.include.agents && scope.include.examples);
        assert!(matches!(
            parse(&["--check", "--include", "nope"]).unwrap_err(),
            ParseError::Invalid {
                option: "--include",
                ..
            }
        ));
        assert!(matches!(
            parse(&["--check", "--include"]).unwrap_err(),
            ParseError::MissingValue { .. }
        ));
    }

    #[test]
    fn check_takes_strict_but_not_dry_run() {
        assert_eq!(
            parse(&["--check", "--strict"]).unwrap(),
            Command::Check {
                scope: Scope::default(),
                strict: true
            }
        );
        assert!(matches!(
            parse(&["--check", "--dry-run"]).unwrap_err(),
            ParseError::NotApplicable {
                option: "--dry-run",
                ..
            }
        ));
        assert!(matches!(
            parse(&["--sync", "--strict"]).unwrap_err(),
            ParseError::NotApplicable {
                option: "--strict",
                ..
            }
        ));
    }

    #[test]
    fn optional_values_are_taken_only_when_they_are_not_options() {
        assert_eq!(
            parse(&["--init-worktree"]).unwrap(),
            Command::InitWorktree {
                path: None,
                dry_run: false,
                force: false
            }
        );
        assert_eq!(
            parse(&["--init-worktree", "--dry-run"]).unwrap(),
            Command::InitWorktree {
                path: None,
                dry_run: true,
                force: false
            }
        );
        assert_eq!(
            parse(&["--undo", "--force"]).unwrap(),
            Command::Undo {
                id: None,
                list: false,
                dry_run: false,
                force: true
            }
        );
        assert_eq!(
            parse(&["--undo", "20261120-104400"]).unwrap(),
            Command::Undo {
                id: Some("20261120-104400".into()),
                list: false,
                dry_run: false,
                force: false
            }
        );
    }

    #[test]
    fn a_windows_path_with_spaces_is_one_value() {
        let path = r"C:\Users\Karel\My Sites\proj with space\.worktrees\feat one";
        assert_eq!(
            parse(&["--init-worktree", path, "--force"]).unwrap(),
            Command::InitWorktree {
                path: Some(path.to_string()),
                dry_run: false,
                force: true
            }
        );
    }

    #[test]
    fn list_means_two_things_depending_on_the_command() {
        assert_eq!(parse(&["--list"]).unwrap(), Command::List);
        assert_eq!(
            parse(&["--undo", "--list"]).unwrap(),
            Command::Undo {
                id: None,
                list: true,
                dry_run: false,
                force: false
            }
        );
        assert_eq!(
            parse(&["--list", "--undo"]).unwrap(),
            parse(&["--undo", "--list"]).unwrap()
        );
        assert!(matches!(
            parse(&["--sync", "--list"]).unwrap_err(),
            ParseError::Conflicting(_)
        ));
    }

    #[test]
    fn scope_options_only_apply_to_sync_and_check() {
        for args in [
            &["--list", "--no-worktrees"][..],
            &["--status", "--include", "agents"],
            &["--undo", "--server", "a"],
        ] {
            assert!(
                matches!(parse(args).unwrap_err(), ParseError::NotApplicable { .. }),
                "{args:?}"
            );
        }
        assert!(matches!(
            parse(&["--force"]).unwrap_err(),
            ParseError::NotApplicable {
                option: "--force",
                ..
            }
        ));
    }
}
