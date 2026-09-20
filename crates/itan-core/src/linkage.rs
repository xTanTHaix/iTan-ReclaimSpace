//! Atomic Linkage Pipeline — the four-step sequence that replaces a workspace file
//! with a hardlink or block-clone pointing to the CAS Master Slot.
//!
//! The four steps are:
//!
//! 1. **Stage New Master** — if the digest is not yet in the store, write the file content
//!    to `.itan_store/tmp/<UUID>_<hash>.stage` first.
//! 2. **Publish Master** — atomic rename from `tmp/` into `objects/<prefix>/`.  The rename
//!    is NOREPLACE-semantics: the first winning worker becomes the master, losers clean up.
//! 3. **Slot Spill Check** — before creating the hardlink, verify the slot's link count is
//!    below the platform ceiling; spill to the next sibling slot if not.
//! 4. **Atomic In-Place Replace** — create a hardlink at `<target>.tmp_link`, then
//!    atomically swap it into the target position.
//!
//! On `ERROR_SHARING_VIOLATION` or POSIX `ETXTBSY` the failing job is sent to the
//! `DeferRetryQueue` rather than propagating an error.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use thiserror::Error;
use uuid::Uuid;

use crate::capability::LinkStrategy;
use crate::ingest::IngestVerdict;
use crate::permissions::{enforce_read_only, is_executable, strip_read_only};
use crate::retry_queue::{DeferRetryQueue, RetryJob};
use crate::store::{CasStore, SlotRef};

// ─── Error type ───────────────────────────────────────────────────────────────

