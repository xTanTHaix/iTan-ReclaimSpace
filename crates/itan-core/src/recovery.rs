//! Crash Recovery — Startup Sweeper and stale staging file cleanup.
//!
//! When the engine is killed mid-ingestion, partially written `*.stage` files may be
//! left in `.itan_store/tmp/`.  The `StartupSweeper` must be invoked once at engine
//! startup before any ingestion begins.  It inspects every file in `tmp/` and removes
//! any entry whose modification time is older than `stale_threshold_secs` (default 300 s).
//!
//! Errors on individual files (e.g. permission denied) are collected non-fatally: the
//! sweep continues and all errors are returned in `SweepReport::errors`.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use thiserror::Error;

use crate::store::CasStore;

/// Default age threshold above which a staging file is considered stale (§7).
pub const DEFAULT_STALE_THRESHOLD_SECS: u64 = 300;

/// Errors that can arise during startup sweep operations.
#[derive(Debug, Error, Clone)]
pub enum RecoveryError {
    #[error("cannot read tmp directory '{path}': {message}")]
    ReadDir { path: PathBuf, message: String },

    #[error("cannot stat staging file '{path}': {message}")]
    Stat { path: PathBuf, message: String },

    #[error("cannot remove stale staging file '{path}': {message}")]
    Remove { path: PathBuf, message: String },
}

/// Summary produced by a startup sweep run.
#[derive(Debug, Default, Clone)]
pub struct SweepReport {
    /// Number of stale staging files successfully removed.
    pub removed_count: u64,
    /// Total bytes freed by removal.
    pub bytes_freed: u64,
    /// Non-fatal errors encountered during the sweep.
    pub errors: Vec<RecoveryError>,
}

// ─── Sweeper ──────────────────────────────────────────────────────────────────

/// Inspects `.itan_store/tmp/` at engine startup and removes staging files that are
/// older than `stale_threshold_secs`.
pub struct StartupSweeper {
    store: Arc<CasStore>,
    stale_threshold: Duration,
}

impl StartupSweeper {
    /// Creates a sweeper with the given age threshold.
    pub fn new(store: Arc<CasStore>, stale_threshold_secs: u64) -> Self {
        Self {
            store,
            stale_threshold: Duration::from_secs(stale_threshold_secs),
        }
    }

    /// Creates a sweeper with the default 300-second threshold (§7).
    pub fn with_default_threshold(store: Arc<CasStore>) -> Self {
        Self::new(store, DEFAULT_STALE_THRESHOLD_SECS)
    }

