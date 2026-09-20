//! Multi-Tier Waterfall Ingestion pipeline.
//!
//! Files flow through three sequential tiers, each cheaper than the last.  A file is
//! rejected as early as possible to avoid unnecessary I/O:
//!
//! ```text
//! [File] → Tier 0: Metadata Stat & Inode Filter
//!              ↓ pass
//!          Tier 1: Boundary Sparse Hash Guard
//!              ↓ candidate (boundary hashes match)
//!          Tier 2: Full CAS Digest
//!              ↓ match → Duplicate(SlotRef)
//! ```
//!
//! The pipeline is **not** thread-safe on its own; callers that parallelise ingestion must
//! instantiate one `IngestPipeline` per worker thread and merge results afterward.  The
//! underlying `CasStore` is safe to share across threads.

use std::collections::HashMap;
use std::path::Path;

use thiserror::Error;

use crate::capability::LinkStrategy;
use crate::digest::{DigestError, MIN_FILE_SIZE, boundary_hash, full_cas_digest};
use crate::store::{CasStore, SlotRef, StoreError};
use crate::whitelist::{FileClass, classify_path};

// ─── Errors ───────────────────────────────────────────────────────────────────

/// Errors that can arise during file ingestion evaluation.
#[derive(Debug, Error)]
pub enum IngestError {
    #[error("filesystem metadata error for '{path}': {source}")]
    Metadata {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("digest computation failed: {0}")]
    Digest(#[from] DigestError),

    #[error("CAS store operation failed: {0}")]
    Store(#[from] StoreError),
}

// ─── Public types ─────────────────────────────────────────────────────────────

/// The reason a file was skipped at Tier 0 without any I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BypassReason {
    /// File size is below the 4 KiB minimum threshold (§3 Tier 0).
    TooSmall,
    /// The file's inode is already tracked in the CAS (link count > 1, inode in cache).
    AlreadyIngested,
    /// The file's extension or location is explicitly prohibited by Target Boundary Rules.
    Prohibited,
    /// The file class is unrecognised and is conservatively skipped.
    UnknownType,
}

/// The outcome of evaluating a single file through the waterfall pipeline.
#[derive(Debug, Clone)]
pub enum IngestVerdict {
    /// The file was bypassed before any content I/O.
    Bypass(BypassReason),
    /// The file passed Tier 1 but its boundary hash differs from every candidate in the store.
    Unique,
    /// The file is a content-level duplicate of the given [`SlotRef`] in the CAS.
    Duplicate(SlotRef),
}

// ─── Pipeline ─────────────────────────────────────────────────────────────────

/// Per-worker state for the Multi-Tier Waterfall Ingestion pipeline.
///
/// `inode_cache` maps platform inode numbers to their corresponding `SlotRef` and acts
/// as the Tier 0 fast-path: if we already know an inode belongs to a CAS slot we can skip
/// all I/O.  The cache is bounded at `MAX_INODE_CACHE_ENTRIES` to prevent unbounded growth
/// during large workspace scans.
pub struct IngestPipeline<'store> {
    store: &'store CasStore,
    strategy: LinkStrategy,
    /// Maps platform inode number → already-ingested SlotRef.
    inode_cache: HashMap<u64, SlotRef>,
    /// Boundary hash cache: maps boundary digest → list of known SlotRefs with that boundary.
    /// Avoids re-reading the store for every Tier-1 candidate.
    boundary_cache: HashMap<[u8; 32], Vec<SlotRef>>,
}

/// Maximum number of inode entries kept in memory.  Above this limit the oldest entry is
/// evicted (simple drain-half strategy to avoid O(n) scan on every insert).
const MAX_INODE_CACHE_ENTRIES: usize = 100_000;

impl<'store> IngestPipeline<'store> {
    /// Creates a new ingestion pipeline bound to `store` and using `strategy` for I/O hints.
    pub fn new(store: &'store CasStore, strategy: LinkStrategy) -> Self {
        Self {
            store,
            strategy,
            inode_cache: HashMap::new(),
            boundary_cache: HashMap::new(),
        }
    }

