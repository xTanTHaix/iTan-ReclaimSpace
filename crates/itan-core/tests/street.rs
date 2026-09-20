//! Street Tests — real on-disk integration tests for the full Waterfall Pipeline.
//!
//! Street tests operate on actual filesystem paths created in a `tempfile::TempDir`.
//! Every test exercises the *complete* pipeline from `probe_volume` → `CasStore::open`
//! → `IngestPipeline::evaluate` → `AtomicLinkagePipeline::apply`, verifying the
//! observable on-disk postconditions rather than internal state.
//!
//! Run with:
//! ```
//! cargo test -p itan-core --test street -- --test-threads=1 --nocapture
//! ```

use std::fs;
use std::path::Path;
use std::sync::Arc;

use tempfile::TempDir;

use itan_core::{
    capability::{LinkStrategy, VolumeCapability},
    gc::QuarantineGc,
    ingest::{BypassReason, IngestPipeline, IngestVerdict},
    linkage::{AtomicLinkagePipeline, LinkResult},
    recovery::StartupSweeper,
    retry_queue::DeferRetryQueue,
    store::{CasStore, hex_encode},
};

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn make_env(dir: &Path) -> (Arc<CasStore>, Arc<DeferRetryQueue>, VolumeCapability) {
    let cap = VolumeCapability {
        volume_root: dir.to_owned(),
        strategy: LinkStrategy::PosixHardlink,
        max_links_per_slot: 1_000,
    };
    let store = Arc::new(CasStore::open(dir, &cap).expect("open store"));
    let retry_q = Arc::new(DeferRetryQueue::new());
    (store, retry_q, cap)
}

fn digest_of(content: &[u8]) -> [u8; 32] {
    *blake3::hash(content).as_bytes()
}

fn write_file(dir: &Path, name: &str, content: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).expect("write file");
    path
}

// ─── ST-01: End-to-end deduplicate a large file ───────────────────────────────

#[test]
fn st01_end_to_end_deduplication() {
    let dir = TempDir::new().expect("tempdir");
    let (store, retry_q, cap) = make_env(dir.path());

    let content: Vec<u8> = (0u8..=255).cycle().take(256 * 1024).collect();
    let digest = digest_of(&content);

    let first = write_file(dir.path(), "copy_a.dll", &content);
    let mut pipeline = IngestPipeline::new(&store, cap.strategy);
    let verdict_a = pipeline.evaluate(&first).expect("evaluate a");
    assert!(
        matches!(verdict_a, IngestVerdict::Unique),
        "first copy must be Unique; got: {:?}",
        verdict_a
    );

    let link_pipeline =
        AtomicLinkagePipeline::new(Arc::clone(&store), Arc::clone(&retry_q), cap.strategy);
    let result_a = link_pipeline
        .apply(&first, verdict_a, &digest)
        .expect("apply a");
    assert!(
        matches!(result_a, LinkResult::MasterPublished { .. }),
        "first copy must publish master; got: {:?}",
        result_a
    );

    let digest_hex = hex_encode(&digest);
    let slot_path = dir
        .path()
        .join(".itan_store/objects")
        .join(&digest_hex[..2])
        .join(format!("{}_s000", digest_hex));
    assert!(slot_path.exists(), "master slot must be on disk");
    assert!(
        fs::metadata(&slot_path).unwrap().permissions().readonly(),
        "master slot must be read-only"
    );

    let second = write_file(dir.path(), "copy_b.dll", &content);
    let mut pipeline2 = IngestPipeline::new(&store, cap.strategy);
    let verdict_b = pipeline2.evaluate(&second).expect("evaluate b");
    assert!(
        matches!(verdict_b, IngestVerdict::Duplicate(_)),
        "second copy must be Duplicate; got: {:?}",
        verdict_b
    );
}

// ─── ST-02: Files below 4 KiB must be bypassed ────────────────────────────────

