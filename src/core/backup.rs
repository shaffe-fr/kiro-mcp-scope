//! Backups for every write kms makes outside the TUI's own project file, and
//! the `--undo` that goes back.
//!
//! Each run that writes gets a folder `~/.kiro/kms-backups/<id>/` holding a
//! copy of every file it replaced (`0001.bak`, ...) and a `manifest.json`
//! saying which path each copy belongs to and the hash of what the run wrote.
//! Keeping the copies out of the projects leaves git status clean; the hash
//! lets `--undo` refuse to overwrite a file edited since.
//!
//! A file whose final content equals what is on disk is never written, never
//! backed up and never listed: Kiro reconnects MCP servers whenever one of these
//! files changes, so a no-op rewrite is not harmless.
//!
//! Backups of agent files may hold secrets in clear, which is why only the
//! last [`KEEP_RUNS`] runs are kept.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::jsonedit::{render_document, write_atomic, Layout};

/// Runs kept by [`BackupStore::purge`].
pub const KEEP_RUNS: usize = 20;

#[derive(Debug, Error)]
pub enum BackupError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("No backup run to undo.")]
    NoRuns,

    #[error("Unknown backup run \"{0}\". Use --undo --list to see them.")]
    UnknownRun(String),

    #[error("Invalid backup run id \"{0}\".")]
    InvalidId(String),
}

/// `~/.kiro/kms-backups`.
pub fn default_backup_root() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".kiro").join("kms-backups"))
}

/// FNV-1a 64 bits, hex. Not cryptographic: it only detects "edited since".
pub fn content_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    /// Backup file name inside the run folder; `None` when the run created the
    /// file, so undoing means deleting it.
    pub backup: Option<String>,
    /// Hash of what the run left at `path`; `None` when the run deleted it.
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    /// `sync`, `init-worktree` or `undo`.
    pub kind: String,
    pub created: String,
    pub entries: Vec<Entry>,
}

/// UTC time as `(year, month, day, hour, minute, second)`.
fn utc_now() -> (i64, u32, u32, u32, u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3_600) as u32,
        (rem % 3_600 / 60) as u32,
        (rem % 60) as u32,
    )
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The backup folder and everything in it.
#[derive(Debug, Clone)]
pub struct BackupStore {
    root: PathBuf,
}

impl BackupStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Start a run. Nothing is created on disk until the first file is saved.
    pub fn begin(&self, kind: &str) -> Recorder {
        let (y, mo, d, h, mi, s) = utc_now();
        let base = format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}");
        let mut id = base.clone();
        let mut n = 2;
        while self.root.join(&id).exists() {
            id = format!("{base}-{n}");
            n += 1;
        }
        Recorder {
            dir: self.root.join(&id),
            manifest: Manifest {
                id,
                kind: kind.to_string(),
                created: format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"),
                entries: Vec::new(),
            },
        }
    }

    fn read_manifest(&self, id: &str) -> Option<Manifest> {
        let raw = std::fs::read_to_string(self.root.join(id).join("manifest.json")).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Every run, newest first.
    pub fn runs(&self) -> Vec<Manifest> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|id| valid_id(id))
            .collect();
        ids.sort();
        ids.reverse();
        ids.iter().filter_map(|id| self.read_manifest(id)).collect()
    }

    /// Keep the newest `keep` runs, delete the rest. Only folders that carry a
    /// manifest are ever deleted.
    pub fn purge(&self, keep: usize) -> std::io::Result<usize> {
        let mut removed = 0;
        for run in self.runs().into_iter().skip(keep) {
            std::fs::remove_dir_all(self.root.join(&run.id))?;
            removed += 1;
        }
        Ok(removed)
    }

    /// Undo a run (the latest when `id` is `None`): put back each file as it
    /// was. Files edited since the run, or whose folder is gone, are skipped
    /// and reported. What `--undo` overwrites is itself backed up as a new run,
    /// so the undo can be undone.
    pub fn undo(
        &self,
        id: Option<&str>,
        force: bool,
        dry_run: bool,
    ) -> Result<UndoReport, BackupError> {
        let manifest = match id {
            Some(id) => {
                if !valid_id(id) {
                    return Err(BackupError::InvalidId(id.to_string()));
                }
                self.read_manifest(id)
                    .ok_or_else(|| BackupError::UnknownRun(id.to_string()))?
            }
            None => self.runs().into_iter().next().ok_or(BackupError::NoRuns)?,
        };

        let mut recorder = self.begin("undo");
        let mut report = UndoReport {
            run: manifest.id.clone(),
            restored: Vec::new(),
            skipped: Vec::new(),
            undo_run: None,
        };

        for entry in manifest.entries.iter().rev() {
            let parent_gone = entry.path.parent().is_some_and(|p| !p.exists());
            if parent_gone {
                report.skipped.push((
                    entry.path.clone(),
                    "its folder no longer exists".to_string(),
                ));
                continue;
            }

            let current = std::fs::read_to_string(&entry.path).ok();
            let expected = entry.after.as_deref();
            let unchanged = match (&current, expected) {
                (Some(text), Some(hash)) => content_hash(text) == hash,
                (None, None) => true,
                _ => false,
            };
            let missing_but_restorable = current.is_none() && entry.backup.is_some();
            if !unchanged && !missing_but_restorable && !force {
                report
                    .skipped
                    .push((entry.path.clone(), "modified since that run".to_string()));
                continue;
            }

            match &entry.backup {
                Some(name) => {
                    let saved = std::fs::read_to_string(self.root.join(&manifest.id).join(name))?;
                    if !dry_run {
                        recorder.write(&entry.path, &saved)?;
                    }
                }
                None => {
                    if !dry_run {
                        recorder.delete(&entry.path)?;
                    }
                }
            }
            report.restored.push(entry.path.clone());
        }

        if !dry_run {
            report.undo_run = recorder.finish()?;
        }
        Ok(report)
    }
}

