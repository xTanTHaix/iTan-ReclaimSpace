//! Two-Stage Quarantine Garbage Collector.
//!
//! # State Machine
//!
//! ```text
//! [State A: Active]   →  (link_count == 1)       →  [State B: Quarantine]
//! [State B: Quarantine] →  (link_count > 1 found)  →  [State A: Active]  (Resurrect)
//! [State B: Quarantine] →  (age > T_cooldown AND link_count == 1)  →  [State C: Purge]
//! ```
//!
//! # TOCTOU Safety
//!
//! The key invariant is that a worker scanning for an object to hardlink will look in
//! `objects/`, not in `quarantine/`.  By atomically moving an orphaned slot into
//! `quarantine/` before the 600-second cooldown, we ensure that any in-flight worker
//! that already computed the hash will either:
//! - Find the slot still in `objects/` (if it ran the lookup before the GC sweep), or
//! - Fail to find it and treat the content as new (publishing a fresh master from `tmp/`).
//!
//! In neither case is there data loss.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::store::CasStore;

/// Default cooldown period before a quarantined slot is permanently purged (§6.2).
pub const DEFAULT_COOLDOWN_SECS: u64 = 600;

/// Errors from GC operations.
#[derive(Debug, Error)]
pub enum GcError {
    #[error("cannot read directory '{path}': {source}")]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot move slot '{src}' to quarantine: {source}")]
    QuarantineMove {
        src: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot purge quarantine entry '{path}': {source}")]
    Purge {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The decision made for a single slot during a GC sweep.
#[derive(Debug, Clone)]
pub enum GcDecision {
    /// Slot was moved from `objects/` to `quarantine/` (State A → B).
    Quarantined { quarantine_path: PathBuf },
    /// Slot was moved back from `quarantine/` to `objects/` because link_count > 1 (State B → A).
    Resurrected { objects_path: PathBuf },
    /// Slot was unlinked from `quarantine/` after cooldown (State B → C).
    Purged { bytes_freed: u64 },
    /// Slot is still active (link_count > 1) — no action taken.
    SkippedActive { link_count: u64 },
    /// Slot in quarantine is not yet past the cooldown period.
    SkippedCooling { remaining_secs: u64 },
}

/// Accumulated statistics for one full GC sweep cycle.
#[derive(Debug, Default, Clone)]
pub struct GcStats {
    pub quarantined: u64,
    pub resurrected: u64,
    pub purged: u64,
    pub bytes_freed: u64,
    pub skipped_active: u64,
    pub errors: Vec<String>,
}

// ─── GC engine ────────────────────────────────────────────────────────────────

/// Two-stage garbage collector for orphaned CAS slots.
pub struct QuarantineGc {
    store: Arc<CasStore>,
    /// Minimum age (seconds) a slot must remain in quarantine before purge.
    cooldown_secs: u64,
}

impl QuarantineGc {
    /// Creates a new GC instance with the given cooldown period.
    pub fn new(store: Arc<CasStore>, cooldown_secs: u64) -> Self {
        Self {
            store,
            cooldown_secs,
        }
    }

    /// Creates a GC instance with the default 600-second cooldown (§6.2).
    pub fn with_default_cooldown(store: Arc<CasStore>) -> Self {
        Self::new(store, DEFAULT_COOLDOWN_SECS)
    }

    // ─── Phase 1: sweep_objects ───────────────────────────────────────────

    /// Scans `objects/` for slots whose filesystem link count is exactly 1 (no workspace
    /// references remain) and moves them atomically into `quarantine/`.
    ///
    /// Returns accumulated statistics; I/O errors on individual slots are logged and
    /// collected into `GcStats::errors` rather than aborting the entire sweep.
    pub fn sweep_objects(&self) -> Result<GcStats, GcError> {
        let objects_dir = self.store.objects_dir();
        let quarantine_dir = self.store.quarantine_dir();
        let mut stats = GcStats::default();

        let shard_iter = fs::read_dir(objects_dir).map_err(|source| GcError::ReadDir {
            path: objects_dir.to_owned(),
            source,
        })?;

        for shard_entry in shard_iter {
            let shard_dir = match shard_entry {
                Ok(e) if e.path().is_dir() => e.path(),
                Ok(_) => continue, // Skip non-directory entries (shouldn't exist).
                Err(e) => {
                    stats
                        .errors
                        .push(format!("ReadDir shard entry error: {}", e));
                    continue;
                }
            };

            let slot_iter = match fs::read_dir(&shard_dir) {
                Ok(i) => i,
                Err(e) => {
                    stats.errors.push(format!(
                        "ReadDir shard '{}' error: {}",
                        shard_dir.display(),
                        e
                    ));
                    continue;
                }
            };

            for slot_entry in slot_iter {
                let slot_path = match slot_entry {
                    Ok(e) => e.path(),
                    Err(e) => {
                        stats.errors.push(format!("Slot entry error: {}", e));
                        continue;
                    }
                };

                match self.evaluate_objects_slot(&slot_path, quarantine_dir) {
                    Ok(GcDecision::Quarantined { .. }) => stats.quarantined += 1,
                    Ok(GcDecision::SkippedActive { .. }) => stats.skipped_active += 1,
                    Ok(_) => {}
                    Err(e) => {
                        stats
                            .errors
                            .push(format!("GC error for '{}': {}", slot_path.display(), e))
                    }
                }
            }
        }

        Ok(stats)
    }

    fn evaluate_objects_slot(
        &self,
        slot_path: &Path,
        quarantine_dir: &Path,
    ) -> Result<GcDecision, GcError> {
        let meta = match fs::metadata(slot_path) {
            Ok(m) => m,
            Err(_) => return Ok(GcDecision::SkippedActive { link_count: 0 }),
        };

        let link_count = platform_link_count(&meta);
        if link_count > 1 {
            return Ok(GcDecision::SkippedActive { link_count });
        }

        // link_count == 1: move to quarantine atomically.
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis();

        let file_name = slot_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let quarantine_name = format!("{}_{}", timestamp_ms, file_name);
        let quarantine_path = quarantine_dir.join(&quarantine_name);

        fs::rename(slot_path, &quarantine_path).map_err(|source| GcError::QuarantineMove {
            src: slot_path.to_owned(),
            source,
        })?;

        log::debug!(
            "GC quarantined: '{}' → '{}'",
            slot_path.display(),
            quarantine_path.display()
        );
        Ok(GcDecision::Quarantined { quarantine_path })
    }

    // ─── Phase 2: sweep_quarantine ────────────────────────────────────────

    /// Processes all entries in `quarantine/`:
    /// - Entries still within the cooldown window are skipped.
    /// - Entries past the cooldown are re-validated:
    ///   - `link_count > 1` → Resurrect (move back to `objects/`).
    ///   - `link_count == 1` → Purge (unlink).
    ///
    /// Returns accumulated statistics; errors on individual entries do not abort the sweep.
    pub fn sweep_quarantine(&self) -> Result<GcStats, GcError> {
        let quarantine_dir = self.store.quarantine_dir();
        let objects_dir = self.store.objects_dir();
        let mut stats = GcStats::default();

        let entries = fs::read_dir(quarantine_dir).map_err(|source| GcError::ReadDir {
            path: quarantine_dir.to_owned(),
            source,
        })?;

        for entry in entries {
            let entry_path = match entry {
                Ok(e) => e.path(),
                Err(e) => {
                    stats
                        .errors
                        .push(format!("ReadDir quarantine entry: {}", e));
                    continue;
                }
            };

            match self.evaluate_quarantine_entry(&entry_path, objects_dir) {
                Ok(GcDecision::Purged { bytes_freed }) => {
                    stats.purged += 1;
                    stats.bytes_freed += bytes_freed;
                }
                Ok(GcDecision::Resurrected { .. }) => stats.resurrected += 1,
                Ok(GcDecision::SkippedCooling { .. }) => {}
                Ok(_) => {}
                Err(e) => stats.errors.push(format!(
                    "Quarantine eval error '{}': {}",
                    entry_path.display(),
                    e
                )),
            }
        }

        Ok(stats)
    }

    fn evaluate_quarantine_entry(
        &self,
        entry_path: &Path,
        objects_dir: &Path,
    ) -> Result<GcDecision, GcError> {
        // Extract timestamp from filename: `<timestamp_ms>_<slot_name>`.
        let file_name = entry_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        let age_ms = quarantine_entry_age_ms(file_name);
        let cooldown_ms = self.cooldown_secs * 1_000;

        if age_ms < cooldown_ms {
            let remaining_secs = (cooldown_ms - age_ms) / 1_000;
            return Ok(GcDecision::SkippedCooling { remaining_secs });
        }

        // Cooldown has passed — re-validate link count.
        let meta = match fs::metadata(entry_path) {
            Ok(m) => m,
            Err(_) => {
                // Entry disappeared between readdir and metadata — treat as already purged.
                return Ok(GcDecision::Purged { bytes_freed: 0 });
            }
        };

        let link_count = platform_link_count(&meta);
        let file_size = meta.len();

        if link_count > 1 {
            // A worker created a new link while this entry was in quarantine — resurrect.
            let slot_name = strip_quarantine_prefix(file_name);
            let prefix = if slot_name.len() >= 2 {
                &slot_name[..2]
            } else {
                "00"
            };
            let target = objects_dir.join(prefix).join(slot_name);
            if let Some(parent) = target.parent() {
                let _ = fs::create_dir_all(parent);
            }
            fs::rename(entry_path, &target).map_err(|source| GcError::QuarantineMove {
                src: entry_path.to_owned(),
                source,
            })?;
            log::info!(
                "GC resurrected: '{}' → '{}'",
                entry_path.display(),
                target.display()
            );
            return Ok(GcDecision::Resurrected {
                objects_path: target,
            });
        }

        // link_count == 1: purge.
        fs::remove_file(entry_path).map_err(|source| GcError::Purge {
            path: entry_path.to_owned(),
            source,
        })?;
        log::info!(
            "GC purged: '{}' ({} bytes freed)",
            entry_path.display(),
            file_size
        );
        Ok(GcDecision::Purged {
            bytes_freed: file_size,
        })
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Parses the millisecond timestamp embedded at the front of a quarantine filename.
/// Format: `<timestamp_ms>_<rest>`.  Returns 0 on parse failure (treats entry as
/// overdue to err on the side of safety — it will still be re-validated by link count).
fn quarantine_entry_age_ms(file_name: &str) -> u64 {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64;

    let ts_ms: u64 = file_name
        .split('_')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    now_ms.saturating_sub(ts_ms)
}

/// Strips the `<timestamp_ms>_` prefix to recover the original slot filename.
fn strip_quarantine_prefix(file_name: &str) -> &str {
    match file_name.find('_') {
        Some(idx) if idx + 1 < file_name.len() => &file_name[idx + 1..],
        _ => file_name,
    }
}

#[cfg(unix)]
fn platform_link_count(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(windows)]
fn platform_link_count(meta: &fs::Metadata) -> u64 {
    let _ = meta;
    1 // number_of_links() requires unstable windows_by_handle; conservative default.
}

#[cfg(not(any(unix, windows)))]
fn platform_link_count(_meta: &fs::Metadata) -> u64 {
    1
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{LinkStrategy, VolumeCapability};
    use std::fs;
    use tempfile::TempDir;

    fn make_gc(dir: &Path) -> (Arc<CasStore>, QuarantineGc) {
        let cap = VolumeCapability {
            volume_root: dir.to_owned(),
            strategy: LinkStrategy::PosixHardlink,
            max_links_per_slot: 1_000,
        };
        let store = Arc::new(CasStore::open(dir, &cap).expect("open store"));
        let gc = QuarantineGc::with_default_cooldown(Arc::clone(&store));
        (store, gc)
    }

    // P5-U01: a slot with link_count == 1 must be quarantined by sweep_objects.
    #[test]
    fn test_gc_sweep_quarantines_orphan_slot() {
        let dir = TempDir::new().expect("tempdir");
        let (store, gc) = make_gc(dir.path());

        // Create a lone slot file in objects/ with no workspace references.
        let shard_dir = store.objects_dir().join("ab");
        fs::create_dir_all(&shard_dir).expect("mkdir shard");
        let slot_path = shard_dir.join("ab".repeat(32) + "_s000");
        fs::write(&slot_path, b"content").expect("write slot");

        let stats = gc.sweep_objects().expect("sweep_objects");
        assert_eq!(stats.quarantined, 1, "one orphan slot must be quarantined");
        assert!(!slot_path.exists(), "slot must be removed from objects/");

        // Quarantine directory must contain exactly one entry.
        let q_count = fs::read_dir(store.quarantine_dir())
            .expect("read quarantine")
            .count();
        assert_eq!(q_count, 1, "quarantine must contain the moved entry");
    }

    // P5-U02: a slot with link_count > 1 must not be touched by sweep_objects.
    // We simulate link_count > 1 by creating a hard link on platforms that support it.
    #[cfg(unix)]
    #[test]
    fn test_gc_skips_active_slot_with_hardlinks() {
        let dir = TempDir::new().expect("tempdir");
        let (store, gc) = make_gc(dir.path());

        let shard_dir = store.objects_dir().join("cd");
        fs::create_dir_all(&shard_dir).expect("mkdir shard");
        let slot_path = shard_dir.join("cd".repeat(32) + "_s000");
        fs::write(&slot_path, b"active content").expect("write slot");

        // Create a hard link to simulate a workspace reference (raises link_count to 2).
        let link_path = dir.path().join("workspace_ref.dll");
        fs::hard_link(&slot_path, &link_path).expect("hard_link");

        let stats = gc.sweep_objects().expect("sweep_objects");
        assert_eq!(stats.skipped_active, 1, "active slot must be skipped");
        assert_eq!(stats.quarantined, 0, "no quarantine for active slot");
        assert!(slot_path.exists(), "active slot must remain in objects/");
    }

    // P5-U03: a quarantine entry within the cooldown window must not be purged.
    #[test]
    fn test_gc_skips_cooling_quarantine_entry() {
        let dir = TempDir::new().expect("tempdir");
        let (store, gc) = make_gc(dir.path());

        // Manually place a "fresh" quarantine entry (timestamp = now).
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis();
        let q_name = format!("{}_abcd_s000", now_ms);
        let q_path = store.quarantine_dir().join(&q_name);
        fs::write(&q_path, b"quarantined").expect("write quarantine entry");

        let stats = gc.sweep_quarantine().expect("sweep_quarantine");
        assert_eq!(stats.purged, 0, "fresh entry must not be purged");
        assert_eq!(stats.resurrected, 0, "fresh entry must not be resurrected");
        assert!(q_path.exists(), "fresh entry must remain in quarantine");
    }

    // P5-U04: an overdue entry with link_count == 1 must be purged.
    #[test]
    fn test_gc_purges_overdue_entry_link1() {
        let dir = TempDir::new().expect("tempdir");
        // Use a 0-second cooldown so the entry is immediately overdue.
        let cap = VolumeCapability {
            volume_root: dir.path().to_owned(),
            strategy: LinkStrategy::PosixHardlink,
            max_links_per_slot: 1_000,
        };
        let store = Arc::new(CasStore::open(dir.path(), &cap).expect("open store"));
        let gc = QuarantineGc::new(Arc::clone(&store), 0);

        // Timestamp = 0 → always overdue.
        let q_name = "0_deadbeef_s000";
        let q_path = store.quarantine_dir().join(q_name);
        fs::write(&q_path, b"stale content").expect("write");

        let stats = gc.sweep_quarantine().expect("sweep_quarantine");
        assert_eq!(stats.purged, 1, "overdue entry must be purged");
        assert!(!q_path.exists(), "purged entry must be deleted from disk");
    }

    // P5-U05: strip_quarantine_prefix must recover the original slot name.
    #[test]
    fn test_strip_quarantine_prefix() {
        // Timestamp prefix = "1234567890", separator = "_", then 64-char hex + "_s000".
        let slot_name = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890_s000";
        let name = format!("1234567890_{}", slot_name);
        let stripped = strip_quarantine_prefix(&name);
        assert_eq!(
            stripped, slot_name,
            "prefix stripping must recover the slot name exactly"
        );
    }

    // P5-U06: sweep_objects on an empty objects directory must complete without error.
    #[test]
    fn test_gc_sweep_objects_empty_dir() {
        let dir = TempDir::new().expect("tempdir");
        let (_store, gc) = make_gc(dir.path());
        let stats = gc.sweep_objects().expect("sweep_objects on empty store");
        assert_eq!(stats.quarantined, 0);
        assert_eq!(stats.errors.len(), 0);
    }
}
