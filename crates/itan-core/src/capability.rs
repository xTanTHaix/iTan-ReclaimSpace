//! Volume capability probing — detects the optimal deduplication strategy for a given
//! filesystem without performing any destructive operations.
//!
//! The probe performs a live CoW dry-run (writing a 4 KiB test object into `.itan_store/tmp/`)
//! and immediately cleans up the artefact regardless of outcome. On any `EOPNOTSUPP`-equivalent
//! error the probe silently falls back to the appropriate hardlink strategy.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

// ─── Error type ──────────────────────────────────────────────────────────────

/// Errors that can occur during volume capability probing.
#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("cannot stat volume root '{path}': {source}")]
    StatFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("volume root '{0}' is not a directory")]
    NotADirectory(PathBuf),

    #[error("cannot create tmp staging directory '{path}': {source}")]
    TmpDirCreation {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("I/O error during CoW dry-run probe: {0}")]
    CowProbeIo(#[source] std::io::Error),
}

// ─── Public types ─────────────────────────────────────────────────────────────

/// The deduplication strategy selected for a specific volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStrategy {
    /// Windows NTFS — hardlink via `CreateHardLinkW`, Slot Spiller enabled (limit 1 000/slot).
    NtfsHardlink,
    /// Windows ReFS / Dev Drive — block clone via `FSCTL_DUPLICATE_EXTENTS_TO_FILE`.
    RefsBlockClone,
    /// Linux Btrfs / XFS / ZFS — reflink via `FICLONE` ioctl.
    BtrfsReflink,
    /// macOS APFS — instant metadata clone via `clonefile(2)`.
    ApfsClonefile,
    /// Linux ext4 / legacy POSIX — hardlink, limit 64 000/inode per POSIX spec.
    PosixHardlink,
}

impl LinkStrategy {
    /// The maximum number of hardlinks that may point to a single slot object before
    /// the engine must spill to a new sibling slot. CoW strategies set this to `u32::MAX`
    /// because they do not share an inode and therefore have no link-count ceiling.
    #[inline]
    pub fn max_links_per_slot(self) -> u32 {
        match self {
            // NTFS hard ceiling is 1 024; we use 1 000 as a software safety buffer
            // aligned with pnpm 12.4's approach.
            LinkStrategy::NtfsHardlink => 1_000,
            // POSIX link ceiling is 65 535 (LINK_MAX); we use 64 000 as the buffer.
            LinkStrategy::PosixHardlink => 64_000,
            // CoW strategies never share an inode — no ceiling applies.
            LinkStrategy::RefsBlockClone
            | LinkStrategy::BtrfsReflink
            | LinkStrategy::ApfsClonefile => u32::MAX,
        }
    }

    /// Returns `true` if this strategy uses shared inodes (hardlinks) rather than CoW clones.
    #[inline]
    pub fn is_hardlink(self) -> bool {
        matches!(
            self,
            LinkStrategy::NtfsHardlink | LinkStrategy::PosixHardlink
        )
    }
}

