//! Cryptographic digest primitives used by the Multi-Tier Waterfall Ingestion pipeline.
//!
//! # Tier 1 — Boundary Hash (Fast Guard)
//!
//! For files between 4 096 and 8 191 bytes the entire file is read (they fit in two
//! aligned 4 KiB pages).  For files ≥ 8 192 bytes only the first and last 4 KiB are
//! hashed — this catches the overwhelming majority of distinct files with minimal I/O.
//!
//! # Tier 2 — Full CAS Digest (Content-Addressable Key)
//!
//! The whole file is streamed through 64 KiB blocks using `blake3`.  On Linux the file
//! descriptor is pre-advised with `POSIX_FADV_SEQUENTIAL`; on Windows the file is opened
//! with `FILE_FLAG_SEQUENTIAL_SCAN` to hint the cache manager for double-buffered
//! read-ahead, reducing latency on large blobs.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use thiserror::Error;

use crate::capability::LinkStrategy;

/// Errors that can arise from digest computation.
#[derive(Debug, Error)]
pub enum DigestError {
    #[error("cannot open file '{path}': {source}")]
    Open {
        path: std::path::PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("I/O error reading '{path}': {source}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("cannot seek within '{path}': {source}")]
    Seek {
        path: std::path::PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Size of the I/O buffer used for both boundary reads and full streaming (§3.1).
const BLOCK_SIZE: usize = 65_536; // 64 KiB

/// Minimum file size (inclusive) required to enter the ingestion pipeline (§3 Tier 0).
pub const MIN_FILE_SIZE: u64 = 4_096;

/// Boundary below which files are read entirely for the Tier 1 hash (§3 Tier 1).
const BOUNDARY_FULL_READ_LIMIT: u64 = 8_192;

/// Size of the head and tail regions sampled in Tier 1 for files ≥ 8 KiB.
const BOUNDARY_SAMPLE: usize = 4_096;

// ─── Tier 1 — Boundary Hash ───────────────────────────────────────────────────

/// Computes the Tier 1 boundary hash for `path`.
///
/// - Files in the range `[4 096, 8 191]` bytes: hash the entire content.
/// - Files ≥ 8 192 bytes: hash only the first 4 KiB + last 4 KiB concatenated.
///
/// The returned digest is a 32-byte blake3 hash.  Two files with identical boundary
/// hashes are **candidates** for full comparison; distinct boundary hashes guarantee
/// the files are distinct (no false negatives are possible with a 256-bit hash at this
/// scale).
///
/// # Errors
///
/// Returns [`DigestError`] on file open, seek, or read failures.
pub fn boundary_hash(path: &Path) -> Result<[u8; 32], DigestError> {
    let file_len = fs::metadata(path)
        .map_err(|source| DigestError::Open {
            path: path.to_owned(),
            source,
        })?
        .len();

    let mut file = open_file_for_read(path)?;
    let mut hasher = blake3::Hasher::new();

    if file_len < BOUNDARY_FULL_READ_LIMIT {
        // Small file: read the whole thing.
        let mut buf = Vec::with_capacity(file_len as usize);
        file.read_to_end(&mut buf)
            .map_err(|source| DigestError::Read {
                path: path.to_owned(),
                source,
            })?;
        hasher.update(&buf);
    } else {
        // Large file: sample head and tail windows.
        let mut head = [0u8; BOUNDARY_SAMPLE];
        file.read_exact(&mut head)
            .map_err(|source| DigestError::Read {
                path: path.to_owned(),
                source,
            })?;

        let tail_offset = file_len - BOUNDARY_SAMPLE as u64;
        file.seek(SeekFrom::Start(tail_offset))
            .map_err(|source| DigestError::Seek {
                path: path.to_owned(),
                source,
            })?;
        let mut tail = [0u8; BOUNDARY_SAMPLE];
        file.read_exact(&mut tail)
            .map_err(|source| DigestError::Read {
                path: path.to_owned(),
                source,
            })?;

        hasher.update(&head);
        hasher.update(&tail);
    }

    Ok(*hasher.finalize().as_bytes())
}

// ─── Tier 2 — Full CAS Digest ─────────────────────────────────────────────────

/// Computes the full blake3 CAS digest for `path` by streaming the entire file
/// through 64 KiB blocks.
///
/// The `strategy` parameter enables OS-specific sequential I/O hints:
/// - [`LinkStrategy::PosixHardlink`] / [`LinkStrategy::BtrfsReflink`]: applies
///   `POSIX_FADV_SEQUENTIAL` on Linux to expand the kernel read-ahead window.
/// - [`LinkStrategy::NtfsHardlink`] / [`LinkStrategy::RefsBlockClone`]: opens the
///   file with `FILE_FLAG_SEQUENTIAL_SCAN` on Windows for cache manager double-buffering.
///
/// # Errors
///
/// Returns [`DigestError`] on file open or read failures.
pub fn full_cas_digest(path: &Path, strategy: LinkStrategy) -> Result<[u8; 32], DigestError> {
    let mut file = open_file_with_sequential_hint(path, strategy)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; BLOCK_SIZE];

    loop {
        let n = file.read(&mut buf).map_err(|source| DigestError::Read {
            path: path.to_owned(),
            source,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(*hasher.finalize().as_bytes())
}

// ─── Platform-specific file openers ───────────────────────────────────────────

/// Opens `path` for sequential reads without any OS-level prefetch hint.
/// Used exclusively by the Tier 1 boundary hash path.
fn open_file_for_read(path: &Path) -> Result<fs::File, DigestError> {
    fs::File::open(path).map_err(|source| DigestError::Open {
        path: path.to_owned(),
        source,
    })
}

/// Opens `path` for reading with the appropriate sequential-scan hint for the given
/// `strategy`. Falls back to a plain `fs::File::open` if hint application is not
/// supported on the current platform.
fn open_file_with_sequential_hint(
    path: &Path,
    strategy: LinkStrategy,
) -> Result<fs::File, DigestError> {
    #[cfg(target_os = "windows")]
    {
        open_file_sequential_windows(path, strategy)
    }

    #[cfg(target_os = "linux")]
    {
        open_file_sequential_linux(path, strategy)
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = strategy;
        open_file_for_read(path)
    }
}

// ─── Windows sequential-scan opener ──────────────────────────────────────────

#[cfg(target_os = "windows")]
fn open_file_sequential_windows(
    path: &Path,
    strategy: LinkStrategy,
) -> Result<fs::File, DigestError> {
    // FILE_FLAG_SEQUENTIAL_SCAN is only beneficial for strategies that read whole files
    // sequentially.  CoW strategies don't change the read pattern but the flag is harmless.
    let _ = strategy; // Strategy selection preserved for future fine-grained tuning.

    use std::os::windows::fs::OpenOptionsExt;
    // FILE_FLAG_SEQUENTIAL_SCAN = 0x08000000 — instructs the cache manager to
    // pre-fetch clusters ahead of the current read position (double-buffering).
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN)
        .open(path)
        .map_err(|source| DigestError::Open {
            path: path.to_owned(),
            source,
        })
}

// ─── Linux sequential-scan opener ────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn open_file_sequential_linux(
    path: &Path,
    strategy: LinkStrategy,
) -> Result<fs::File, DigestError> {
    let file = open_file_for_read(path)?;

    // POSIX_FADV_SEQUENTIAL hints the kernel to increase the read-ahead window for
    // this file descriptor.  The call is advisory — kernel may silently ignore it.
    // We log a debug warning if it fails rather than propagating an error.
    #[cfg(feature = "linux-cow")]
    {
        use std::os::unix::io::AsRawFd;
        // posix_fadvise(fd, offset=0, len=0 means "entire file", POSIX_FADV_SEQUENTIAL=2)
        let ret =
            unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
        if ret != 0 {
            log::debug!(
                "posix_fadvise SEQUENTIAL failed for '{}': errno={}",
                path.display(),
                ret
            );
        }
        let _ = strategy;
    }

    #[cfg(not(feature = "linux-cow"))]
    let _ = (file, strategy, path);

    #[cfg(feature = "linux-cow")]
    return Ok(file);
    #[cfg(not(feature = "linux-cow"))]
    open_file_for_read(path)
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // P3-U07: full_cas_digest on a small file must produce a deterministic blake3 hash.
    #[test]
    fn test_full_cas_digest_deterministic() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("test.bin");
        let content = vec![0x42u8; MIN_FILE_SIZE as usize];
        fs::write(&path, &content).expect("write");

        let d1 = full_cas_digest(&path, LinkStrategy::PosixHardlink).expect("digest");
        let d2 = full_cas_digest(&path, LinkStrategy::PosixHardlink).expect("digest again");
        assert_eq!(d1, d2, "digest must be deterministic");
    }

    // P3-U07b: two files with different content must produce different CAS digests.
    #[test]
    fn test_full_cas_digest_different_content() {
        let dir = TempDir::new().expect("tempdir");
        let p1 = dir.path().join("a.bin");
        let p2 = dir.path().join("b.bin");
        fs::write(&p1, vec![0xAAu8; MIN_FILE_SIZE as usize]).expect("write a");
        fs::write(&p2, vec![0xBBu8; MIN_FILE_SIZE as usize]).expect("write b");

        let d1 = full_cas_digest(&p1, LinkStrategy::PosixHardlink).expect("digest a");
        let d2 = full_cas_digest(&p2, LinkStrategy::PosixHardlink).expect("digest b");
        assert_ne!(d1, d2, "different content must produce different digests");
    }

    // P3-U04: boundary_hash on a file in [4096, 8191] reads it entirely.
    #[test]
    fn test_boundary_hash_small_file_full_read() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("small.bin");
        // 5 000 bytes — below BOUNDARY_FULL_READ_LIMIT, above MIN_FILE_SIZE.
        let content = vec![0xCCu8; 5_000];
        fs::write(&path, &content).expect("write");

        let hash = boundary_hash(&path).expect("boundary_hash");
        // Must equal the blake3 of the full content.
        let expected = *blake3::hash(&content).as_bytes();
        assert_eq!(hash, expected);
    }

    // P3-U05: boundary_hash on a file >= 8192 reads only head+tail.
    #[test]
    fn test_boundary_hash_large_file_head_tail() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("large.bin");
        // 16 KiB: distinct head (0xAA), middle (0x00), tail (0xBB).
        let mut content = vec![0xAAu8; BOUNDARY_SAMPLE];
        content.extend(vec![0x00u8; BOUNDARY_SAMPLE * 2]);
        content.extend(vec![0xBBu8; BOUNDARY_SAMPLE]);
        fs::write(&path, &content).expect("write");

        let hash = boundary_hash(&path).expect("boundary_hash");

        // Manually compute expected: blake3(head || tail).
        let mut hasher = blake3::Hasher::new();
        hasher.update(&content[..BOUNDARY_SAMPLE]);
        hasher.update(&content[content.len() - BOUNDARY_SAMPLE..]);
        let expected = *hasher.finalize().as_bytes();

        assert_eq!(hash, expected);
    }

    // P3-U06: two large files that differ only in the middle must have identical boundary hashes.
    #[test]
    fn test_boundary_hash_middle_diff_same_boundary() {
        let dir = TempDir::new().expect("tempdir");
        let make = |mid: u8| {
            let mut v = vec![0xAAu8; BOUNDARY_SAMPLE];
            v.extend(vec![mid; BOUNDARY_SAMPLE * 2]);
            v.extend(vec![0xBBu8; BOUNDARY_SAMPLE]);
            v
        };
        let p1 = dir.path().join("f1.bin");
        let p2 = dir.path().join("f2.bin");
        fs::write(&p1, make(0x11)).expect("write f1");
        fs::write(&p2, make(0x22)).expect("write f2");

        let h1 = boundary_hash(&p1).expect("h1");
        let h2 = boundary_hash(&p2).expect("h2");
        assert_eq!(
            h1, h2,
            "files differing only in the middle section must share a boundary hash"
        );
    }

    // P3-U08: full digest on a file that differs in the middle must differ from boundary hash collision.
    #[test]
    fn test_full_digest_catches_middle_difference() {
        let dir = TempDir::new().expect("tempdir");
        let make = |mid: u8| {
            let mut v = vec![0xAAu8; BOUNDARY_SAMPLE];
            v.extend(vec![mid; BOUNDARY_SAMPLE * 2]);
            v.extend(vec![0xBBu8; BOUNDARY_SAMPLE]);
            v
        };
        let p1 = dir.path().join("f1.bin");
        let p2 = dir.path().join("f2.bin");
        fs::write(&p1, make(0x11)).expect("write f1");
        fs::write(&p2, make(0x22)).expect("write f2");

        let d1 = full_cas_digest(&p1, LinkStrategy::PosixHardlink).expect("d1");
        let d2 = full_cas_digest(&p2, LinkStrategy::PosixHardlink).expect("d2");
        assert_ne!(
            d1, d2,
            "full digest must distinguish files that share a boundary hash"
        );
    }

    // Error path: digest on nonexistent path must return DigestError::Open.
    #[test]
    fn test_full_cas_digest_missing_file() {
        let result = full_cas_digest(
            Path::new("/no/such/file/itan_test_xyz.bin"),
            LinkStrategy::PosixHardlink,
        );
        assert!(
            matches!(result, Err(DigestError::Open { .. })),
            "expected Open error, got: {:?}",
            result
        );
    }
}