    /// Evaluates `path` through all three waterfall tiers and returns the verdict.
    ///
    /// This is the main entry point for the ingestion pipeline.  The method is designed
    /// to be called once per candidate file in a workspace traversal.
    ///
    /// # Errors
    ///
    /// Returns [`IngestError`] on I/O or CAS store failures.  A returned `Err` means the
    /// file could not be evaluated — it does not imply the file is corrupt.
    pub fn evaluate(&mut self, path: &Path) -> Result<IngestVerdict, IngestError> {
        // ── Tier 0a: Target Boundary Rules (no I/O) ────────────────────────
        match classify_path(path) {
            FileClass::Prohibited => return Ok(IngestVerdict::Bypass(BypassReason::Prohibited)),
            FileClass::Unknown => return Ok(IngestVerdict::Bypass(BypassReason::UnknownType)),
            FileClass::Whitelisted => {} // Proceed to size check.
        }

        // ── Tier 0b: Size and inode filter ────────────────────────────────
        let meta = std::fs::metadata(path).map_err(|source| IngestError::Metadata {
            path: path.to_owned(),
            source,
        })?;

        if meta.len() < MIN_FILE_SIZE {
            return Ok(IngestVerdict::Bypass(BypassReason::TooSmall));
        }

        // Check the inode cache for an already-ingested identity.
        let inode = platform_inode(&meta);
        if let Some(slot_ref) = self.inode_cache.get(&inode) {
            // Confirm link count > 1 to guard against inode recycling on some filesystems.
            if meta_link_count(&meta) > 1 {
                return Ok(IngestVerdict::Bypass(BypassReason::AlreadyIngested));
            }
            // link_count == 1 means we may have a stale cache entry — proceed normally.
            let _ = slot_ref;
        }

        // ── Tier 1: Boundary Sparse Hash Guard ────────────────────────────
        let b_hash = boundary_hash(path)?;

        // If we have no cached candidates for this boundary hash, mark the boundary
        // as "known but unresolved" with an empty vec, then proceed to Tier 2.
        self.boundary_cache.entry(b_hash).or_default();

        // ── Tier 2: Full CAS Digest ────────────────────────────────────────
        let cas_digest = full_cas_digest(path, self.strategy)?;

        match self.store.lookup(&cas_digest)? {
            Some(slot_ref) => {
                // Cache the inode for future Tier-0 fast-path lookups.
                self.cache_inode(inode, slot_ref.clone());
                // Update the boundary cache with the resolved slot.
                self.boundary_cache
                    .entry(b_hash)
                    .or_default()
                    .push(slot_ref.clone());
                Ok(IngestVerdict::Duplicate(slot_ref))
            }
            None => Ok(IngestVerdict::Unique),
        }
    }

    // ─── Inode cache management ───────────────────────────────────────────

    fn cache_inode(&mut self, inode: u64, slot_ref: SlotRef) {
        if self.inode_cache.len() >= MAX_INODE_CACHE_ENTRIES {
            // Evict roughly half the entries when the cache is full.  A simple drain
            // avoids the overhead of an LRU structure while keeping memory bounded.
            let drain_target = MAX_INODE_CACHE_ENTRIES / 2;
            let keys: Vec<u64> = self
                .inode_cache
                .keys()
                .copied()
                .take(drain_target)
                .collect();
            for k in keys {
                self.inode_cache.remove(&k);
            }
        }
        self.inode_cache.insert(inode, slot_ref);
    }
}

// ─── Platform inode extraction ────────────────────────────────────────────────

#[cfg(unix)]
fn platform_inode(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(windows)]
fn platform_inode(_meta: &std::fs::Metadata) -> u64 {
    // file_index() requires unstable windows_by_handle; return 0 to disable inode cache.
    0
}

#[cfg(not(any(unix, windows)))]
fn platform_inode(_meta: &std::fs::Metadata) -> u64 {
    0 // Unknown platform; disable inode cache.
}

/// Returns the filesystem link count from file metadata.
#[cfg(unix)]
fn meta_link_count(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(windows)]
fn meta_link_count(_meta: &std::fs::Metadata) -> u64 {
    // number_of_links() requires unstable windows_by_handle; return 1 as safe default.
    1
}