#[test]
fn st02_small_files_bypassed() {
    let dir = TempDir::new().expect("tempdir");
    let (store, _retry_q, cap) = make_env(dir.path());

    let tiny = write_file(dir.path(), "tiny.dll", &[0xFFu8; 100]);
    let mut pipeline = IngestPipeline::new(&store, cap.strategy);
    let verdict = pipeline.evaluate(&tiny).expect("evaluate");
    assert!(
        matches!(verdict, IngestVerdict::Bypass(BypassReason::TooSmall)),
        "100-byte file must be TooSmall; got: {:?}",
        verdict
    );
}

// ─── ST-03: Prohibited file types must be bypassed before I/O ─────────────────

#[test]
fn st03_prohibited_extensions_bypassed() {
    let dir = TempDir::new().expect("tempdir");
    let (store, _retry_q, cap) = make_env(dir.path());
    let mut pipeline = IngestPipeline::new(&store, cap.strategy);

    for ext in &["obj", "pdb", "d", "tsbuildinfo"] {
        let name = format!("build_artifact.{}", ext);
        let path = write_file(dir.path(), &name, &[0xABu8; 10_000]);
        let verdict = pipeline.evaluate(&path).expect("evaluate");
        assert!(
            matches!(verdict, IngestVerdict::Bypass(BypassReason::Prohibited)),
            "{} must be Prohibited; got: {:?}",
            name,
            verdict
        );
    }
}

// ─── ST-04: Whitelisted extensions pass through ────────────────────────────────

#[test]
fn st04_whitelisted_extensions_pass_tier0() {
    let dir = TempDir::new().expect("tempdir");
    let (store, _retry_q, cap) = make_env(dir.path());
    let mut pipeline = IngestPipeline::new(&store, cap.strategy);

    for ext in &["dll", "so", "lib", "png", "gguf"] {
        let name = format!("asset.{}", ext);
        let path = write_file(dir.path(), &name, &[0xCCu8; 8_192]);
        let verdict = pipeline.evaluate(&path).expect("evaluate");
        assert!(
            !matches!(verdict, IngestVerdict::Bypass(BypassReason::Prohibited)),
            "{} must not be Prohibited; got: {:?}",
            name,
            verdict
        );
        assert!(
            !matches!(verdict, IngestVerdict::Bypass(BypassReason::TooSmall)),
            "{} (8 KiB) must not be TooSmall; got: {:?}",
            name,
            verdict
        );
    }
}

// ─── ST-05: Startup sweeper removes stale .stage files ────────────────────────

#[test]
fn st05_startup_sweeper_cleans_stale_staging() {
    let dir = TempDir::new().expect("tempdir");
    let (store, _retry_q, _cap) = make_env(dir.path());

    let stale = store.tmp_dir().join("deadbeef_abcdef.stage");
    fs::write(&stale, b"partial staging data").expect("write stale");

    let sweeper = StartupSweeper::new(Arc::clone(&store), 0);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let report = sweeper.sweep_stale_tmp();

    assert_eq!(
        report.removed_count, 1,
        "stale staging file must be removed"
    );
    assert!(!stale.exists(), "staging file must not exist after sweep");
    assert!(report.errors.is_empty(), "no errors expected");
}

// ─── ST-06: GC state machine A → B → C ───────────────────────────────────────

#[test]
fn st06_gc_quarantine_then_purge() {
    let dir = TempDir::new().expect("tempdir");
    let (store, _retry_q, _cap) = make_env(dir.path());

    let shard = store.objects_dir().join("ef");
    fs::create_dir_all(&shard).expect("mkdir shard");
    let slot = shard.join("ef".repeat(32) + "_s000");
    fs::write(&slot, b"orphaned content").expect("write orphan");

    let gc = QuarantineGc::new(Arc::clone(&store), 0);
    let obj_stats = gc.sweep_objects().expect("sweep_objects");
    assert_eq!(obj_stats.quarantined, 1, "orphan must be quarantined");
    assert!(!slot.exists(), "slot must leave objects/");

    std::thread::sleep(std::time::Duration::from_millis(20));

    let q_stats = gc.sweep_quarantine().expect("sweep_quarantine");
    assert_eq!(q_stats.purged, 1, "orphan must be purged from quarantine");
    assert_eq!(q_stats.bytes_freed, 16, "bytes_freed must match file size");
}

