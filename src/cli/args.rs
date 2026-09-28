//! Hand-rolled argument parsing.
//!
//! The surface is a handful of subcommands expressed as flags, plus a couple of
//! options. No `clap`: the shape is fixed and small, and dependency weight is
//! what the whole port is about.

use thiserror::Error;

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
}

/// Parse arguments (excluding the program name).
pub fn parse_args(args: &[String]) -> Result<Command, ParseError> {
    let mut iter = args.iter().peekable();
    let mut command: Option<Command> = None;
    let mut dry_run = false;
    let mut no_env = false;

    fn set(slot: &mut Option<Command>, candidate: Command, label: &str) -> Result<(), ParseError> {
        if slot.is_some() {
            return Err(ParseError::Conflicting(label.to_string()));
        }
        *slot = Some(candidate);
        Ok(())
    }

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--list" => set(&mut command, Command::List, "--list")?,
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
            "--dry-run" => dry_run = true,
            "--no-env" => no_env = true,
            "--help" | "-h" => set(&mut command, Command::Help, "--help")?,
            "--version" | "-V" => set(&mut command, Command::Version, "--version")?,
            "--activate" => {
                let name = iter
                    .next()
                    .ok_or_else(|| ParseError::MissingValue {
                        option: "--activate".to_string(),
                    })?
                    .clone();
                set(&mut command, Command::Activate { name }, "--activate")?;
            }
            "--deactivate" => {
                let name = iter
                    .next()
                    .ok_or_else(|| ParseError::MissingValue {
                        option: "--deactivate".to_string(),
                    })?
                    .clone();
                set(&mut command, Command::Deactivate { name }, "--deactivate")?;
            }
            other if other.starts_with('-') => {
                return Err(ParseError::Unknown(other.to_string()));
            }
            other => return Err(ParseError::Unexpected(other.to_string())),
        }
    }

    match command.unwrap_or(Command::Tui) {
        Command::Migrate { .. } => Ok(Command::Migrate {
            dry_run,
            skip_env: no_env,
        }),
        Command::Rollback { .. } if no_env => Err(ParseError::NotApplicable {
            option: "--no-env",
            applies_to: "--migrate",
        }),
        Command::Rollback { .. } => Ok(Command::Rollback { dry_run }),
        _ if dry_run => Err(ParseError::NotApplicable {
            option: "--dry-run",
            applies_to: "--migrate and --rollback",
        }),
        _ if no_env => Err(ParseError::NotApplicable {
            option: "--no-env",
            applies_to: "--migrate",
        }),
        other => Ok(other),
    }
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
}
