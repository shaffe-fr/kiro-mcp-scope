//! Git worktrees: which directories belong to which project.
//!
//! A linked worktree is any checkout whose git common directory is the `.git`
//! of another project, wherever it lives (`<project>/.worktrees/<name>`, but
//! also a sibling such as `<project>__build`). The relation is read from git's
//! own metadata files — the `.git` pointer file, `commondir`, and the
//! `worktrees/*/gitdir` records — which is what `git worktree list` itself
//! reads. No process is spawned, so it behaves the same under Git Bash,
//! PowerShell and a plain Windows console, and an entry whose directory is gone
//! (prunable) is simply skipped.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// A project and the worktrees linked to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Family {
    /// The main worktree.
    pub parent: PathBuf,
    /// Linked worktrees, sorted, existing on disk.
    pub worktrees: Vec<PathBuf>,
}

/// Remove `.` and resolve `..` lexically.
fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A key that is equal for two spellings of the same location: symlinks and
/// 8.3 short names resolved when the path exists (git records real paths, while
/// a temp dir or a symlinked folder may be spelled otherwise), separators
/// unified, no `\\?\` prefix, no trailing slash, case-folded on Windows.
pub fn path_key(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = clean(&resolved).to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text);
    let text = text.trim_end_matches('/');
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.to_string()
    }
}

/// Whether two paths designate the same location.
pub fn same_path(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

fn resolve(base: &Path, target: &str) -> PathBuf {
    let target = Path::new(target.trim());
    if target.is_absolute() {
        clean(target)
    } else {
        clean(&base.join(target))
    }
}

/// `gitdir: <path>` pointer of a `.git` file.
fn read_gitdir_pointer(git_file: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(git_file).ok()?;
    let target = raw.lines().next()?.strip_prefix("gitdir:")?;
    Some(resolve(git_file.parent()?, target))
}

/// The git common directory of the checkout at `dir`, if it is one.
pub fn common_dir(dir: &Path) -> Option<PathBuf> {
    let dotgit = dir.join(".git");
    if dotgit.is_dir() {
        return Some(clean(&dotgit));
    }
    if !dotgit.is_file() {
        return None;
    }
    let gitdir = read_gitdir_pointer(&dotgit)?;
    match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(rel) => Some(resolve(&gitdir, &rel)),
        Err(_) => Some(gitdir),
    }
}

/// The main worktree for the checkout at `dir`: `dir` itself when it is the
/// main one. `None` outside a git checkout or for a bare repository.
pub fn main_worktree(dir: &Path) -> Option<PathBuf> {
    let common = common_dir(dir)?;
    if common.file_name()? != ".git" {
        return None;
    }
    let main = common.parent()?.to_path_buf();
    if same_path(&main, dir) {
        Some(dir.to_path_buf())
    } else {
        Some(main)
    }
}

/// Linked worktrees registered in the main worktree's `.git/worktrees`,
/// skipping entries whose directory no longer exists.
pub fn linked_worktrees(main: &Path) -> Vec<PathBuf> {
    let registry = main.join(".git").join("worktrees");
    let Ok(entries) = std::fs::read_dir(&registry) else {
        return Vec::new();
    };
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    for entry in entries.flatten() {
        let record = entry.path();
        let Ok(raw) = std::fs::read_to_string(record.join("gitdir")) else {
            continue;
        };
        let pointer = resolve(&record, raw.lines().next().unwrap_or(""));
        let Some(worktree) = pointer.parent() else {
            continue;
        };
        if worktree.join(".git").exists() {
            found.insert(path_key(worktree), worktree.to_path_buf());
        }
    }
    found.into_values().collect()
}

/// Group directories into families. Each candidate is attached to its main
/// worktree, which brings in all that worktree's linked checkouts; a directory
/// outside any git checkout is its own family.
pub fn families(candidates: &[PathBuf]) -> Vec<Family> {
    let mut by_key: BTreeMap<String, Family> = BTreeMap::new();
    for candidate in candidates {
        match main_worktree(candidate) {
            Some(main) => {
                by_key.entry(path_key(&main)).or_insert_with(|| Family {
                    worktrees: linked_worktrees(&main),
                    parent: main,
                });
            }
            None => {
                by_key.entry(path_key(candidate)).or_insert_with(|| Family {
                    parent: candidate.clone(),
                    worktrees: Vec::new(),
                });
            }
        }
    }
    by_key.into_values().collect()
}