    /// Scans `.itan_store/tmp/` and removes all files whose `mtime` is older than
    /// `stale_threshold`.
    ///
    /// This method never returns an `Err` — all I/O failures are collected into
    /// `SweepReport::errors` to avoid aborting the startup sequence.
    pub fn sweep_stale_tmp(&self) -> SweepReport {
        let tmp_dir = self.store.tmp_dir();
        let mut report = SweepReport::default();

        let entries = match fs::read_dir(tmp_dir) {
            Ok(iter) => iter,
            Err(e) => {
                report.errors.push(RecoveryError::ReadDir {
                    path: tmp_dir.to_owned(),
                    message: e.to_string(),
                });
                return report;
            }
        };

        let now = SystemTime::now();

        for entry_result in entries {
            let entry_path = match entry_result {
                Ok(e) => e.path(),
                Err(e) => {
                    report.errors.push(RecoveryError::ReadDir {
                        path: tmp_dir.to_owned(),
                        message: e.to_string(),
                    });
                    continue;
                }
            };

            // Only process regular files; directories in tmp/ should not exist but
            // we skip them conservatively rather than panicking.
            let meta = match fs::metadata(&entry_path) {
                Ok(m) if m.is_file() => m,
                Ok(_) => continue,
                Err(e) => {
                    report.errors.push(RecoveryError::Stat {
                        path: entry_path.clone(),
                        message: e.to_string(),
                    });
                    continue;
                }
            };

            let age = file_age(&meta, now);
            if age < self.stale_threshold {
                continue; // File is fresh — leave it alone.
            }

            let file_size = meta.len();
            match fs::remove_file(&entry_path) {
                Ok(()) => {
                    report.removed_count += 1;
                    report.bytes_freed += file_size;
                    log::info!(
                        "StartupSweeper: removed stale staging file '{}' (age={:.1}s)",
                        entry_path.display(),
                        age.as_secs_f64()
                    );
                }
                Err(e) => {
                    // Non-fatal: another process may have locked the file.
                    report.errors.push(RecoveryError::Remove {
                        path: entry_path,
                        message: e.to_string(),
                    });
                }
            }
        }

        report
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Returns the elapsed time since the file's `mtime`.
/// Falls back to `Duration::MAX` if the metadata does not carry a valid `mtime`,
/// which causes the file to be treated as infinitely stale (safe to remove).
fn file_age(meta: &fs::Metadata, now: SystemTime) -> Duration {
    meta.modified()
        .ok()
        .and_then(|mtime| now.duration_since(mtime).ok())
        .unwrap_or(Duration::MAX)
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{LinkStrategy, VolumeCapability};
    use std::fs;
    use std::path::Path;
    use std::time::Duration;
    use tempfile::TempDir;

    fn make_sweeper(dir: &Path, threshold_secs: u64) -> (Arc<CasStore>, StartupSweeper) {
        let cap = VolumeCapability {
            volume_root: dir.to_owned(),
            strategy: LinkStrategy::PosixHardlink,
            max_links_per_slot: 1_000,
        };
        let store = Arc::new(CasStore::open(dir, &cap).expect("open store"));
        let sweeper = StartupSweeper::new(Arc::clone(&store), threshold_secs);
        (store, sweeper)
    }

    // P6-U03: sweep of an empty tmp/ directory must succeed with zero removals.
    #[test]
    fn test_sweeper_handles_empty_tmp_dir() {
        let dir = TempDir::new().expect("tempdir");
        let (_store, sweeper) = make_sweeper(dir.path(), 300);
        let report = sweeper.sweep_stale_tmp();
        assert_eq!(
            report.removed_count, 0,
            "empty tmp/ must yield zero removals"
        );
        assert!(report.errors.is_empty(), "no errors expected on empty tmp/");
    }

    // P6-U02: a fresh file (age < threshold) must NOT be removed.
    #[test]
    fn test_sweeper_preserves_fresh_tmp_files() {
        let dir = TempDir::new().expect("tempdir");
        // Use a very large threshold so any freshly created file is preserved.
        let (_store, sweeper) = make_sweeper(dir.path(), 86_400); // 24 hours

        let store_ref = CasStore::open(
            dir.path(),
            &VolumeCapability {
                volume_root: dir.path().to_owned(),
                strategy: LinkStrategy::PosixHardlink,
                max_links_per_slot: 1_000,
            },
        )
        .expect("open");

        let fresh_file = store_ref.tmp_dir().join("fresh_stage.stage");
        fs::write(&fresh_file, b"staging data").expect("write fresh file");

        let report = sweeper.sweep_stale_tmp();
        assert_eq!(report.removed_count, 0, "fresh file must not be removed");
        assert!(fresh_file.exists(), "fresh file must still exist on disk");
    }

    // P6-U01: a file with a threshold of 0 seconds is always stale and must be removed.
    #[test]
    fn test_sweeper_removes_stale_tmp_files_zero_threshold() {
        let dir = TempDir::new().expect("tempdir");
        // threshold=0 means every file is immediately stale.
        let (_store, sweeper) = make_sweeper(dir.path(), 0);

        let store_ref = CasStore::open(
            dir.path(),
            &VolumeCapability {
                volume_root: dir.path().to_owned(),
                strategy: LinkStrategy::PosixHardlink,
                max_links_per_slot: 1_000,
            },
        )
        .expect("open");

        let stale_file = store_ref.tmp_dir().join("stale_12345.stage");
        fs::write(&stale_file, b"stale staging data").expect("write stale file");

        // Give the OS time to write the mtime so file_age > 0.
        std::thread::sleep(Duration::from_millis(10));

        let report = sweeper.sweep_stale_tmp();
        assert_eq!(report.removed_count, 1, "stale file must be removed");
        assert!(
            !stale_file.exists(),
            "stale file must not exist after sweep"
        );
        assert!(report.bytes_freed > 0, "bytes_freed must be positive");
    }

    // P6-U04: a locked/inaccessible file must produce a non-fatal error, not a panic.
    // We simulate this by making the tmp/ directory itself unreadable on POSIX.
    #[cfg(unix)]
    #[test]
    fn test_sweeper_non_fatal_on_read_error() {
        // Root (uid 0) bypasses DAC permissions via CAP_DAC_OVERRIDE, so 0o000 won't cause EACCES.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().expect("tempdir");
        let cap = VolumeCapability {
            volume_root: dir.path().to_owned(),
            strategy: LinkStrategy::PosixHardlink,
            max_links_per_slot: 1_000,
        };
        let store = Arc::new(CasStore::open(dir.path(), &cap).expect("open"));
        let sweeper = StartupSweeper::new(Arc::clone(&store), 0);

        // Revoke read permission on tmp/ to force a ReadDir error.
        let tmp_dir = store.tmp_dir().to_owned();
        fs::set_permissions(&tmp_dir, fs::Permissions::from_mode(0o000)).expect("set permissions");

        let report = sweeper.sweep_stale_tmp();

        // Restore permissions so TempDir cleanup succeeds.
        fs::set_permissions(&tmp_dir, fs::Permissions::from_mode(0o755))
            .expect("restore permissions");

        assert_eq!(
            report.removed_count, 0,
            "no files should be removed when dir is unreadable"
        );
        assert!(
            !report.errors.is_empty(),
            "errors must be collected non-fatally"
        );
    }
}