/// The result of a successful capability probe for a single volume.
#[derive(Debug, Clone)]
pub struct VolumeCapability {
    /// Absolute path to the volume root (e.g. `C:\` or `/`).
    pub volume_root: PathBuf,
    /// The deduplication strategy that will be used for this volume.
    pub strategy: LinkStrategy,
    /// Maximum hardlinks per slot — derived from `strategy.max_links_per_slot()`.
    pub max_links_per_slot: u32,
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Probes the filesystem at `root` and returns the optimal [`VolumeCapability`].
///
/// The probe:
/// 1. Validates that `root` is an accessible directory.
/// 2. Ensures `.itan_store/tmp/` exists (creating it lazily if absent).
/// 3. Attempts a live CoW dry-run by writing a 4 KiB sentinel file and invoking
///    the platform-specific CoW syscall against a duplicate target path.
/// 4. Cleans up both probe files unconditionally before returning.
///
/// # Errors
///
/// Returns [`ProbeError`] if the volume root is inaccessible or the tmp directory
/// cannot be created. CoW failures are treated as non-errors and trigger fallback.
pub fn probe_volume(root: &Path) -> Result<VolumeCapability, ProbeError> {
    // Guard: root must be a directory we can stat.
    let meta = fs::metadata(root).map_err(|source| ProbeError::StatFailed {
        path: root.to_owned(),
        source,
    })?;
    if !meta.is_dir() {
        return Err(ProbeError::NotADirectory(root.to_owned()));
    }

    let tmp_dir = root.join(".itan_store").join("tmp");
    fs::create_dir_all(&tmp_dir).map_err(|source| ProbeError::TmpDirCreation {
        path: tmp_dir.clone(),
        source,
    })?;

    let strategy = detect_strategy(root, &tmp_dir)?;

    Ok(VolumeCapability {
        volume_root: root.to_owned(),
        max_links_per_slot: strategy.max_links_per_slot(),
        strategy,
    })
}

// ─── Platform dispatch ────────────────────────────────────────────────────────

/// Selects the optimal strategy for `root` by examining the filesystem type and
/// optionally running a live CoW dry-run probe.
fn detect_strategy(root: &Path, tmp_dir: &Path) -> Result<LinkStrategy, ProbeError> {
    #[cfg(target_os = "windows")]
    {
        detect_strategy_windows(root, tmp_dir)
    }

    #[cfg(target_os = "linux")]
    {
        detect_strategy_linux(root, tmp_dir)
    }

    #[cfg(target_os = "macos")]
    {
        detect_strategy_macos(root, tmp_dir)
    }

    // Catch-all for other POSIX systems — fall back to hardlink.
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, tmp_dir); // Suppress unused warnings.
        Ok(LinkStrategy::PosixHardlink)
    }
}

// ─── Windows implementation ───────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn detect_strategy_windows(root: &Path, tmp_dir: &Path) -> Result<LinkStrategy, ProbeError> {
    // Without the windows-ntfs feature we cannot call Win32 — fall back conservatively.
    #[cfg(not(feature = "windows-ntfs"))]
    {
        let _ = (root, tmp_dir);
        Ok(LinkStrategy::NtfsHardlink)
    }

    #[cfg(feature = "windows-ntfs")]
    detect_strategy_windows_ntfs(root, tmp_dir)
}

#[cfg(all(target_os = "windows", feature = "windows-ntfs"))]
fn detect_strategy_windows_ntfs(root: &Path, tmp_dir: &Path) -> Result<LinkStrategy, ProbeError> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    // Encode the volume root path as a null-terminated wide string for Win32.
    let root_wide: Vec<u16> = OsStr::new(root)
        .encode_wide()
        .chain(std::iter::once(0u16))
        .collect();

    // GetVolumeInformationW gives us lpFileSystemNameBuffer + dwFileSystemFlags.
    let mut fs_name_buf = vec![0u16; 256];
    let mut fs_flags: u32 = 0;
    let mut serial: u32 = 0;
    let mut max_component: u32 = 0;

    // SAFETY: all buffers are valid, sizes match the Vec lengths.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW(
            root_wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            &mut max_component,
            &mut fs_flags,
            fs_name_buf.as_mut_ptr(),
            fs_name_buf.len() as u32,
        )
    };

    if ok == 0 {
        // GetVolumeInformationW failed — fall back to NTFS hardlink conservatively.
        log::warn!(
            "GetVolumeInformationW failed for '{}'; defaulting to NtfsHardlink",
            root.display()
        );
        return Ok(LinkStrategy::NtfsHardlink);
    }

    // Decode the null-terminated wide filesystem name.
    let fs_name_len = fs_name_buf.iter().position(|&c| c == 0).unwrap_or(0);
    let fs_name = String::from_utf16_lossy(&fs_name_buf[..fs_name_len]);

    // FILE_SUPPORTS_BLOCK_REFCOUNTING = 0x08000000 — signals ReFS or Dev Drive CoW support.
    const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;

    if fs_flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0 {
        // Perform a live CoW dry-run to confirm FSCTL_DUPLICATE_EXTENTS_TO_FILE works.
        if cow_dry_run_windows(tmp_dir) {
            log::info!(
                "Volume '{}' fs='{}' → RefsBlockClone",
                root.display(),
                fs_name
            );
            return Ok(LinkStrategy::RefsBlockClone);
        }
        log::warn!(
            "Volume '{}' reports CoW but dry-run failed; falling back to NtfsHardlink",
            root.display()
        );
    } else {
        log::info!(
            "Volume '{}' fs='{}' → NtfsHardlink",
            root.display(),
            fs_name
        );
    }

    Ok(LinkStrategy::NtfsHardlink)
}

