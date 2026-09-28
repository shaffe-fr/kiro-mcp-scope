//! Persisting variables in the user's environment, per platform.
//!
//! Windows keeps a user-scope environment in the registry. It is written through
//! PowerShell's `[Environment]::SetEnvironmentVariable(…, 'User')`: `setx`
//! cannot delete a variable and truncates values at 1024 characters. The name
//! and value reach the PowerShell process through its environment, never its
//! command line, so the secret does not show in the process list.
//!
//! Other platforms have no user-scope store. Variables go to a dedicated file,
//! `~/.kiro/kms-env.sh`, readable by the user only, which the shell profile
//! sources. kms never edits the profile itself: that file belongs to the user.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Where kms persists the variables a migration defines.
pub trait EnvStore {
    fn set(&mut self, name: &str, value: &str) -> io::Result<()>;
    /// Removing a variable that does not exist is not an error.
    fn remove(&mut self, name: &str) -> io::Result<()>;
    /// Where the variables live and what makes new processes see them.
    fn activation_hint(&self) -> String;
}

/// The store for the current platform.
pub fn default_env_store() -> Option<Box<dyn EnvStore>> {
    if cfg!(windows) {
        return Some(Box::new(WindowsUserEnv));
    }
    let home = dirs::home_dir()?;
    Some(Box::new(ShellFileEnv::new(
        home.join(".kiro").join("kms-env.sh"),
    )))
}

/// The user-scope environment of Windows.
pub struct WindowsUserEnv;

/// Double leading underscore: no migrated variable can take these names, since
/// migrated names always start with `KMS__`.
const NAME_TRANSPORT: &str = "__KMS_STORE_NAME";
const VALUE_TRANSPORT: &str = "__KMS_STORE_VALUE";

impl WindowsUserEnv {
    fn run(script: &str, name: &str, value: Option<&str>) -> io::Result<()> {
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env(NAME_TRANSPORT, name)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(value) = value {
            command.env(VALUE_TRANSPORT, value);
        }
        let output = command.output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "PowerShell exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

impl EnvStore for WindowsUserEnv {
    fn set(&mut self, name: &str, value: &str) -> io::Result<()> {
        Self::run(
            "[Environment]::SetEnvironmentVariable($env:__KMS_STORE_NAME, $env:__KMS_STORE_VALUE, 'User')",
            name,
            Some(value),
        )
    }

    fn remove(&mut self, name: &str) -> io::Result<()> {
        Self::run(
            "[Environment]::SetEnvironmentVariable($env:__KMS_STORE_NAME, $null, 'User')",
            name,
            None,
        )
    }

    fn activation_hint(&self) -> String {
        "Stored as Windows user environment variables. Restart Kiro (and any \
         terminal it is launched from) so it sees them."
            .to_string()
    }
}

/// A shell file of `export NAME='value'` lines, sourced by the user's profile.
pub struct ShellFileEnv {
    path: PathBuf,
}

const SHELL_FILE_HEADER: &str =
    "# Managed by kms. Holds secrets: keep it private and out of version control.\n\
# Source it from your shell profile:  . \"$HOME/.kiro/kms-env.sh\"\n";

impl ShellFileEnv {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn exports(&self) -> io::Result<Vec<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => Ok(raw
                .lines()
                .filter(|line| line.starts_with("export "))
                .map(str::to_string)
                .collect()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(err),
        }
    }

    fn write_exports(&self, exports: &[String]) -> io::Result<()> {
        if exports.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
                _ => Ok(()),
            };
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut content = String::from(SHELL_FILE_HEADER);
        for line in exports {
            content.push_str(line);
            content.push('\n');
        }
        write_private(&self.path, &content)
    }
}

fn export_prefix(name: &str) -> String {
    format!("export {name}=")
}

/// Single-quote a value for POSIX shells: nothing inside is interpreted, and an
/// embedded `'` is closed, escaped, and reopened.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Write a file readable by its owner only. The mode is set at creation, so the
/// secret never sits in a world-readable file, and re-applied for a file that
/// existed with looser permissions.
fn write_private(path: &Path, content: &str) -> io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(content.as_bytes())
}