/// What an undo did.
#[derive(Debug, Clone)]
pub struct UndoReport {
    pub run: String,
    pub restored: Vec<PathBuf>,
    pub skipped: Vec<(PathBuf, String)>,
    /// The run that backs up what the undo overwrote.
    pub undo_run: Option<String>,
}

/// One run in progress.
#[derive(Debug)]
pub struct Recorder {
    dir: PathBuf,
    manifest: Manifest,
}

impl Recorder {
    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    fn next_backup_name(&self) -> String {
        format!("{:04}.bak", self.manifest.entries.len() + 1)
    }

    /// Replace `path` with `text`, saving the previous content first. Does
    /// nothing when the content is already identical. Returns whether the file
    /// was written.
    pub fn write(&mut self, path: &Path, text: &str) -> std::io::Result<bool> {
        let current = match std::fs::read_to_string(path) {
            Ok(current) => Some(current),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        };
        if current.as_deref() == Some(text) {
            return Ok(false);
        }
        let backup = match &current {
            Some(previous) => {
                let name = self.next_backup_name();
                write_atomic(&self.dir.join(&name), previous)?;
                Some(name)
            }
            None => None,
        };
        // The manifest is written before the file changes: an interrupted run
        // still knows how to go back.
        self.manifest.entries.push(Entry {
            path: path.to_path_buf(),
            backup,
            after: Some(content_hash(text)),
        });
        self.save_manifest()?;
        write_atomic(path, text)?;
        Ok(true)
    }

