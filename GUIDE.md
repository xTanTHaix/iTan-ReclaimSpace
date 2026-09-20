# iTan-ReclaimSpace v1.0 — User Guide

> **Autonomous Content-Addressable Hardlink & Storage Reclamation Engine**

---

## Table of Contents

1. [Overview](#overview)
2. [How It Works](#how-it-works)
3. [Installation & Build](#installation--build)
4. [CLI Reference](#cli-reference)
5. [Subcommand Details](#subcommand-details)
   - [probe](#probe)
   - [recover](#recover)
   - [ingest](#ingest)
   - [gc](#gc)
   - [status](#status)
6. [Storage Layout](#storage-layout)
7. [File Eligibility Rules](#file-eligibility-rules)
8. [Operating System Behaviour](#operating-system-behaviour)
9. [Fault Tolerance & Crash Recovery](#fault-tolerance--crash-recovery)
10. [Running the Test Suites](#running-the-test-suites)
11. [Troubleshooting](#troubleshooting)

---

## Overview

**iTan-ReclaimSpace** eliminates redundant copies of identical binary artifacts that accumulate
across multiple build workspaces on the same volume — shared libraries, compiled DLLs, AI model
weights, static assets, and `node_modules` packages.

Instead of keeping N identical byte-for-byte copies on disk, the engine replaces all but one
with a **hardlink** pointing at a single master copy stored in a per-volume Content-Addressable
Store (CAS). On filesystems that support block-level Copy-on-Write (ReFS Dev Drive, Btrfs, APFS),
it uses block cloning instead of hardlinks for even lower metadata overhead.

**Key guarantees:**

| Property | Detail |
|---|---|
| Atomicity | No window of data loss during replacement — `renameat2`/`MoveFileExW` at the kernel level |
| Safety | Master objects are read-only (`0444` / `FILE_ATTRIBUTE_READONLY`); compilers cannot silently corrupt shared data |
| Idempotency | Running `ingest` twice on the same workspace is safe — already-linked files are skipped at the inode-cache stage |
| Cross-project isolation | Each volume gets its own independent CAS; hardlinks never cross volume or mount-point boundaries |

---

## How It Works

```
Workspace files
      │
      ▼
[Tier 0] Metadata gate
  ├─ Size < 4 KiB           → skip (too small to save meaningfully)
  ├─ Extension prohibited   → skip (build-system volatile files)
  ├─ Inode already in CAS   → skip (already deduplicated)
  └─ Pass
      │
      ▼
[Tier 1] Boundary sparse hash
  ├─ 4 KiB–8 KiB files      → hash full content
  ├─ ≥ 8 KiB files          → hash first 4 KiB + last 4 KiB (fast guard)
  ├─ Hash differs from CAS  → skip (unique content)
  └─ Hash matches
      │
      ▼
[Tier 2] Full BLAKE3 digest
  └─ Stream entire file in 64 KiB blocks
      │
      ├─ Digest already in store → hardlink/clone workspace copy → master slot
      └─ Digest new             → publish as new master → link workspace copy
```

The two-phase hashing (boundary guard + full digest) cuts expensive full-file reads to near zero
for files that are merely similar but not identical.

---

## Installation & Build

### Prerequisites

| Requirement | Version |
|---|---|
| Rust toolchain | 1.85 or later (stable) |
| `just` task runner | any recent version (optional but recommended) |

### Build steps

```bash
# Clone or navigate to the workspace
cd L:\iTan-ReclaimSpace   # or wherever you placed it

# Debug build
cargo build

# Optimised release build
cargo build --release

# The CLI binary will be at:
#   target/release/itan          (Linux / macOS)
#   target\release\itan.exe      (Windows)
```

### Enable platform-specific acceleration

```bash
# Windows NTFS/ReFS — enables Win32 MoveFileExW, USN Journal, FSCTL block-clone
cargo build --release -p itan-cli --features windows-ntfs

# Linux Btrfs/XFS CoW — enables FICLONE ioctl and renameat2
cargo build --release -p itan-cli --features linux-cow

# macOS APFS clonefile(2)
cargo build --release -p itan-cli --features macos-apfs
```

> **Note:** Without a platform feature flag the engine falls back to POSIX hardlinks, which
> work on any filesystem that supports them (ext4, NTFS, APFS, etc.).

---

## CLI Reference

```
itan [OPTIONS] <COMMAND>

COMMANDS:
  probe    Probe a volume and print the detected deduplication strategy
  recover  Run the Startup Sweeper to clean up stale staging files after a crash
  ingest   Scan a workspace directory and deduplicate eligible files using the CAS store
  gc       Run a full GarbageCollector sweep cycle (objects → quarantine → purge)
  status   Print CAS store statistics for a volume
  help     Print help or the help of the given subcommand(s)

OPTIONS:
      --log-level <LOG_LEVEL>  Logging verbosity: trace | debug | info | warn | error [default: info]
  -h, --help                   Print help
  -V, --version                Print version
```

All subcommands accept `-h` / `--help` for detailed per-subcommand usage.

---

## Subcommand Details

### `probe`

Inspects the target volume and reports which link strategy will be selected by the capability engine.

```bash
itan probe <VOLUME_ROOT>
```

**Arguments:**

| Parameter | Type | Description |
|---|---|---|
| `<VOLUME_ROOT>` | Positional path | Volume root directory to probe (e.g. `C:\`, `D:\`, `/`, `/mnt/data`) |

**Example:**

```
$ itan probe C:\

Volume root   : C:\
Strategy      : NtfsHardlink
Max links/slot: 1000
```

**Possible strategies:**

| Strategy | When selected |
|---|---|
| `RefsBlockClone` | Windows ReFS / Dev Drive with `FILE_SUPPORTS_BLOCK_REFCOUNTING` + dry-run success |
| `NtfsHardlink` | Windows NTFS (fallback / default on Windows) |
| `BtrfsReflink` | Linux, FICLONE ioctl succeeds on a Btrfs/XFS volume |
| `ApfsClonefile` | macOS APFS, `clonefile(2)` succeeds |
| `PosixHardlink` | Any POSIX filesystem as universal fallback |

---

### `recover`

Cleans up `.stage` temporary files that were left behind if a previous `ingest` run crashed
or was terminated unexpectedly.

```bash
itan recover <VOLUME_ROOT>
```

**Arguments:**

| Parameter | Type | Description |
|---|---|---|
| `<VOLUME_ROOT>` | Positional path | Volume root containing `.itan_store/` |

**Example:**

```
$ itan recover D:\

Removed 3 stale staging file(s), freed 13002342 bytes
```

> **When to run:** You do not normally need to call this manually. The `ingest` command
> automatically runs the startup sweep protocol before processing any files. Call `recover` explicitly
> only if you want to reclaim disk space immediately after an interrupted run.

---

### `ingest`

Scans a workspace directory, computes BLAKE3 digests for eligible files, and replaces
duplicates with hardlinks (or block-clones) pointing at master objects in the CAS store.

```bash
itan ingest <WORKSPACE_DIR> [OPTIONS]
```

**Arguments & Options:**

| Flag | Type | Description |
|---|---|---|
| `<WORKSPACE_DIR>` | Positional path | Directory tree to scan and deduplicate |
| `--store-volume <PATH>` | Optional named | Volume root where `.itan_store/` resides (defaults to workspace root) |
| `--workers <N>` | Optional named | Concurrency worker threads (defaults to logical CPU count) |
| `--dry-run` | Flag | Preview what would happen without modifying any files |

**Example:**

```
$ itan ingest D:\projects\my-app

Ingest complete:
  Total files scanned : 14372
  Linked (deduped)    : 4247
  New masters added   : 284
  Skipped             : 9841
  Bytes saved         : 2480332800
```

> **Safety:** `ingest` never deletes data. It replaces workspace copies with hardlinks to
> a read-only master. If a compiler later rebuilds a file, the OS break-on-write behaviour
> ensures the new output goes to a fresh inode — the master copy in the CAS is unaffected.

---

### `gc`

Runs the two-stage quarantine garbage collector to reclaim CAS storage from objects that
no workspace references any longer ($N_{\text{link}} == 1$).

```bash
itan gc <VOLUME_ROOT>
```

**Arguments:**

| Parameter | Type | Description |
|---|---|---|
| `<VOLUME_ROOT>` | Positional path | Volume root containing `.itan_store/` |

**GC state machine:**

```
[Active]  objects/
  │  link_count == 1 (no workspace references)
  ▼
[Quarantine]  quarantine/<timestamp>_<hash>_s000
  │  if new hardlink arrives before cooldown → resurrected back to [Active]
  │  if cooldown elapsed (600s) and link_count still == 1
  ▼
[Purge]  unlink() — inode released, disk space reclaimed
```

**Example:**

```
$ itan gc D:\

Objects sweep: quarantined=12
Quarantine sweep: purged=8, resurrected=4, bytes_freed=881852416
```

> **Safety margin:** The 10-minute cooldown (default) protects objects that are in-flight —
> i.e., a parallel `ingest` worker has computed the digest but has not yet created the
> hardlink.

---

### `status`

Prints a summary of the CAS store on a given volume without modifying anything.

```bash
itan status <VOLUME_ROOT>
```

**Arguments:**

| Parameter | Type | Description |
|---|---|---|
| `<VOLUME_ROOT>` | Positional path | Volume root containing `.itan_store/` |

**Example output:**

```
$ itan status D:\

CAS Store: D:\.itan_store
  Master slots   : 1284
  Objects bytes  : 20078854144
  Quarantine     : 0 entries
  Staging (tmp)  : 0 entries
```

---

## Storage Layout

Every volume that has been processed by `ingest` will contain a hidden `.itan_store/` directory
at its root:

```
<Volume Root>/
└── .itan_store/
    ├── tmp/                  ← Staging area for new master objects (automatically cleaned)
    ├── quarantine/           ← GC holding buffer (objects awaiting permanent deletion)
    └── objects/              ← The CAS master pool
        ├── 00/
        ├── 01/
        │   ...
        └── ff/               ← 256 shards, keyed by the first two hex chars of the BLAKE3 digest
            ├── <hash>_s000   ← Master slot 0 (up to 1,000 hardlinks on NTFS)
            └── <hash>_s001   ← Spill slot 1 (created when slot 0 hits the link ceiling)
```

**Do not manually edit** `.itan_store/`. All objects inside are read-only by design. Moving,
renaming, or deleting them directly will leave dangling hardlinks in your workspaces.

### Link ceiling and slot spilling

NTFS limits a single file to **1,024 hardlinks**. The engine conservatively caps each slot
at **1,000 links** to stay well below this boundary. When a slot reaches the cap:

1. A new sibling slot (`_s001`, `_s002`, …) is created as a separate inode with identical
   content.
2. Subsequent workspaces receive hardlinks to the new slot.
3. All slots for the same digest refer to the same bytes — which slot a workspace points to
   is transparent to users and compilers.

---

## File Eligibility Rules

The engine automatically classifies files before touching them.

### ✅ Eligible (will be deduplicated)

| Category | Examples |
|---|---|
| Static libraries | `.a`, `.lib` |
| Shared / dynamic libraries | `.so`, `.dll`, `.dylib` |
| Node.js package modules | Any file inside `node_modules/` |
| Static assets | `.png`, `.jpg`, `.wav`, `.mp3` |
| AI model weights & data | `.gguf`, `.safetensors`, `.bin` |
| Other whitelisted formats | `.wasm`, `.nupkg`, `.vsix`, … |

> Files must also be **at least 4,096 bytes** to pass the size gate.

### ❌ Ineligible (always skipped)

| Category | Examples | Reason |
|---|---|---|
| Compiler intermediate objects | `.o`, `.obj` | Overwritten per build; timestamps are critical |
| Incremental build state | `.pdb`, `.fingerprint`, `.tsbuildinfo` | Mutated by toolchain |
| Dependency tree maps | `.d`, `compile_commands.json` | Path-specific; cannot be shared |
| Files smaller than 4 KiB | — | Savings are negligible |

---

## Operating System Behaviour

### Windows

- **NTFS**: Uses `CreateHardLinkW` + `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)` for atomic in-place replacement.
- **ReFS / Dev Drive** (with `--features windows-ntfs`): Uses `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (block-clone). No inode sharing — each workspace has its own inode, but disk blocks are shared until either side writes.
- **File locks**: If a file is locked by another process (`ERROR_SHARING_VIOLATION`), the job is placed in a **Defer Retry Queue** with exponential back-off and retried automatically. The run does not fail.
- **USN Journal** (with `--features windows-ntfs`): Delta scanning via `FSCTL_READ_USN_JOURNAL` for instant change detection without full-tree traversal.

### Linux

- **Btrfs / XFS** (with `--features linux-cow`): Uses `ioctl(FICLONE)` for reflinks. Falls back to hardlinks if the ioctl returns `EOPNOTSUPP`.
- **ext4 / legacy**: Uses POSIX `link(2)` + `renameat2(RENAME_NOREPLACE)` for atomic publish.
- **I/O hint**: `posix_fadvise(POSIX_FADV_SEQUENTIAL)` is set during Tier 2 streaming to maximise kernel readahead.

### macOS

- **APFS** (with `--features macos-apfs`): Uses `clonefile(2)`. Falls back to hardlinks if unavailable.

---

## Fault Tolerance & Crash Recovery

| Failure scenario | Risk | Protection mechanism |
|---|---|---|
| Power loss during atomic replace | Source file might be lost | `MoveFileExW` / `renameat2` operate at the metadata-pointer level; the original file survives intact if the operation is incomplete |
| Crash during staging (`tmp/`) | Orphaned `.stage` files | **Startup Sweeper**: on the next `ingest` (or explicit `recover`), files older than the threshold are automatically deleted |
| Two workers ingesting the same content simultaneously | Two processes try to publish the same master object | The first `renameat2(RENAME_NOREPLACE)` wins; the second detects the collision, discards its staging file, and links to the existing master |
| Compiler/user overwrites a workspace file | Shared inode contents corrupted | **Read-Only invariant** (`0444` / `FILE_ATTRIBUTE_READONLY`): the OS rejects write-open attempts; toolchains must unlink and create a new file (break-on-write), leaving the master untouched |
| GC races with a late-arriving hardlink | Good object deleted while still in use | Two-stage quarantine + 10-minute cooldown + re-validation of `link_count` before unlink |

---

## Running the Test Suites

### Unit tests (fast, in-memory)

```bash
cargo test --lib
# Expected Windows : 68 passed, 0 failed
# Expected Linux   : 72 passed, 0 failed (includes 4 POSIX mode permission tests)
```

### Street tests (real on-disk integration)

```bash
cargo test -p itan-core --test street -- --test-threads=1
# Expected: 10 passed, 0 failed (both platforms)
```

Street tests create real files in `tempfile::TempDir` and exercise the full pipeline
end-to-end: probe → store → ingest → link → GC → recover.

### Cluster tests (multi-thread concurrency)

```bash
cargo test -p itan-core --test cluster -- --test-threads=4
# Expected Windows : 4 passed, 0 failed  (CT-03 skipped: Unix-only)
# Expected Linux   : 5 passed, 0 failed  (CT-03 GC resurrection race included)
```

Cluster tests spawn 4–8 threads simultaneously ingesting overlapping content, stress-testing
publish races, slot-spill races, GC resurrection, and retry-queue drain.

### All tests in one command

```bash
# Windows:
cargo test --all
# Expected: 83 passed, 0 failed

# Linux (WSL or native):
cargo test --all
# Expected: 88 passed, 0 failed
```

> **Tip for WSL:** Set `CARGO_TARGET_DIR=/tmp/itan_target` when compiling in WSL over Windows mounts
> to build on native Linux ext4 for maximum compile and execution throughput.

### Using `just`

If you have the `just` task runner installed:

```bash
just test      # cargo test --all
just street    # cargo test -p itan-core --test street
just cluster   # cargo test -p itan-core --test cluster
just bench     # cargo bench (Criterion benchmarks)
just ci        # full lint + format check + test (mirrors CI pipeline)
```

---

## Troubleshooting

### `PermissionDenied` when running `ingest` on Windows

**Cause:** The target file is locked by another process (e.g. an IDE, antivirus, or running
application).

**Solution:** The engine automatically defers locked files to the retry queue and will retry
with exponential back-off. If the lock persists, close the application holding the file and
re-run `ingest`.

### `cargo build` fails: "windows-sys not found"

**Cause:** You are building with `--features windows-ntfs` on a non-Windows host.

**Solution:** The `windows-ntfs` feature is only for Windows targets. Build without it on
Linux/macOS:

```bash
cargo build --features linux-cow    # Linux
cargo build --features macos-apfs   # macOS
cargo build                         # any platform, POSIX hardlinks
```

### Objects directory keeps growing after GC

**Cause:** The default cooldown is 10 minutes (600 s). Objects quarantined less than 10
minutes ago will not be purged yet.

**Solution:** Run `gc` again after the cooldown window, or reduce it with `--cooldown-secs 60`
in a single-user environment where no parallel ingestion is running.

### `cargo check` reports warnings as errors

**Cause:** The project enforces zero warnings via `RUSTFLAGS = "-D warnings"` in
`.cargo/config.toml`.

**Solution:** This is intentional. Fix the warning before committing. Common false positives
on Windows are `unused import` inside `#[cfg(unix)]` blocks — make sure platform-specific
imports are gated with the correct `#[cfg]` attribute.

### Store takes more disk space than expected

**Cause:** The CAS stores one master copy of each unique file. If your workspaces contain
many _nearly_ identical (but not byte-for-byte identical) files, they will not be
deduplicated — each creates its own master slot.

**Solution:** Run `itan status --root <PATH>` to see how many unique objects are stored.
Use `itan gc --root <PATH>` to purge objects no longer referenced by any workspace.

---

*Documentation for iTan-ReclaimSpace v1.0 — generated from the engineering specification and
empirically verified against the live codebase.*