/// Writes a 4 KiB sentinel file and attempts `FSCTL_DUPLICATE_EXTENTS_TO_FILE`.
/// Returns `true` if the clone succeeded; the probe files are always cleaned up.
#[cfg(all(target_os = "windows", feature = "windows-ntfs"))]
fn cow_dry_run_windows(tmp_dir: &Path) -> bool {
    use std::io::Write;

    let src_path = tmp_dir.join(format!("probe_src_{}.tmp", uuid::Uuid::new_v4().simple()));
    let dst_path = tmp_dir.join(format!("probe_dst_{}.tmp", uuid::Uuid::new_v4().simple()));
    let sentinel = [0xABu8; 4096];

    // Write the source probe file.
    let write_ok = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&src_path)?;
        f.write_all(&sentinel)?;
        f.flush()
    })();

    if write_ok.is_err() {
        let _ = fs::remove_file(&src_path);
        return false;
    }

    // Attempt FSCTL_DUPLICATE_EXTENTS_TO_FILE via windows-sys.
    #[cfg(feature = "windows-ntfs")]
    let result = {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, OPEN_ALWAYS,
            OPEN_EXISTING,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;
        use windows_sys::Win32::System::Ioctl::{
            DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE,
        };

        let src_wide: Vec<u16> = OsStr::new(&src_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let dst_wide: Vec<u16> = OsStr::new(&dst_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: raw Win32 handles; we validate against INVALID_HANDLE_VALUE immediately.
        let src_handle = unsafe {
            CreateFileW(
                src_wide.as_ptr(),
                FILE_GENERIC_READ,
                FILE_SHARE_READ,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0 as _,
            )
        };
        let dst_handle = unsafe {
            CreateFileW(
                dst_wide.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_ALWAYS,
                0,
                0 as _,
            )
        };

        let mut success = false;
        if src_handle != INVALID_HANDLE_VALUE && dst_handle != INVALID_HANDLE_VALUE {
            let ded = DUPLICATE_EXTENTS_DATA {
                FileHandle: src_handle as _,
                SourceFileOffset: 0,
                TargetFileOffset: 0,
                ByteCount: 4096,
            };
            let mut bytes_returned: u32 = 0;
            let io_ok = unsafe {
                DeviceIoControl(
                    dst_handle,
                    FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                    &ded as *const _ as *const _,
                    std::mem::size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut bytes_returned,
                    std::ptr::null_mut(),
                )
            };
            success = io_ok != 0;
        }

        // SAFETY: closing handles we own.
        if src_handle != INVALID_HANDLE_VALUE {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(src_handle);
            }
        }
        if dst_handle != INVALID_HANDLE_VALUE {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(dst_handle);
            }
        }
        success
    };

    #[cfg(not(feature = "windows-ntfs"))]
    let result = false;

    let _ = fs::remove_file(&src_path);
    let _ = fs::remove_file(&dst_path);
    result
}

// ─── Linux implementation ─────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn detect_strategy_linux(root: &Path, _tmp_dir: &Path) -> Result<LinkStrategy, ProbeError> {
    // statfs(2) gives us f_type — the numeric filesystem magic constant.
    // We use libc::statfs directly for the raw f_type field.
    #[cfg(feature = "linux-cow")]
    {
        let fs_type = linux_statfs_type(root);
        match fs_type {
            // Btrfs and XFS support FICLONE; ZFS ioctl path is different — treat as reflink.
            Some(t) if is_cow_linux_fstype(t) => {
                if cow_dry_run_linux(_tmp_dir) {
                    log::info!(
                        "Volume '{}' fstype=0x{:X} → BtrfsReflink",
                        root.display(),
                        t
                    );
                    return Ok(LinkStrategy::BtrfsReflink);
                }
                log::warn!(
                    "Volume '{}' CoW fstype detected but FICLONE failed; falling back",
                    root.display()
                );
            }
            _ => {}
        }
    }

    log::info!("Volume '{}' → PosixHardlink", root.display());
    Ok(LinkStrategy::PosixHardlink)
}