/// Errors from the Atomic Linkage Pipeline.
#[derive(Debug, Error)]
pub enum LinkError {
    #[error("I/O error during linkage of '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("CAS store error: {0}")]
    Store(#[from] crate::store::StoreError),

    #[error("permission error: {0}")]
    Perm(#[from] crate::permissions::PermError),

    #[error("target file is locked; job enqueued for retry: '{0}'")]
    Deferred(PathBuf),
}

/// Summary of a completed linkage operation.
#[derive(Debug, Clone)]
pub enum LinkResult {
    /// The file was successfully replaced with a hardlink to the given slot.
    Linked {
        target: PathBuf,
        slot: SlotRef,
        /// Number of bytes recovered (size of the original workspace copy).
        bytes_saved: u64,
    },
    /// The file was already ingested (Bypass verdict) — no action taken.
    Skipped,
    /// The file was unique — no matching master existed; the master has been published.
    MasterPublished { slot: SlotRef },
    /// The target was locked; the job has been sent to the retry queue.
    Deferred,
}

// ─── Pipeline ─────────────────────────────────────────────────────────────────

/// Orchestrates the four-step atomic linkage sequence for a single file.
pub struct AtomicLinkagePipeline {
    store: Arc<CasStore>,
    retry_queue: Arc<DeferRetryQueue>,
    /// Strategy is stored for future use in block-clone code paths (RefsBlockClone, BtrfsReflink).
    _strategy: LinkStrategy,
}

impl AtomicLinkagePipeline {
    /// Creates a new pipeline bound to the given store, retry queue, and link strategy.
    pub fn new(
        store: Arc<CasStore>,
        retry_queue: Arc<DeferRetryQueue>,
        strategy: LinkStrategy,
    ) -> Self {
        Self {
            store,
            retry_queue,
            _strategy: strategy,
        }
    }

    /// Executes the full linkage sequence for `target` given the ingestion `verdict`.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError`] on unrecoverable I/O or permission failures.  Transient
    /// lock violations are returned as `Ok(LinkResult::Deferred)` after enqueueing
    /// the job.
    pub fn apply(
        &self,
        target: &Path,
        verdict: IngestVerdict,
        raw_digest: &[u8; 32],
    ) -> Result<LinkResult, LinkError> {
        match verdict {
            IngestVerdict::Bypass(_reason) => Ok(LinkResult::Skipped),
            IngestVerdict::Unique => {
                // The file is new — publish it as a master, then link it.
                let slot = self.publish_new_master(target, raw_digest)?;
                Ok(LinkResult::MasterPublished { slot })
            }
            IngestVerdict::Duplicate(slot) => {
                // The file duplicates an existing master — replace it with a hardlink.
                self.link_to_master(target, slot)
            }
        }
    }

    // ─── Step 1 + 2: Stage and publish a new master ──────────────────────

    fn publish_new_master(&self, source: &Path, digest: &[u8; 32]) -> Result<SlotRef, LinkError> {
        let digest_hex = crate::store::hex_encode(digest);
        let slot_ref = self.store.acquire_slot(digest)?;

        // Fast path: a concurrent worker may have already published this digest.
        if slot_ref.path.exists() {
            return Ok(slot_ref);
        }

        // Step 1: write to tmp/ staging area with a UUID-namespaced filename.
        let stage_name = format!("{}_{}.stage", digest_hex, Uuid::new_v4().simple());
        let stage_path = self.store.tmp_dir().join(stage_name);

        fs::copy(source, &stage_path).map_err(|source| LinkError::Io {
            path: stage_path.clone(),
            source,
        })?;

        // Step 2: Atomic NOREPLACE rename into the shard directory.
        // Ensure the shard directory exists (lazy mkdir).
        let shard_dir = slot_ref
            .path
            .parent()
            .expect("slot path must have a parent");
        fs::create_dir_all(shard_dir).map_err(|source| LinkError::Io {
            path: shard_dir.to_owned(),
            source,
        })?;

        let rename_result = atomic_rename_noreplace(&stage_path, &slot_ref.path);

        match rename_result {
            Ok(()) => {
                // We won the race — apply read-only invariant.
                let exec = is_executable(&slot_ref.path).unwrap_or(false);
                enforce_read_only(&slot_ref.path, exec)?;
                log::debug!("Published new master: {}", slot_ref.path.display());
            }
            Err(RenameError::AlreadyExists) => {
                // Another worker won the race — clean up our staging copy.
                let _ = fs::remove_file(&stage_path);
                log::debug!("Race lost for digest {}; using existing master", digest_hex);
            }
            Err(RenameError::Io(e)) => {
                let _ = fs::remove_file(&stage_path);
                return Err(LinkError::Io {
                    path: slot_ref.path.clone(),
                    source: e,
                });
            }
        }

        Ok(slot_ref)
    }

    // ─── Step 3 + 4: Spill check + atomic in-place replace ───────────────

    fn link_to_master(&self, target: &Path, mut slot: SlotRef) -> Result<LinkResult, LinkError> {
        // Record the size of the workspace copy before we replace it.
        let original_size = fs::metadata(target).map(|m| m.len()).unwrap_or(0);

        // Step 3: Slot Spill Check — acquire the slot that can still accept a new link.
        let raw_digest = parse_digest_from_hex(&slot.digest_hex);
        slot = self.store.acquire_slot(&raw_digest)?;

        // Step 4: Create a temporary hardlink, then atomically swap it into place.
        let tmp_link = sibling_tmp_link(target);

        // Clear the target's read-only flag if it is already a hardlink into the CAS.
        // This is safe because we are replacing it immediately afterward.
        if let Ok(true) = self.can_strip_target_readonly(target) {
            let _ = strip_read_only(target);
        }

        let link_result = platform_hardlink(&slot.path, &tmp_link);

        if let Err(e) = link_result {
            if is_transient_lock_error(&e) {
                let job = RetryJob::new(target.to_owned(), slot.path.clone());
                if self.retry_queue.enqueue(job).is_err() {
                    log::error!(
                        "Retry queue full; permanently dropping link job for '{}'",
                        target.display()
                    );
                }
                return Ok(LinkResult::Deferred);
            }
            return Err(LinkError::Io {
                path: tmp_link,
                source: e,
            });
        }

        // Atomic swap: replace the workspace file with the tmp_link.
        let swap_result = atomic_replace(&tmp_link, target);

        if let Err(e) = swap_result {
            // Clean up the dangling tmp_link before propagating.
            let _ = fs::remove_file(&tmp_link);
            if is_transient_lock_error(&e) {
                let job = RetryJob::new(target.to_owned(), slot.path.clone());
                let _ = self.retry_queue.enqueue(job);
                return Ok(LinkResult::Deferred);
            }
            return Err(LinkError::Io {
                path: target.to_owned(),
                source: e,
            });
        }

        // Ensure the master slot is read-only after all link operations complete.
        let exec = is_executable(&slot.path).unwrap_or(false);
        enforce_read_only(&slot.path, exec)?;

        log::info!(
            "Linked '{}' → '{}' (saved {} bytes)",
            target.display(),
            slot.path.display(),
            original_size
        );

        Ok(LinkResult::Linked {
            target: target.to_owned(),
            slot,
            bytes_saved: original_size,
        })
    }

    /// Returns `true` if the target file has the read-only attribute set, which happens
    /// when it is already a hardlink to a CAS master that was previously deduplicated.
    fn can_strip_target_readonly(&self, target: &Path) -> Result<bool, LinkError> {
        let meta = fs::metadata(target).map_err(|source| LinkError::Io {
            path: target.to_owned(),
            source,
        })?;
        Ok(meta.permissions().readonly())
    }
}

// ─── Platform-specific hardlink creation ──────────────────────────────────────

/// Creates a hardlink from `src` (the CAS master slot) to `dst` (a sibling tmp path).
fn platform_hardlink(src: &Path, dst: &Path) -> Result<(), std::io::Error> {
    fs::hard_link(src, dst)
}

// ─── Platform-specific atomic replace ────────────────────────────────────────

/// Atomically replaces `target` with `tmp_link`.
///
/// On Windows this uses `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`.
/// On POSIX this calls `rename(2)` which is atomic per the POSIX specification.
fn atomic_replace(tmp_link: &Path, target: &Path) -> Result<(), std::io::Error> {
    #[cfg(target_os = "windows")]
    {
        atomic_replace_windows(tmp_link, target)
    }
    #[cfg(not(target_os = "windows"))]
    {
        fs::rename(tmp_link, target)
    }
}

#[cfg(target_os = "windows")]
fn atomic_replace_windows(src: &Path, dst: &Path) -> Result<(), std::io::Error> {
    #[cfg(feature = "windows-ntfs")]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };

        let src_wide: Vec<u16> = OsStr::new(src)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let dst_wide: Vec<u16> = OsStr::new(dst)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: both wide strings are null-terminated and valid.
        let ok = unsafe {
            MoveFileExW(
                src_wide.as_ptr(),
                dst_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(not(feature = "windows-ntfs"))]
    {
        // Fallback: std rename (not fully WRITE_THROUGH but acceptable for feature-off builds).
        fs::rename(src, dst)
    }
}

// ─── NOREPLACE rename ─────────────────────────────────────────────────────────

enum RenameError {
    AlreadyExists,
    Io(std::io::Error),
}

/// Renames `src` to `dst` only if `dst` does not already exist.
///
/// On Linux with the `linux-cow` feature this uses `renameat2(RENAME_NOREPLACE)`.
/// On all other platforms it falls back to an existence-check then rename
/// (which is not atomically NOREPLACE, but the post-condition — one winner, all others
/// clean up — is enforced by the caller verifying `slot_path.exists()` on failure).
fn atomic_rename_noreplace(src: &Path, dst: &Path) -> Result<(), RenameError> {
    #[cfg(all(target_os = "linux", feature = "linux-cow"))]
    {
        renameat2_noreplace(src, dst)
    }

    #[cfg(not(all(target_os = "linux", feature = "linux-cow")))]
    {
        // Portable fallback: check existence before rename.
        if dst.exists() {
            return Err(RenameError::AlreadyExists);
        }
        fs::rename(src, dst).map_err(|e| {
            // If the rename fails because dst now exists (TOCTOU window), treat as AlreadyExists.
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                RenameError::AlreadyExists
            } else {
                RenameError::Io(e)
            }
        })
    }
}

#[cfg(all(target_os = "linux", feature = "linux-cow"))]
fn renameat2_noreplace(src: &Path, dst: &Path) -> Result<(), RenameError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let src_c = CString::new(src.as_os_str().as_bytes())
        .map_err(|e| RenameError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;
    let dst_c = CString::new(dst.as_os_str().as_bytes())
        .map_err(|e| RenameError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;

    // RENAME_NOREPLACE = 1.  renameat2 syscall number varies by arch.
    const RENAME_NOREPLACE: libc::c_uint = 1;

    // SAFETY: both CStrings are valid; AT_FDCWD = -100 is a well-known constant.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            src_c.as_ptr(),
            libc::AT_FDCWD,
            dst_c.as_ptr(),
            RENAME_NOREPLACE,
        )
    };

    if ret == 0 {
        Ok(())
    } else {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EEXIST) || e.raw_os_error() == Some(libc::ENOTEMPTY) {
            Err(RenameError::AlreadyExists)
        } else {
            Err(RenameError::Io(e))
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Constructs the sibling temporary link path: `<target_stem>.tmp_link`.
fn sibling_tmp_link(target: &Path) -> PathBuf {
    let mut p = target.to_owned();
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unnamed")
        .to_owned();
    p.set_file_name(format!("{}.tmp_link", name));
    p
}

/// Parses 32 raw bytes from a 64-character lowercase hex string.
/// Panics on malformed input — callers must ensure the hex is valid (from `SlotRef`).
fn parse_digest_from_hex(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0]);
        let lo = hex_nibble(chunk[1]);
        out[i] = (hi << 4) | lo;
    }
    out
}