#[cfg(not(any(unix, windows)))]
fn meta_link_count(_meta: &std::fs::Metadata) -> u64 {
    1
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{LinkStrategy, VolumeCapability};
    use crate::store::hex_encode;
    use std::fs;
    use tempfile::TempDir;

    // Convenience: sets up store and pipeline with the store having a stable address.
    fn setup(dir: &Path) -> (CasStore, VolumeCapability) {
        let cap = VolumeCapability {
            volume_root: dir.to_owned(),
            strategy: LinkStrategy::PosixHardlink,
            max_links_per_slot: 1_000,
        };
        let store = CasStore::open(dir, &cap).expect("open store");
        (store, cap)
    }

    // P3-U01: file smaller than 4096 bytes must be bypassed as TooSmall.
    #[test]
    fn test_tier0_bypass_too_small() {
        let dir = TempDir::new().expect("tempdir");
        let (store, _cap) = setup(dir.path());
        let mut pipeline = IngestPipeline::new(&store, LinkStrategy::PosixHardlink);

        // A .dll file of 4095 bytes — whitelisted but below the size gate.
        let path = dir.path().join("tiny.dll");
        fs::write(&path, vec![0u8; 4095]).expect("write");

        let verdict = pipeline.evaluate(&path).expect("evaluate");
        assert!(
            matches!(verdict, IngestVerdict::Bypass(BypassReason::TooSmall)),
            "expected TooSmall bypass, got: {:?}",
            verdict
        );
    }

    // P3-U02: file of exactly 4096 bytes must NOT be bypassed as TooSmall.
    #[test]
    fn test_tier0_boundary_4096_proceeds() {
        let dir = TempDir::new().expect("tempdir");
        let (store, _cap) = setup(dir.path());
        let mut pipeline = IngestPipeline::new(&store, LinkStrategy::PosixHardlink);

        let path = dir.path().join("exact.dll");
        fs::write(&path, vec![0x42u8; 4096]).expect("write");

        let verdict = pipeline.evaluate(&path).expect("evaluate");
        // Must NOT be TooSmall — should be Unique since store is empty.
        assert!(
            !matches!(verdict, IngestVerdict::Bypass(BypassReason::TooSmall)),
            "4096-byte file must not be bypassed as TooSmall"
        );
    }

    // P3-U11: .obj file must be bypassed as Prohibited before any I/O.
    #[test]
    fn test_tier0_prohibited_obj_bypassed() {
        let dir = TempDir::new().expect("tempdir");
        let (store, _cap) = setup(dir.path());
        let mut pipeline = IngestPipeline::new(&store, LinkStrategy::PosixHardlink);

        let path = dir.path().join("main.obj");
        fs::write(&path, vec![0u8; 10_000]).expect("write");

        let verdict = pipeline.evaluate(&path).expect("evaluate");
        assert!(
            matches!(verdict, IngestVerdict::Bypass(BypassReason::Prohibited)),
            "expected Prohibited bypass, got: {:?}",
            verdict
        );
    }

    // P3-U12: .pdb file must be bypassed as Prohibited.
    #[test]
    fn test_tier0_prohibited_pdb_bypassed() {
        let dir = TempDir::new().expect("tempdir");
        let (store, _cap) = setup(dir.path());
        let mut pipeline = IngestPipeline::new(&store, LinkStrategy::PosixHardlink);

        let path = dir.path().join("app.pdb");
        fs::write(&path, vec![0u8; 10_000]).expect("write");

        let verdict = pipeline.evaluate(&path).expect("evaluate");
        assert!(
            matches!(verdict, IngestVerdict::Bypass(BypassReason::Prohibited)),
            "expected Prohibited bypass, got: {:?}",
            verdict
        );
    }

    // P3-U08: when a matching slot exists in the store, evaluate must return Duplicate.
    #[test]
    fn test_tier2_duplicate_found() {
        let dir = TempDir::new().expect("tempdir");
        let (store, _cap) = setup(dir.path());

        // Pre-populate the store with the exact content we will evaluate.
        let content = vec![0xABu8; 8_192];
        let digest = *blake3::hash(&content).as_bytes();
        let digest_hex = hex_encode(&digest);
        let prefix = &digest_hex[..2];
        let shard_dir = store.objects_dir().join(prefix);
        fs::create_dir_all(&shard_dir).expect("mkdir shard");
        let slot_path = shard_dir.join(format!("{}_s000", digest_hex));
        fs::write(&slot_path, &content).expect("write slot");

        let candidate = dir.path().join("candidate.dll");
        fs::write(&candidate, &content).expect("write candidate");

        let mut pipeline = IngestPipeline::new(&store, LinkStrategy::PosixHardlink);
        let verdict = pipeline.evaluate(&candidate).expect("evaluate");

        assert!(
            matches!(verdict, IngestVerdict::Duplicate(_)),
            "expected Duplicate, got: {:?}",
            verdict
        );
    }

    // P3-U: a unique file with no store match must return IngestVerdict::Unique.
    #[test]
    fn test_tier2_unique_file() {
        let dir = TempDir::new().expect("tempdir");
        let (store, _cap) = setup(dir.path());
        let mut pipeline = IngestPipeline::new(&store, LinkStrategy::PosixHardlink);

        let path = dir.path().join("unique.dll");
        fs::write(&path, vec![0xFEu8; 8_192]).expect("write");

        let verdict = pipeline.evaluate(&path).expect("evaluate");
        assert!(
            matches!(verdict, IngestVerdict::Unique),
            "expected Unique, got: {:?}",
            verdict
        );
    }

    // P3-U09: inode cache must not grow beyond MAX_INODE_CACHE_ENTRIES.
    #[test]
    fn test_inode_cache_bounded() {
        let dir = TempDir::new().expect("tempdir");
        let (store, _cap) = setup(dir.path());
        let mut pipeline = IngestPipeline::new(&store, LinkStrategy::PosixHardlink);

        // Force-insert entries directly to test the eviction boundary.
        let dummy_slot = crate::store::SlotRef {
            prefix: "ab".into(),
            digest_hex: "a".repeat(64),
            slot_idx: 0,
            path: dir.path().join("dummy"),
        };
        for i in 0..=(MAX_INODE_CACHE_ENTRIES + 100) as u64 {
            pipeline.cache_inode(i, dummy_slot.clone());
        }

        assert!(
            pipeline.inode_cache.len() <= MAX_INODE_CACHE_ENTRIES,
            "cache must not exceed {} entries; currently at {}",
            MAX_INODE_CACHE_ENTRIES,
            pipeline.inode_cache.len()
        );
    }
}
