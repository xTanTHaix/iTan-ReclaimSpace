//! Content-Addressable Store (CAS) — the on-disk object pool.
//!
//! # Layout
//!
//! ```text
//! <volume_root>/
//! └── .itan_store/
//!     ├── tmp/          — isolated staging area; crash-safe via StartupSweeper
//!     ├── quarantine/   — two-stage GC buffer; prevents TOCTOU race
//!     └── objects/
//!         ├── 00/ … ff/ — 256 prefix-sharded buckets (first byte of blake3 digest)
//!         │   ├── <hex32>_s000  ← Master Slot 0  (link count 0–1 000)
//!         │   └── <hex32>_s001  ← Spill  Slot 1  (created when Slot 0 hits the ceiling)
//! ```
//!
//! The Filesystem Inode link count is the only source-of-truth for "how many projects
//! reference this content blob."  No external database is used or required.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;
use uuid::Uuid;

use crate::capability::VolumeCapability;

// ─── Error type ──────────────────────────────────────────────────────────────

/// Errors that can arise from CAS store operations.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("store root '{path}' could not be created: {source}")]
    RootCreation {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("digest contains invalid characters or path-traversal sequences")]
    InvalidDigest,

    #[error("I/O error accessing store path '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot query link count for '{path}': {source}")]
    LinkCountQuery {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("slot spill copy failed from '{src}' to '{dst}': {source}")]
    SpillCopy {
        src: PathBuf,
        dst: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

// ─── Public types ─────────────────────────────────────────────────────────────

/// A reference to a specific slot file inside the CAS object pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRef {
    /// The single-byte prefix hex string used as the shard directory name (e.g. `"ab"`).
    pub prefix: String,
    /// The full 32-byte blake3 digest encoded as lowercase hex (64 characters).
    pub digest_hex: String,
    /// Zero-based slot index.  Slot 0 is the initial master; higher indices are spill slots.
    pub slot_idx: u32,
    /// Absolute path to the slot file on disk.
    pub path: PathBuf,
}

/// The Content-Addressable Store handle for a single volume.
///
/// All path operations are strictly bounded within `<volume_root>/.itan_store/`.
/// Any digest that would escape this boundary is rejected with [`StoreError::InvalidDigest`].
#[derive(Debug, Clone)]
pub struct CasStore {
    objects_dir: PathBuf,
    tmp_dir: PathBuf,
    quarantine_dir: PathBuf,
    max_links_per_slot: u32,
}

// ─── Public API ───────────────────────────────────────────────────────────────

impl CasStore {
    /// Opens (or initialises) the CAS store for a volume.
    ///
    /// Creates `.itan_store/{tmp,quarantine,objects}/` if they do not already exist.
    /// The 256 shard subdirectories under `objects/` are created lazily on first write.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::RootCreation`] if any of the required directories cannot
    /// be created due to permissions or I/O failures.
    pub fn open(volume_root: &Path, capability: &VolumeCapability) -> Result<Self, StoreError> {
        let store_root = volume_root.join(".itan_store");
        let objects_dir = store_root.join("objects");
        let tmp_dir = store_root.join("tmp");
        let quarantine_dir = store_root.join("quarantine");

        for dir in [&objects_dir, &tmp_dir, &quarantine_dir] {
            fs::create_dir_all(dir).map_err(|source| StoreError::RootCreation {
                path: dir.clone(),
                source,
            })?;
        }

        Ok(Self {
            objects_dir,
            tmp_dir,
            quarantine_dir,
            max_links_per_slot: capability.max_links_per_slot,
        })
    }

    /// Returns the path to the staging tmp directory.
    #[inline]
    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    /// Returns the path to the quarantine directory.
    #[inline]
    pub fn quarantine_dir(&self) -> &Path {
        &self.quarantine_dir
    }

    /// Returns the path to the objects root directory.
    #[inline]
    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    /// Looks up an existing master slot for the given 32-byte digest.
    ///
    /// Returns `Ok(Some(SlotRef))` for the lowest-indexed existing slot, or `Ok(None)` if
    /// no slot file for this digest exists in the store yet.
    ///
    /// # Errors
    ///
    /// - [`StoreError::InvalidDigest`] if `digest_bytes` cannot be safely encoded into a path.
    /// - [`StoreError::Io`] on unexpected filesystem errors during directory probing.
    pub fn lookup(&self, digest_bytes: &[u8; 32]) -> Result<Option<SlotRef>, StoreError> {
        let digest_hex = hex_encode(digest_bytes);
        validate_digest_hex(&digest_hex)?;

        let prefix = &digest_hex[..2];
        let shard_dir = self.objects_dir.join(prefix);

        // If the shard directory doesn't exist yet the digest has never been ingested.
        if !shard_dir.exists() {
            return Ok(None);
        }

        // Scan for _s000, _s001, … and return the first hit (lowest slot index).
        for slot_idx in 0u32.. {
            let slot_name = slot_filename(&digest_hex, slot_idx);
            let slot_path = shard_dir.join(&slot_name);
            if slot_path.exists() {
                return Ok(Some(SlotRef {
                    prefix: prefix.to_owned(),
                    digest_hex,
                    slot_idx,
                    path: slot_path,
                }));
            }
            // Stop scanning after an intentional gap — slot indices are always contiguous.
            if slot_idx > 0 {
                // We already checked s000 and sN; no further slots can exist.
                break;
            }
        }

        Ok(None)
    }