#[cfg(all(target_os = "linux", feature = "linux-cow"))]
fn linux_statfs_type(path: &Path) -> Option<i64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `buf` is zero-initialised; `c_path` is valid null-terminated.
    let ret = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
    if ret == 0 {
        Some(buf.f_type as i64)
    } else {
        None
    }
}

#[cfg(all(target_os = "linux", feature = "linux-cow"))]
fn is_cow_linux_fstype(f_type: i64) -> bool {
    const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;
    const XFS_SUPER_MAGIC: i64 = 0x5846_5342;
    // ZFS magic (from zfs_ioctl.h) — treat as CoW-capable for FICLONE attempt.
    const ZFS_SUPER_MAGIC: i64 = 0x2FC1_2FC1;
    matches!(
        f_type,
        BTRFS_SUPER_MAGIC | XFS_SUPER_MAGIC | ZFS_SUPER_MAGIC
    )
}

/// Performs a live FICLONE dry-run to confirm reflink support.
#[cfg(all(target_os = "linux", feature = "linux-cow"))]
fn cow_dry_run_linux(tmp_dir: &Path) -> bool {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    let src_path = tmp_dir.join(format!("probe_src_{}.tmp", uuid::Uuid::new_v4().simple()));
    let dst_path = tmp_dir.join(format!("probe_dst_{}.tmp", uuid::Uuid::new_v4().simple()));
    let sentinel = [0xCDu8; 4096];

    let result = (|| -> std::io::Result<bool> {
        let mut src = fs::File::create(&src_path)?;
        src.write_all(&sentinel)?;
        src.flush()?;
        drop(src);

        let src_f = fs::File::open(&src_path)?;
        // Pre-allocate the destination to the same size.
        let dst_f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&dst_path)?;
        // Grow destination to match source so FICLONE can succeed.
        dst_f.set_len(4096)?;

        // ioctl FICLONE (0x40049409) — copies extents from src_fd into dst_fd.
        // SAFETY: both fds are open and valid; ioctl is read-only with respect to src.
        const FICLONE: u64 = 0x4004_9409;
        let ret = unsafe { libc::ioctl(dst_f.as_raw_fd(), FICLONE, src_f.as_raw_fd()) };
        Ok(ret == 0)
    })();

    let _ = fs::remove_file(&src_path);
    let _ = fs::remove_file(&dst_path);
    result.unwrap_or(false)
}

// ─── macOS implementation ─────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn detect_strategy_macos(root: &Path, tmp_dir: &Path) -> Result<LinkStrategy, ProbeError> {
    if cow_dry_run_macos(tmp_dir) {
        log::info!("Volume '{}' → ApfsClonefile", root.display());
        return Ok(LinkStrategy::ApfsClonefile);
    }
    log::info!(
        "Volume '{}' → PosixHardlink (clonefile not supported)",
        root.display()
    );
    Ok(LinkStrategy::PosixHardlink)
}