impl EnvStore for ShellFileEnv {
    fn set(&mut self, name: &str, value: &str) -> io::Result<()> {
        let prefix = export_prefix(name);
        let mut exports: Vec<String> = self
            .exports()?
            .into_iter()
            .filter(|line| !line.starts_with(&prefix))
            .collect();
        exports.push(format!("{prefix}{}", shell_quote(value)));
        self.write_exports(&exports)
    }

    fn remove(&mut self, name: &str) -> io::Result<()> {
        let prefix = export_prefix(name);
        let exports: Vec<String> = self
            .exports()?
            .into_iter()
            .filter(|line| !line.starts_with(&prefix))
            .collect();
        self.write_exports(&exports)
    }

    fn activation_hint(&self) -> String {
        format!(
            "Stored in {}. Add `. \"{}\"` to your shell profile, then restart Kiro.",
            self.path.display(),
            self.path.display()
        )
    }
}

/// In-memory store for tests: never touches the real environment.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MemoryEnv {
    pub vars: std::collections::BTreeMap<String, String>,
}

#[cfg(test)]
impl EnvStore for MemoryEnv {
    fn set(&mut self, name: &str, value: &str) -> io::Result<()> {
        self.vars.insert(name.to_string(), value.to_string());
        Ok(())
    }

    fn remove(&mut self, name: &str) -> io::Result<()> {
        self.vars.remove(name);
        Ok(())
    }

    fn activation_hint(&self) -> String {
        "Stored in memory.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> ShellFileEnv {
        ShellFileEnv::new(dir.join("kms-env.sh"))
    }

    #[test]
    fn set_writes_an_export_line_under_a_header() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = store(dir.path());
        env.set("KMS_TOKEN", "abc").unwrap();
        let content = std::fs::read_to_string(env.path()).unwrap();
        assert!(content.starts_with("# Managed by kms."));
        assert!(content.contains("export KMS_TOKEN='abc'\n"));
    }

    #[test]
    fn set_replaces_an_existing_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = store(dir.path());
        env.set("KMS_TOKEN", "first-value").unwrap();
        env.set("KMS_TOKEN", "second-value").unwrap();
        let content = std::fs::read_to_string(env.path()).unwrap();
        assert!(content.contains("export KMS_TOKEN='second-value'"));
        assert!(!content.contains("first-value"));
        assert_eq!(content.matches("export KMS_TOKEN=").count(), 1);
    }

    #[test]
    fn set_does_not_confuse_a_name_with_its_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = store(dir.path());
        env.set("KMS_TOKEN_2", "two").unwrap();
        env.set("KMS_TOKEN", "one").unwrap();
        let content = std::fs::read_to_string(env.path()).unwrap();
        assert!(content.contains("export KMS_TOKEN_2='two'"));
        assert!(content.contains("export KMS_TOKEN='one'"));
    }

    #[test]
    fn single_quotes_are_escaped() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$HOME `x`"), "'$HOME `x`'");
    }

    #[test]
    fn remove_drops_the_line_and_the_file_once_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = store(dir.path());
        env.set("KMS_A", "1").unwrap();
        env.set("KMS_B", "2").unwrap();

        env.remove("KMS_A").unwrap();
        let content = std::fs::read_to_string(env.path()).unwrap();
        assert!(!content.contains("KMS_A"));
        assert!(content.contains("export KMS_B='2'"));

        env.remove("KMS_B").unwrap();
        assert!(!env.path().exists(), "no variable left, no file left");
    }

    #[test]
    fn removing_an_absent_variable_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = store(dir.path());
        env.remove("KMS_NOPE").unwrap();
        assert!(!env.path().exists());
    }

    /// Touches the real Windows user environment, with a throwaway name it
    /// removes again. Run by hand: `cargo test -- --ignored`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn windows_store_sets_and_removes_a_user_variable() {
        fn read_back(name: &str) -> String {
            let output = Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &format!("[Environment]::GetEnvironmentVariable('{name}', 'User')"),
                ])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        let name = "KMS_SMOKE_TEST";
        let value = "it's a \"quoted\" $value & more";
        let mut env = WindowsUserEnv;

        env.set(name, value).unwrap();
        assert_eq!(read_back(name), value);

        env.remove(name).unwrap();
        assert_eq!(read_back(name), "");
    }

    #[cfg(unix)]
    #[test]
    fn file_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut env = store(dir.path());
        env.set("KMS_TOKEN", "abc").unwrap();
        let mode = std::fs::metadata(env.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