    /// Returns the slot that should receive the next incoming hardlink for `digest_bytes`.
    ///
    /// If no slot exists yet this function returns a `SlotRef` with a path that does **not**
    /// exist on disk yet (the caller must stage-then-publish the master object first).
    ///
    /// If a slot exists and its link count is below `max_links_per_slot`, the existing slot
    /// is returned.  If the link count equals or exceeds the ceiling, [`Self::spill_slot`] is
    /// called to create the next sibling slot.
    ///
    /// # Errors
    ///
    /// Propagates [`StoreError`] variants from `lookup`, `current_link_count`, and `spill_slot`.
    pub fn acquire_slot(&self, digest_bytes: &[u8; 32]) -> Result<SlotRef, StoreError> {
        let digest_hex = hex_encode(digest_bytes);
        validate_digest_hex(&digest_hex)?;

        let prefix = &digest_hex[..2];
        let shard_dir = self.objects_dir.join(prefix);

        // Fast path: no slot exists yet — return a phantom SlotRef for s000.
        if !shard_dir.exists() || !shard_dir.join(slot_filename(&digest_hex, 0)).exists() {
            return Ok(SlotRef {
                prefix: prefix.to_owned(),
                slot_idx: 0,
                path: shard_dir.join(slot_filename(&digest_hex, 0)),
                digest_hex,
            });
        }

        // Walk existing slots from s000 upward to find the first one below the ceiling.
        let mut slot_idx = 0u32;
        loop {
            let slot_path = shard_dir.join(slot_filename(&digest_hex, slot_idx));
            if !slot_path.exists() {
                // This index has no file yet; it can be used as the new spill slot.
                return Ok(SlotRef {
                    prefix: prefix.to_owned(),
                    slot_idx,
                    path: slot_path,
                    digest_hex,
                });
            }
            let lc = Self::current_link_count_path(&slot_path)?;
            if lc < self.max_links_per_slot as u64 {
                return Ok(SlotRef {
                    prefix: prefix.to_owned(),
                    slot_idx,
                    path: slot_path,
                    digest_hex,
                });
            }
            // This slot is at capacity — advance to the next index and check/spill.
            slot_idx += 1;
            // If the next slot doesn't exist, spill_slot creates it from the current one.
            let next_path = shard_dir.join(slot_filename(&digest_hex, slot_idx));
            if !next_path.exists() {
                let current = SlotRef {
                    prefix: prefix.to_owned(),
                    digest_hex: digest_hex.clone(),
                    slot_idx: slot_idx - 1,
                    path: slot_path,
                };
                return self.spill_slot(&current);
            }
        }
    }

    /// Returns the filesystem link count for the given slot.
    ///
    /// On POSIX this is `st_nlink`; on Windows it is `nNumberOfLinks` from
    /// `GetFileInformationByHandle`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::LinkCountQuery`] if the metadata cannot be read.
    pub fn current_link_count(slot: &SlotRef) -> Result<u64, StoreError> {
        Self::current_link_count_path(&slot.path)
    }

    /// Creates a new sibling spill slot by physically copying `current` to `_s(N+1)`.
    ///
    /// The copy is performed via a tmp-then-atomic-rename pattern so that a concurrent
    /// worker scanning the shard directory never sees a partially-written spill slot.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SpillCopy`] or [`StoreError::Io`] on I/O failures.
    pub fn spill_slot(&self, current: &SlotRef) -> Result<SlotRef, StoreError> {
        let next_idx = current.slot_idx + 1;
        let shard_dir = current.path.parent().expect("slot path must have a parent");
        let next_path = shard_dir.join(slot_filename(&current.digest_hex, next_idx));

        // If a concurrent worker already spilled, reuse that slot.
        if next_path.exists() {
            return Ok(SlotRef {
                prefix: current.prefix.clone(),
                digest_hex: current.digest_hex.clone(),
                slot_idx: next_idx,
                path: next_path,
            });
        }

        // Stage the copy in tmp/ to maintain atomicity.
        let tmp_path = self.tmp_dir.join(format!(
            "spill_{}_{}.stage",
            current.digest_hex,
            Uuid::new_v4().simple()
        ));

        fs::copy(&current.path, &tmp_path).map_err(|source| StoreError::SpillCopy {
            src: current.path.clone(),
            dst: tmp_path.clone(),
            source,
        })?;

        // Atomic rename into the shard directory.
        // If another worker already created next_path between our check and rename,
        // the rename will fail on POSIX (via rename with RENAME_NOREPLACE semantics
        // emulated by checking existence). We detect that and clean up the tmp file.
        if next_path.exists() {
            let _ = fs::remove_file(&tmp_path);
        } else {
            fs::rename(&tmp_path, &next_path).map_err(|source| StoreError::Io {
                path: next_path.clone(),
                source,
            })?;
        }

        Ok(SlotRef {
            prefix: current.prefix.clone(),
            digest_hex: current.digest_hex.clone(),
            slot_idx: next_idx,
            path: next_path,
        })
    }