/// Probes APFS clonefile(2) availability by performing a live dry-run clone.
#[cfg(target_os = "macos")]
fn cow_dry_run_macos(tmp_dir: &Path) -> bool {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;

    let src_path = tmp_dir.join(format!("probe_src_{}.tmp", uuid::Uuid::new_v4().simple()));
    let dst_path = tmp_dir.join(format!("probe_dst_{}.tmp", uuid::Uuid::new_v4().simple()));
    let sentinel = [0xEFu8; 4096];

    let result = (|| -> std::io::Result<bool> {
        let mut f = fs::File::create(&src_path)?;
        f.write_all(&sentinel)?;
        f.flush()?;
        drop(f);

        let src_c = CString::new(src_path.as_os_str().as_bytes())?;
        let dst_c = CString::new(dst_path.as_os_str().as_bytes())?;

        // clonefile(2): src, dst, flags=0. Returns 0 on success, -1 on error.
        // SAFETY: both strings are valid null-terminated C strings; flags=0 is documented.
        extern "C" {
            fn clonefile(
                src: *const libc::c_char,
                dst: *const libc::c_char,
                flags: u32,
            ) -> libc::c_int;
        }
        let ret = unsafe { clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
        Ok(ret == 0)
    })();

    let _ = fs::remove_file(&src_path);
    let _ = fs::remove_file(&dst_path);
    result.unwrap_or(false)
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // P1-U01: probe_volume on a real local temp dir must return a valid strategy without error.
    // On Windows the temp dir is typically on NTFS; on Linux typically ext4/tmpfs.
    #[test]
    fn test_probe_real_local_volume() {
        let dir = TempDir::new().expect("tempdir");
        let cap = probe_volume(dir.path()).expect("probe_volume must not error");
        assert!(
            cap.volume_root == dir.path(),
            "volume_root must match the probed path"
        );
        assert!(
            cap.max_links_per_slot > 0,
            "max_links_per_slot must be positive"
        );
    }

    // P1-U02: probe_volume on a path that does not exist must return StatFailed.
    #[test]
    fn test_probe_nonexistent_path_returns_stat_failed() {
        let result = probe_volume(Path::new("/this/path/does/not/exist/itan_probe_test_xyz"));
        assert!(
            matches!(result, Err(ProbeError::StatFailed { .. })),
            "expected StatFailed, got: {:?}",
            result
        );
    }

    // P1-U03: probe_volume on a regular file must return NotADirectory.
    #[test]
    fn test_probe_regular_file_returns_not_a_directory() {
        let dir = TempDir::new().expect("tempdir");
        let file_path = dir.path().join("not_a_dir.txt");
        fs::write(&file_path, b"hello").expect("write sentinel");

        let result = probe_volume(&file_path);
        assert!(
            matches!(result, Err(ProbeError::NotADirectory(_))),
            "expected NotADirectory, got: {:?}",
            result
        );
    }

    // P1-U04: NtfsHardlink strategy must report max_links_per_slot == 1_000.
    #[test]
    fn test_ntfs_hardlink_max_links() {
        assert_eq!(LinkStrategy::NtfsHardlink.max_links_per_slot(), 1_000);
    }

    // P1-U05: PosixHardlink strategy must report max_links_per_slot == 64_000.
    #[test]
    fn test_posix_hardlink_max_links() {
        assert_eq!(LinkStrategy::PosixHardlink.max_links_per_slot(), 64_000);
    }

    // P1-U06: CoW strategies must report max_links_per_slot == u32::MAX (no ceiling).
    #[test]
    fn test_cow_strategies_no_link_ceiling() {
        for strategy in [
            LinkStrategy::RefsBlockClone,
            LinkStrategy::BtrfsReflink,
            LinkStrategy::ApfsClonefile,
        ] {
            assert_eq!(
                strategy.max_links_per_slot(),
                u32::MAX,
                "{:?} should have no link ceiling",
                strategy
            );
        }
    }

    // P1-U07: is_hardlink() must return true only for hardlink strategies.
    #[test]
    fn test_is_hardlink_classification() {
        assert!(LinkStrategy::NtfsHardlink.is_hardlink());
        assert!(LinkStrategy::PosixHardlink.is_hardlink());
        assert!(!LinkStrategy::RefsBlockClone.is_hardlink());
        assert!(!LinkStrategy::BtrfsReflink.is_hardlink());
        assert!(!LinkStrategy::ApfsClonefile.is_hardlink());
    }

    // P1-U08: probe_volume must create .itan_store/tmp/ on the target volume.
    #[test]
    fn test_probe_creates_itan_store_tmp() {
        let dir = TempDir::new().expect("tempdir");
        probe_volume(dir.path()).expect("probe_volume");
        let tmp_dir = dir.path().join(".itan_store").join("tmp");
        assert!(
            tmp_dir.is_dir(),
            ".itan_store/tmp/ must be created by probe_volume"
        );
    }
}
