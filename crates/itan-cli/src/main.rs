//! iTan-ReclaimSpace CLI — command-line front-end.
//!
//! # Commands
//!
//! ```text
//! itan probe    <volume-root>              Detect filesystem capability and print strategy
//! itan recover  <volume-root>              Run StartupSweeper (crash recovery)
//! itan ingest   <workspace> --store <vol>  Scan workspace and deduplicate files
//! itan gc       <volume-root>              Run QuarantineGC sweep cycle
//! itan status   <volume-root>              Print CAS store statistics
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use itan_core::{
    capability::probe_volume, gc::QuarantineGc, ingest::IngestPipeline,
    linkage::AtomicLinkagePipeline, recovery::StartupSweeper, retry_queue::DeferRetryQueue,
    store::CasStore,
};

// ─── CLI definition ───────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "itan",
    version = env!("CARGO_PKG_VERSION"),
    about = "Autonomous Content-Addressable Hardlink & Storage Reclamation Engine"
)]
struct Cli {
    /// Logging verbosity: trace | debug | info | warn | error
    #[arg(long, default_value = "info", global = true)]
    log_level: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Probe a volume and print the detected deduplication strategy.
    Probe {
        /// Path to the volume root (e.g. `C:\` or `/`).
        volume_root: PathBuf,
    },

    /// Run the Startup Sweeper to clean up stale staging files after a crash.
    Recover {
        /// Path to the volume root.
        volume_root: PathBuf,
    },

    /// Scan a workspace directory and deduplicate eligible files using the CAS store.
    Ingest {
        /// Workspace directory to scan.
        workspace: PathBuf,
        /// Volume root where the CAS store resides.
        /// Defaults to the workspace path's root volume.
        #[arg(long)]
        store_volume: Option<PathBuf>,
        /// Number of worker threads (defaults to number of logical CPUs).
        #[arg(long)]
        workers: Option<usize>,
        /// Print what would happen without making any changes.
        #[arg(long)]
        dry_run: bool,
    },

    /// Run a full GarbageCollector sweep cycle (objects → quarantine → purge).
    Gc {
        /// Path to the volume root.
        volume_root: PathBuf,
    },

    /// Print CAS store statistics for a volume.
    Status {
        /// Path to the volume root.
        volume_root: PathBuf,
    },
}

// ─── Entry point ──────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    // Initialise env_logger with the requested verbosity.
    let log_filter = format!("itan={}", cli.log_level);
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&log_filter))
        .format_timestamp_secs()
        .init();

    let exit_code = match run(cli.command) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    };

    std::process::exit(exit_code);
}

// ─── Command dispatch ─────────────────────────────────────────────────────────

fn run(cmd: Command) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Command::Probe { volume_root } => cmd_probe(&volume_root),
        Command::Recover { volume_root } => cmd_recover(&volume_root),
        Command::Ingest {
            workspace,
            store_volume,
            workers,
            dry_run,
        } => cmd_ingest(&workspace, store_volume.as_deref(), workers, dry_run),
        Command::Gc { volume_root } => cmd_gc(&volume_root),
        Command::Status { volume_root } => cmd_status(&volume_root),
    }
}

// ─── Command implementations ──────────────────────────────────────────────────

fn cmd_probe(volume_root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let cap = probe_volume(volume_root)?;
    println!("Volume root   : {}", cap.volume_root.display());
    println!("Strategy      : {:?}", cap.strategy);
    println!("Max links/slot: {}", cap.max_links_per_slot);
    Ok(())
}

fn cmd_recover(volume_root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let cap = probe_volume(volume_root)?;
    let store = Arc::new(CasStore::open(volume_root, &cap)?);
    let sweeper = StartupSweeper::with_default_threshold(Arc::clone(&store));

    let report = sweeper.sweep_stale_tmp();
    println!(
        "Removed {} stale staging file(s), freed {} bytes",
        report.removed_count, report.bytes_freed
    );
    for err in &report.errors {
        eprintln!("warning: {}", err);
    }
    Ok(())
}