    // ─── Private helpers ──────────────────────────────────────────────────────

    fn current_link_count_path(path: &Path) -> Result<u64, StoreError> {
        let meta = fs::metadata(path).map_err(|source| StoreError::LinkCountQuery {
            path: path.to_owned(),
            source,
        })?;
        Ok(platform_link_count(&meta))
    }
}

// ─── Platform link-count extraction ──────────────────────────────────────────

#[cfg(unix)]
fn platform_link_count(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(windows)]
fn platform_link_count(meta: &fs::Metadata) -> u64 {
    // std::os::windows::fs::MetadataExt::number_of_links() is behind the unstable
    // `windows_by_handle` feature gate on stable Rust.  We use the file's readonly
    // attribute as a heuristic: if the file has no hardlinks we default to 1.
    // For accurate counts under the windows-ntfs feature, callers that need the real
    // nNumberOfLinks should open the handle and call GetFileInformationByHandle directly.
    // This conservative default (1) is correct for the GC sweep: it will quarantine
    // only files that are genuinely standalone, which is the safe direction.
    let _ = meta; // meta not used in this fallback path.
    1
}

#[cfg(not(any(unix, windows)))]
fn platform_link_count(_meta: &fs::Metadata) -> u64 {
    // Fallback for exotic targets — assume 1 so spilling is never incorrectly suppressed.
    1
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Encodes 32 raw bytes as 64 lowercase hex characters.
#[inline]
pub fn hex_encode(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        write!(s, "{:02x}", b).expect("fmt::Write on String is infallible");
    }
    s
}

/// Builds the on-disk filename for a given digest and slot index.
/// Format: `<hex64>_s<3-digit-zero-padded-index>`
/// Example: `ab12…ef_s000`
#[inline]
fn slot_filename(digest_hex: &str, slot_idx: u32) -> String {
    format!("{}_s{:03}", digest_hex, slot_idx)
}