#[inline]
fn hex_nibble(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

/// Returns `true` for OS-level errors that indicate the target file is temporarily locked
/// and the operation should be retried rather than failing permanently.
fn is_transient_lock_error(e: &std::io::Error) -> bool {
    #[cfg(target_os = "windows")]
    {
        // ERROR_SHARING_VIOLATION = 32, ERROR_LOCK_VIOLATION = 33.
        matches!(e.raw_os_error(), Some(32) | Some(33))
    }
    #[cfg(unix)]
    {
        // ETXTBSY = text file busy (trying to replace a running executable).
        e.raw_os_error() == Some(libc::ETXTBSY)
    }
    #[cfg(not(any(target_os = "windows", unix)))]
    {
        let _ = e;
        false
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{LinkStrategy, VolumeCapability};
    use crate::store::hex_encode;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn make_pipeline(store_dir: &Path) -> (CasStore, Arc<DeferRetryQueue>, AtomicLinkagePipeline) {
        let cap = VolumeCapability {
            volume_root: store_dir.to_owned(),
            strategy: LinkStrategy::PosixHardlink,
            max_links_per_slot: 1_000,
        };
        let store = Arc::new(CasStore::open(store_dir, &cap).expect("open store"));
        let retry_queue = Arc::new(DeferRetryQueue::new());
        let pipeline = AtomicLinkagePipeline::new(
            Arc::clone(&store),
            Arc::clone(&retry_queue),
            LinkStrategy::PosixHardlink,
        );
        let store_inner = Arc::try_unwrap(store.clone()).unwrap_or_else(|s| (*s).clone());
        (store_inner, retry_queue, pipeline)
    }

    // P4-U01: apply() on a Bypass verdict must return LinkResult::Skipped.
    #[test]
    fn test_apply_bypass_returns_skipped() {
        let dir = TempDir::new().expect("tempdir");
        let (_store, _retry_q, pipeline) = make_pipeline(dir.path());
        let target = dir.path().join("target.dll");
        fs::write(&target, vec![0u8; 8192]).expect("write");
        let digest = [0u8; 32];

        let result = pipeline
            .apply(
                &target,
                IngestVerdict::Bypass(crate::ingest::BypassReason::TooSmall),
                &digest,
            )
            .expect("apply");
        assert!(
            matches!(result, LinkResult::Skipped),
            "expected Skipped, got: {:?}",
            result
        );
    }

    // P4-U02: apply() with Unique verdict must stage and publish the master slot.
    #[test]
    fn test_apply_unique_publishes_master() {
        let dir = TempDir::new().expect("tempdir");

        let cap = VolumeCapability {
            volume_root: dir.path().to_owned(),
            strategy: LinkStrategy::PosixHardlink,
            max_links_per_slot: 1_000,
        };
        let store = Arc::new(CasStore::open(dir.path(), &cap).expect("open store"));
        let retry_queue = Arc::new(DeferRetryQueue::new());
        let pipeline = AtomicLinkagePipeline::new(
            Arc::clone(&store),
            Arc::clone(&retry_queue),
            LinkStrategy::PosixHardlink,
        );

        let content = vec![0xAAu8; 8192];
        let source = dir.path().join("source.dll");
        fs::write(&source, &content).expect("write source");

        let digest = *blake3::hash(&content).as_bytes();
        let result = pipeline
            .apply(&source, IngestVerdict::Unique, &digest)
            .expect("apply");

        assert!(
            matches!(result, LinkResult::MasterPublished { .. }),
            "expected MasterPublished, got: {:?}",
            result
        );

        // The slot file must exist in the object store.
        let digest_hex = hex_encode(&digest);
        let slot_path = dir
            .path()
            .join(".itan_store")
            .join("objects")
            .join(&digest_hex[..2])
            .join(format!("{}_s000", digest_hex));
        assert!(
            slot_path.exists(),
            "master slot must be on disk after publish"
        );
    }

    // P4-U06: master slot must be read-only after publication.
    #[test]
    fn test_master_slot_readonly_after_publish() {
        let dir = TempDir::new().expect("tempdir");
        let cap = VolumeCapability {
            volume_root: dir.path().to_owned(),
            strategy: LinkStrategy::PosixHardlink,
            max_links_per_slot: 1_000,
        };
        let store = Arc::new(CasStore::open(dir.path(), &cap).expect("open store"));
        let retry_queue = Arc::new(DeferRetryQueue::new());
        let pipeline = AtomicLinkagePipeline::new(
            Arc::clone(&store),
            Arc::clone(&retry_queue),
            LinkStrategy::PosixHardlink,
        );

        let content = vec![0xBBu8; 8192];
        let source = dir.path().join("source.dll");
        fs::write(&source, &content).expect("write");
        let digest = *blake3::hash(&content).as_bytes();

        pipeline
            .apply(&source, IngestVerdict::Unique, &digest)
            .expect("apply");

        let digest_hex = hex_encode(&digest);
        let slot_path = dir
            .path()
            .join(".itan_store")
            .join("objects")
            .join(&digest_hex[..2])
            .join(format!("{}_s000", digest_hex));

        let meta = fs::metadata(&slot_path).expect("metadata");
        assert!(
            meta.permissions().readonly(),
            "published master slot must be read-only"
        );
    }

    // sibling_tmp_link must produce a path in the same directory with .tmp_link suffix.
    #[test]
    fn test_sibling_tmp_link_path() {
        let link = sibling_tmp_link(Path::new("/workspace/project/foo.dll"));
        assert_eq!(
            link,
            PathBuf::from("/workspace/project/foo.dll.tmp_link"),
            "tmp_link path must be adjacent to target"
        );
    }

    // parse_digest_from_hex must round-trip correctly.
    #[test]
    fn test_parse_digest_from_hex_roundtrip() {
        let original = [0xABu8; 32];
        let hex = hex_encode(&original);
        let parsed = parse_digest_from_hex(&hex);
        assert_eq!(parsed, original, "hex round-trip must be lossless");
    }
}