fn cmd_ingest(
    workspace: &std::path::Path,
    store_volume: Option<&std::path::Path>,
    _workers: Option<usize>,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let vol_root = store_volume.unwrap_or(workspace);
    let cap = probe_volume(vol_root)?;
    let store = Arc::new(CasStore::open(vol_root, &cap)?);

    // Run crash recovery before ingestion begins (§7 Startup Sweeper Protocol).
    let sweeper = StartupSweeper::with_default_threshold(Arc::clone(&store));
    let sweep_report = sweeper.sweep_stale_tmp();
    if sweep_report.removed_count > 0 {
        log::info!(
            "Startup sweep removed {} stale staging file(s)",
            sweep_report.removed_count
        );
    }

    let retry_queue = Arc::new(DeferRetryQueue::new());
    let mut pipeline = IngestPipeline::new(&store, cap.strategy);
    let link_pipeline =
        AtomicLinkagePipeline::new(Arc::clone(&store), Arc::clone(&retry_queue), cap.strategy);

    let mut total_files: u64 = 0;
    let mut total_saved: u64 = 0;
    let mut linked: u64 = 0;
    let mut unique: u64 = 0;
    let mut skipped: u64 = 0;

    // Recursive workspace traversal using std::fs::read_dir.
    let mut dirs_to_visit: Vec<std::path::PathBuf> = vec![workspace.to_owned()];

    while let Some(dir) = dirs_to_visit.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("Cannot read directory '{}': {}", dir.display(), e);
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip the .itan_store directory to avoid recursing into the CAS itself.
                if path.file_name().and_then(|n| n.to_str()) == Some(".itan_store") {
                    continue;
                }
                dirs_to_visit.push(path);
                continue;
            }

            total_files += 1;
            let verdict = match pipeline.evaluate(&path) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("Ingest eval error for '{}': {}", path.display(), e);
                    continue;
                }
            };

            if dry_run {
                log::info!(
                    "[dry-run] {:?}: {}",
                    verdict_label(&verdict),
                    path.display()
                );
                continue;
            }

            // Compute CAS digest only for non-bypass verdicts that need it.
            let digest_result = match &verdict {
                itan_core::ingest::IngestVerdict::Duplicate(slot) => {
                    parse_hex_digest(&slot.digest_hex)
                }
                itan_core::ingest::IngestVerdict::Unique => {
                    itan_core::digest::full_cas_digest(&path, cap.strategy)
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
                }
                itan_core::ingest::IngestVerdict::Bypass(_) => {
                    skipped += 1;
                    continue;
                }
            };

            let digest = match digest_result {
                Ok(d) => d,
                Err(e) => {
                    log::warn!("Digest error for '{}': {}", path.display(), e);
                    continue;
                }
            };

            match link_pipeline.apply(&path, verdict, &digest) {
                Ok(itan_core::linkage::LinkResult::Linked { bytes_saved, .. }) => {
                    linked += 1;
                    total_saved += bytes_saved;
                }
                Ok(itan_core::linkage::LinkResult::MasterPublished { .. }) => unique += 1,
                Ok(itan_core::linkage::LinkResult::Skipped) => skipped += 1,
                Ok(itan_core::linkage::LinkResult::Deferred) => {}
                Err(e) => log::warn!("Link error for '{}': {}", path.display(), e),
            }
        }
    }

    println!("Ingest complete:");
    println!("  Total files scanned : {}", total_files);
    println!("  Linked (deduped)    : {}", linked);
    println!("  New masters added   : {}", unique);
    println!("  Skipped             : {}", skipped);
    println!("  Bytes saved         : {}", total_saved);

    Ok(())
}

fn cmd_gc(volume_root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let cap = probe_volume(volume_root)?;
    let store = Arc::new(CasStore::open(volume_root, &cap)?);
    let gc = QuarantineGc::with_default_cooldown(Arc::clone(&store));

    let obj_stats = gc.sweep_objects()?;
    println!("Objects sweep: quarantined={}", obj_stats.quarantined);

    let quar_stats = gc.sweep_quarantine()?;
    println!(
        "Quarantine sweep: purged={}, resurrected={}, bytes_freed={}",
        quar_stats.purged, quar_stats.resurrected, quar_stats.bytes_freed
    );

    for err in obj_stats.errors.iter().chain(quar_stats.errors.iter()) {
        eprintln!("warning: {}", err);
    }
    Ok(())
}

fn cmd_status(volume_root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let store_root = volume_root.join(".itan_store");
    if !store_root.exists() {
        println!("No CAS store found at '{}'", store_root.display());
        return Ok(());
    }

    let objects_dir = store_root.join("objects");
    let (slot_count, total_bytes) = count_objects(&objects_dir);
    let quarantine_count = count_dir_entries(&store_root.join("quarantine"));
    let tmp_count = count_dir_entries(&store_root.join("tmp"));

    println!("CAS Store: {}", store_root.display());
    println!("  Master slots   : {}", slot_count);
    println!("  Objects bytes  : {}", total_bytes);
    println!("  Quarantine     : {} entries", quarantine_count);
    println!("  Staging (tmp)  : {} entries", tmp_count);
    Ok(())
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn verdict_label(verdict: &itan_core::ingest::IngestVerdict) -> &'static str {
    match verdict {
        itan_core::ingest::IngestVerdict::Duplicate(_) => "Duplicate",
        itan_core::ingest::IngestVerdict::Unique => "Unique",
        itan_core::ingest::IngestVerdict::Bypass(_) => "Bypass",
    }
}

fn parse_hex_digest(hex: &str) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    if hex.len() != 64 {
        return Err(format!("invalid digest hex length: {}", hex.len()).into());
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0]);
        let lo = hex_nibble(chunk[1]);
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
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

fn count_objects(objects_dir: &std::path::Path) -> (u64, u64) {
    let mut count = 0u64;
    let mut bytes = 0u64;

    if let Ok(shards) = std::fs::read_dir(objects_dir) {
        for shard in shards.flatten() {
            if let Ok(slots) = std::fs::read_dir(shard.path()) {
                for slot in slots.flatten() {
                    if let Ok(meta) = slot.metadata() {
                        count += 1;
                        bytes += meta.len();
                    }
                }
            }
        }
    }
    (count, bytes)
}

fn count_dir_entries(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|iter| iter.count() as u64)
        .unwrap_or(0)
}