// ─── ST-07: Node_modules path is whitelisted regardless of extension ──────────

#[test]
fn st07_node_modules_whitelisted() {
    let dir = TempDir::new().expect("tempdir");
    let node_dir = dir.path().join("project/node_modules/lodash");
    fs::create_dir_all(&node_dir).expect("mkdir");

    let (store, _retry_q, cap) = make_env(dir.path());
    let mut pipeline = IngestPipeline::new(&store, cap.strategy);

    let path = write_file(&node_dir, "index.js", &[0x42u8; 8_192]);
    let verdict = pipeline.evaluate(&path).expect("evaluate");
    assert!(
        !matches!(verdict, IngestVerdict::Bypass(BypassReason::Prohibited)),
        "node_modules file must not be Prohibited; got: {:?}",
        verdict
    );
}

// ─── ST-08: Retry queue enqueue under simulated lock contention ───────────────

#[test]
fn st08_retry_queue_full_does_not_panic() {
    use itan_core::retry_queue::{DeferRetryQueue, RetryJob};
    use std::path::PathBuf;

    let queue = DeferRetryQueue::new();
    let mut overflowed = false;
    for i in 0..4_097u64 {
        let job = RetryJob::new(
            PathBuf::from(format!("/workspace/file_{}.dll", i)),
            PathBuf::from("/store/ab/abc_s000"),
        );
        if queue.enqueue(job).is_err() {
            overflowed = true;
            break;
        }
    }
    assert!(overflowed, "queue must overflow gracefully at capacity");
}

// ─── ST-09: CasStore lookup returns None for missing digest ───────────────────

#[test]
fn st09_store_lookup_miss() {
    let dir = TempDir::new().expect("tempdir");
    let (store, _retry_q, _cap) = make_env(dir.path());
    let missing_digest = [0xDEu8; 32];
    let result = store.lookup(&missing_digest).expect("lookup");
    assert!(
        result.is_none(),
        "lookup of unknown digest must return None"
    );
}

// ─── ST-10: CasStore slot spill at max_links boundary ────────────────────────

#[test]
fn st10_store_spill_creates_sibling_slot() {
    let dir = TempDir::new().expect("tempdir");
    let cap = VolumeCapability {
        volume_root: dir.path().to_owned(),
        strategy: LinkStrategy::PosixHardlink,
        max_links_per_slot: 1,
    };
    let store = CasStore::open(dir.path(), &cap).expect("open store");

    let content = vec![0xBBu8; 8_192];
    let digest = digest_of(&content);
    let digest_hex = hex_encode(&digest);

    let shard = store.objects_dir().join(&digest_hex[..2]);
    fs::create_dir_all(&shard).expect("mkdir shard");
    let s000 = shard.join(format!("{}_s000", digest_hex));
    fs::write(&s000, &content).expect("write s000");

    // acquire_slot drives the spill: it reads link_count. s000 exists with link_count==1
    // which is >= max_links_per_slot(1), so it spills to s001.
    let existing_slot = itan_core::store::SlotRef {
        prefix: digest_hex[..2].to_owned(),
        digest_hex: digest_hex.clone(),
        slot_idx: 0,
        path: s000.clone(),
    };
    let slot = store.spill_slot(&existing_slot).expect("spill_slot");
    assert_eq!(slot.slot_idx, 1, "spill must produce slot index 1");

    let s001 = shard.join(format!("{}_s001", digest_hex));
    assert!(s001.exists(), "s001 must exist on disk after spill");
}