#[cfg(test)]
pub(crate) mod testing {
    //! Hand-built git metadata, so tests need no git binary.
    use std::path::Path;

    /// A main worktree: just a `.git` directory.
    pub fn main_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git").join("worktrees")).unwrap();
    }

    /// A linked worktree of `main` named `name`, located at `at`.
    pub fn linked(main: &Path, at: &Path, name: &str) {
        let record = main.join(".git").join("worktrees").join(name);
        std::fs::create_dir_all(&record).unwrap();
        std::fs::create_dir_all(at).unwrap();
        std::fs::write(
            record.join("gitdir"),
            format!("{}\n", at.join(".git").to_string_lossy().replace('\\', "/")),
        )
        .unwrap();
        std::fs::write(record.join("commondir"), "../..\n").unwrap();
        std::fs::write(record.join("HEAD"), "ref: refs/heads/x\n").unwrap();
        std::fs::write(
            at.join(".git"),
            format!("gitdir: {}\n", record.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{linked, main_repo};
    use super::*;

    #[test]
    fn path_keys_ignore_spelling() {
        assert_eq!(
            path_key(Path::new("C:\\Users\\Karel\\Sites\\a\\")),
            path_key(Path::new("C:/Users/Karel/Sites/./a"))
        );
        assert!(same_path(Path::new("/a/b/../c"), Path::new("/a/c")));
    }

    #[test]
    fn a_worktree_is_attached_to_its_main_checkout_wherever_it_lives() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("proj");
        main_repo(&main);
        let inside = main.join(".worktrees").join("feat");
        let sibling = dir.path().join("proj__build");
        linked(&main, &inside, "feat");
        linked(&main, &sibling, "build");

        assert_eq!(main_worktree(&inside).unwrap(), main);
        assert_eq!(main_worktree(&sibling).unwrap(), main);
        assert!(same_path(&main_worktree(&main).unwrap(), &main));

        // Discovered as three standalone candidates, they collapse to one family.
        let found = families(&[main.clone(), sibling.clone(), inside.clone()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].parent, main);
        assert_eq!(found[0].worktrees.len(), 2);
        assert!(found[0].worktrees.iter().any(|w| same_path(w, &inside)));
        assert!(found[0].worktrees.iter().any(|w| same_path(w, &sibling)));
    }

    #[test]
    fn prunable_entries_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("proj");
        main_repo(&main);
        let gone = main.join(".worktrees").join("gone");
        linked(&main, &gone, "gone");
        std::fs::remove_dir_all(&gone).unwrap();
        assert!(linked_worktrees(&main).is_empty());
    }

    #[test]
    fn a_plain_directory_is_its_own_family() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(main_worktree(&plain), None);
        let found = families(std::slice::from_ref(&plain));
        assert_eq!(found.len(), 1);
        assert!(found[0].worktrees.is_empty());
    }

    #[test]
    fn relative_gitdir_pointers_are_followed() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("proj");
        main_repo(&main);
        let wt = dir.path().join("wt");
        let record = main.join(".git").join("worktrees").join("wt");
        std::fs::create_dir_all(&record).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(record.join("gitdir"), "../../../../wt/.git\n").unwrap();
        std::fs::write(record.join("commondir"), "../..\n").unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../proj/.git/worktrees/wt\n").unwrap();

        assert!(same_path(&main_worktree(&wt).unwrap(), &main));
        assert_eq!(linked_worktrees(&main).len(), 1);
    }

    #[test]
    fn agrees_with_real_git_when_it_is_installed() {
        let git_ok = std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !git_ok {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("repo with space");
        std::fs::create_dir_all(&main).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "x"]);
        let wt = dir.path().join("repo with space__wt");
        git(&["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "feat"]);

        let found = families(std::slice::from_ref(&wt));
        assert_eq!(found.len(), 1);
        assert!(same_path(&found[0].parent, &main));
        assert_eq!(found[0].worktrees.len(), 1);
        assert!(same_path(&found[0].worktrees[0], &wt));
    }
}