    /// Delete `path`, saving its content first. Missing file: nothing.
    pub fn delete(&mut self, path: &Path) -> std::io::Result<bool> {
        let previous = match std::fs::read_to_string(path) {
            Ok(previous) => previous,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        let name = self.next_backup_name();
        write_atomic(&self.dir.join(&name), &previous)?;
        self.manifest.entries.push(Entry {
            path: path.to_path_buf(),
            backup: Some(name),
            after: None,
        });
        self.save_manifest()?;
        std::fs::remove_file(path)?;
        Ok(true)
    }

    fn save_manifest(&self) -> std::io::Result<()> {
        let value: Value =
            serde_json::to_value(&self.manifest).expect("a manifest always serializes");
        write_atomic(
            &self.dir.join("manifest.json"),
            &render_document(&value, &Layout::default()),
        )
    }

    /// End the run. `None` when it changed nothing.
    pub fn finish(self) -> std::io::Result<Option<String>> {
        if self.manifest.entries.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.manifest.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        store: BackupStore,
        work: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        Fixture {
            store: BackupStore::new(dir.path().join("backups")),
            work,
            _dir: dir,
        }
    }

    #[test]
    fn writes_back_up_the_previous_content_and_undo_restores_it() {
        let f = fixture();
        let file = f.work.join("mcp.json");
        std::fs::write(&file, "old").unwrap();

        let mut run = f.store.begin("sync");
        assert!(run.write(&file, "new").unwrap());
        let id = run.finish().unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");

        let report = f.store.undo(Some(&id), false, false).unwrap();
        assert_eq!(report.restored, vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old");
        assert!(report.undo_run.is_some(), "the undo is itself backed up");
    }

    #[test]
    fn identical_content_is_neither_written_nor_recorded() {
        let f = fixture();
        let file = f.work.join("a.json");
        std::fs::write(&file, "same").unwrap();
        let mut run = f.store.begin("sync");
        assert!(!run.write(&file, "same").unwrap());
        assert_eq!(run.finish().unwrap(), None);
        assert!(!f.store.root.exists(), "no run folder for a no-op");
    }

    #[test]
    fn undo_refuses_a_file_edited_since_unless_forced() {
        let f = fixture();
        let file = f.work.join("a.json");
        std::fs::write(&file, "v1").unwrap();
        let mut run = f.store.begin("sync");
        run.write(&file, "v2").unwrap();
        let id = run.finish().unwrap().unwrap();
        std::fs::write(&file, "edited by hand").unwrap();

        let report = f.store.undo(Some(&id), false, false).unwrap();
        assert!(report.restored.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited by hand");

        let forced = f.store.undo(Some(&id), true, false).unwrap();
        assert_eq!(forced.restored.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
    }

    #[test]
    fn undo_skips_files_whose_folder_vanished() {
        let f = fixture();
        let sub = f.work.join("wt");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("mcp.json");
        std::fs::write(&file, "v1").unwrap();
        let mut run = f.store.begin("sync");
        run.write(&file, "v2").unwrap();
        let id = run.finish().unwrap().unwrap();
        std::fs::remove_dir_all(&sub).unwrap();

        let report = f.store.undo(Some(&id), false, false).unwrap();
        assert!(report.restored.is_empty());
        assert!(report.skipped[0].1.contains("no longer exists"));
    }

    #[test]
    fn a_created_file_is_deleted_by_undo_and_the_undo_can_be_undone() {
        let f = fixture();
        let file = f.work.join("new.json");
        let mut run = f.store.begin("init-worktree");
        run.write(&file, "created").unwrap();
        let id = run.finish().unwrap().unwrap();

        let report = f.store.undo(Some(&id), false, false).unwrap();
        assert!(!file.exists());
        let undo_id = report.undo_run.unwrap();

        f.store.undo(Some(&undo_id), false, false).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "created");
    }

    #[test]
    fn dry_run_changes_nothing() {
        let f = fixture();
        let file = f.work.join("a.json");
        std::fs::write(&file, "v1").unwrap();
        let mut run = f.store.begin("sync");
        run.write(&file, "v2").unwrap();
        let id = run.finish().unwrap().unwrap();

        let report = f.store.undo(Some(&id), false, true).unwrap();
        assert_eq!(report.restored.len(), 1);
        assert!(report.undo_run.is_none());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");
    }

    #[test]
    fn purge_keeps_the_newest_runs() {
        let f = fixture();
        for n in 0..5 {
            let file = f.work.join(format!("f{n}.json"));
            std::fs::write(&file, "a").unwrap();
            let mut run = f.store.begin("sync");
            run.write(&file, "b").unwrap();
            run.finish().unwrap();
        }
        assert_eq!(f.store.runs().len(), 5);
        assert_eq!(f.store.purge(2).unwrap(), 3);
        assert_eq!(f.store.runs().len(), 2);
    }

    #[test]
    fn ids_are_validated_before_touching_the_disk() {
        let f = fixture();
        assert!(matches!(
            f.store.undo(Some("../etc"), false, false).unwrap_err(),
            BackupError::InvalidId(_)
        ));
        assert!(matches!(
            f.store.undo(None, false, false).unwrap_err(),
            BackupError::NoRuns
        ));
    }

    #[test]
    fn utc_formatting_is_sane() {
        let (y, m, d, _, _, _) = utc_now();
        assert!(y >= 2024 && (1..=12).contains(&m) && (1..=31).contains(&d));
    }
}