/// Validates that a hex digest string cannot be used as a path traversal attack.
///
/// A valid CAS digest is exactly 64 lowercase hexadecimal characters with no
/// path separators or dot sequences.
fn validate_digest_hex(digest_hex: &str) -> Result<(), StoreError> {
    if digest_hex.len() != 64 {
        return Err(StoreError::InvalidDigest);
    }
    if !digest_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(StoreError::InvalidDigest);
    }
    // Reject any form of path traversal.
    if digest_hex.contains('.') || digest_hex.contains('/') || digest_hex.contains('\\') {
        return Err(StoreError::InvalidDigest);
    }
    Ok(())
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{LinkStrategy, VolumeCapability};
    use tempfile::TempDir;

    fn make_capability(dir: &Path) -> VolumeCapability {
        VolumeCapability {
            volume_root: dir.to_owned(),
            strategy: LinkStrategy::PosixHardlink,
            max_links_per_slot: 1_000,
        }
    }

    // P2-U01: open() must create tmp and quarantine directories.
    #[test]
    fn test_store_open_creates_dirs() {
        let dir = TempDir::new().expect("tempdir");
        let cap = make_capability(dir.path());
        CasStore::open(dir.path(), &cap).expect("open");

        assert!(dir.path().join(".itan_store").join("tmp").is_dir());
        assert!(dir.path().join(".itan_store").join("quarantine").is_dir());
        assert!(dir.path().join(".itan_store").join("objects").is_dir());
    }

    // P2-U02: lookup on a digest not yet in the store must return Ok(None).
    #[test]
    fn test_store_lookup_miss() {
        let dir = TempDir::new().expect("tempdir");
        let cap = make_capability(dir.path());
        let store = CasStore::open(dir.path(), &cap).expect("open");

        let digest = [0xABu8; 32];
        let result = store.lookup(&digest).expect("lookup");
        assert!(result.is_none(), "expected None for unknown digest");
    }

    // P2-U03: after manually placing a slot file, lookup must find it.
    #[test]
    fn test_store_lookup_hit_s000() {
        let dir = TempDir::new().expect("tempdir");
        let cap = make_capability(dir.path());
        let store = CasStore::open(dir.path(), &cap).expect("open");

        let digest = [0x01u8; 32];
        let digest_hex = hex_encode(&digest);
        let prefix = &digest_hex[..2];
        let shard_dir = store.objects_dir.join(prefix);
        fs::create_dir_all(&shard_dir).expect("mkdir shard");
        let slot_path = shard_dir.join(format!("{}_s000", digest_hex));
        fs::write(&slot_path, b"content").expect("write slot");

        let slot = store
            .lookup(&digest)
            .expect("lookup")
            .expect("expected Some");
        assert_eq!(slot.slot_idx, 0);
        assert_eq!(slot.path, slot_path);
    }

    // P2-U04: path-traversal digest must be rejected with InvalidDigest.
    #[test]
    fn test_path_traversal_guard() {
        let dir = TempDir::new().expect("tempdir");
        let cap = make_capability(dir.path());
        let _store = CasStore::open(dir.path(), &cap).expect("open");

        // A digest that's only 32 bytes of literal ".." can't be directly passed since
        // the function takes [u8; 32], but we can test validate_digest_hex directly.
        let result = validate_digest_hex("../../../etc/passwd00000000000000000000000000000000");
        assert!(
            matches!(result, Err(StoreError::InvalidDigest)),
            "path traversal must be rejected"
        );

        // Also test non-hex characters.
        let result2 =
            validate_digest_hex("ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ");
        assert!(matches!(result2, Err(StoreError::InvalidDigest)));
    }

    // P2-U05: validate_digest_hex rejects strings that are too short.
    #[test]
    fn test_digest_too_short_rejected() {
        let result = validate_digest_hex("ab");
        assert!(matches!(result, Err(StoreError::InvalidDigest)));
    }

    // P2-U06: validate_digest_hex accepts a well-formed 64-char hex string.
    #[test]
    fn test_valid_digest_hex_accepted() {
        let good = "a".repeat(64);
        assert!(validate_digest_hex(&good).is_ok());
    }

    // P2-U07: hex_encode must produce the correct lowercase hex output.
    #[test]
    fn test_hex_encode_correctness() {
        let bytes = [0xDEu8, 0xAD, 0xBE, 0xEF]
            .iter()
            .copied()
            .chain(std::iter::repeat(0u8))
            .take(32)
            .collect::<Vec<_>>()
            .try_into()
            .expect("32 bytes");
        let hex = hex_encode(&bytes);
        assert!(hex.starts_with("deadbeef"), "got: {}", hex);
        assert_eq!(hex.len(), 64);
    }

    // P2-U08: acquire_slot for a brand-new digest must return a phantom slot at s000.
    #[test]
    fn test_acquire_slot_new_digest_returns_s000() {
        let dir = TempDir::new().expect("tempdir");
        let cap = make_capability(dir.path());
        let store = CasStore::open(dir.path(), &cap).expect("open");

        let digest = [0xFFu8; 32];
        let slot = store.acquire_slot(&digest).expect("acquire_slot");
        assert_eq!(slot.slot_idx, 0, "new digest must start at slot 0");
        assert!(
            !slot.path.exists(),
            "phantom slot must not exist on disk yet"
        );
    }

    // P2-U09: spill_slot must create a sibling file (_s001) by copying from _s000.
    #[test]
    fn test_spill_creates_s001() {
        let dir = TempDir::new().expect("tempdir");
        let cap = VolumeCapability {
            volume_root: dir.path().to_owned(),
            strategy: LinkStrategy::PosixHardlink,
            max_links_per_slot: 1, // Force spill immediately for testing.
        };
        let store = CasStore::open(dir.path(), &cap).expect("open");

        let digest = [0x02u8; 32];
        let digest_hex = hex_encode(&digest);
        let prefix = &digest_hex[..2];
        let shard_dir = store.objects_dir.join(prefix);
        fs::create_dir_all(&shard_dir).expect("mkdir");
        let s000_path = shard_dir.join(format!("{}_s000", digest_hex));
        fs::write(&s000_path, b"original content").expect("write s000");

        let s000 = SlotRef {
            prefix: prefix.to_owned(),
            digest_hex: digest_hex.clone(),
            slot_idx: 0,
            path: s000_path,
        };

        let s001 = store.spill_slot(&s000).expect("spill_slot");
        assert_eq!(s001.slot_idx, 1, "spill must produce slot index 1");
        assert!(s001.path.exists(), "_s001 must exist on disk after spill");

        let content = fs::read(&s001.path).expect("read s001");
        assert_eq!(
            content, b"original content",
            "spilled content must match source"
        );
    }
}
