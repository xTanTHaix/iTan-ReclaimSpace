//! Read-Only Invariant enforcement — ensures Master Slot files in the CAS can never
//! be overwritten in-place by a build toolchain or IDE.
//!
//! On POSIX, chmod `0444` (read-only data file) or `0555` (read-execute for binaries)
//! is applied.  On Windows, `FILE_ATTRIBUTE_READONLY` is set.  Both strategies force
//! any write attempt to fail at the kernel level, obliging compilers to `unlink` the
//! old file and create a fresh inode — the Break-on-Write invariant.

use std::path::Path;

use thiserror::Error;

/// Errors that can arise from permission enforcement.
#[derive(Debug, Error)]
pub enum PermError {
    #[error("cannot set read-only attribute on '{path}': {source}")]
    SetReadOnly {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot remove read-only attribute from '{path}': {source}")]
    ClearReadOnly {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot query file permissions for '{path}': {source}")]
    Query {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Sets the platform-appropriate read-only permission on a Master Slot file.
///
/// On POSIX: applies `0555` if `is_executable` is true, otherwise `0444`.
/// On Windows: sets `FILE_ATTRIBUTE_READONLY`.
///
/// # Errors
///
/// Returns [`PermError::SetReadOnly`] if the permission change fails.
pub fn enforce_read_only(path: &Path, is_executable: bool) -> Result<(), PermError> {
    #[cfg(unix)]
    {
        enforce_posix_mode(path, is_executable)
    }
    #[cfg(windows)]
    {
        let _ = is_executable; // Windows has no executable bit concept for hardlink safety.
        enforce_read_only_windows(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, is_executable);
        Ok(())
    }
}

/// Temporarily clears the read-only restriction so the file can be atomically replaced.
///
/// This is called immediately before the in-place swap step of the Atomic Linkage Pipeline
/// and must be followed by re-applying `enforce_read_only` on the new master slot after swap.
///
/// # Errors
///
/// Returns [`PermError::ClearReadOnly`] if the attribute cannot be cleared.
pub fn strip_read_only(path: &Path) -> Result<(), PermError> {
    #[cfg(unix)]
    {
        strip_read_only_posix(path)
    }
    #[cfg(windows)]
    {
        strip_read_only_windows(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

/// Returns `true` if `path` carries the executable bit (POSIX) or is considered executable
/// by inspecting the original file's metadata before it is replaced.
///
/// On Windows this always returns `false` because the executable concept is encoded in
/// the PE header, not NTFS ACLs.
///
/// # Errors
///
/// Returns [`PermError::Query`] if metadata cannot be read.
pub fn is_executable(path: &Path) -> Result<bool, PermError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).map_err(|source| PermError::Query {
            path: path.to_owned(),
            source,
        })?;
        // Check any execute bit: user, group, or other (st_mode & 0o111).
        Ok(meta.mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

// ─── POSIX implementations ────────────────────────────────────────────────────

#[cfg(unix)]
fn enforce_posix_mode(path: &Path, is_executable: bool) -> Result<(), PermError> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    // 0o444 = r--r--r-- (data file), 0o555 = r-xr-xr-x (executable).
    let mode = if is_executable { 0o555 } else { 0o444 };
    let perms = fs::Permissions::from_mode(mode);
    fs::set_permissions(path, perms).map_err(|source| PermError::SetReadOnly {
        path: path.to_owned(),
        source,
    })
}

#[cfg(unix)]
fn strip_read_only_posix(path: &Path) -> Result<(), PermError> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    // 0o644 grants owner write access for the brief window needed during atomic swap.
    let perms = fs::Permissions::from_mode(0o644);
    fs::set_permissions(path, perms).map_err(|source| PermError::ClearReadOnly {
        path: path.to_owned(),
        source,
    })
}

// ─── Windows implementations ──────────────────────────────────────────────────

#[cfg(windows)]
fn enforce_read_only_windows(path: &Path) -> Result<(), PermError> {
    let mut perms = std::fs::metadata(path)
        .map_err(|source| PermError::Query {
            path: path.to_owned(),
            source,
        })?
        .permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(path, perms).map_err(|source| PermError::SetReadOnly {
        path: path.to_owned(),
        source,
    })
}

#[cfg(windows)]
fn strip_read_only_windows(path: &Path) -> Result<(), PermError> {
    let mut perms = std::fs::metadata(path)
        .map_err(|source| PermError::Query {
            path: path.to_owned(),
            source,
        })?
        .permissions();
    // On Windows, set_readonly(false) clears FILE_ATTRIBUTE_READONLY; it does not affect POSIX world-writable bits.
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms).map_err(|source| PermError::ClearReadOnly {
        path: path.to_owned(),
        source,
    })
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // P4-U06 / P4-U07: master slot must be read-only after enforce_read_only.
    #[test]
    fn test_enforce_read_only_makes_file_readonly() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("master_slot");
        fs::write(&path, b"content").expect("write");

        enforce_read_only(&path, false).expect("enforce_read_only");

        let meta = fs::metadata(&path).expect("metadata");
        assert!(
            meta.permissions().readonly(),
            "file must be read-only after enforce_read_only"
        );
    }

    // strip_read_only must reverse the read-only flag.
    #[test]
    fn test_strip_read_only_makes_file_writable() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("master_slot");
        fs::write(&path, b"content").expect("write");
        enforce_read_only(&path, false).expect("enforce");

        strip_read_only(&path).expect("strip");

        let meta = fs::metadata(&path).expect("metadata");
        assert!(
            !meta.permissions().readonly(),
            "file must be writable after strip_read_only"
        );
    }

    // P4-U08: executable file on POSIX must receive mode 0o555.
    #[cfg(unix)]
    #[test]
    fn test_enforce_executable_mode_posix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("master_exec");
        fs::write(&path, b"#!/bin/sh").expect("write");

        enforce_read_only(&path, true).expect("enforce exec");

        let mode = fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o555, "executable master must have mode 0o555");
    }

    // Non-executable file on POSIX must receive mode 0o444.
    #[cfg(unix)]
    #[test]
    fn test_enforce_data_mode_posix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("master_data");
        fs::write(&path, b"data").expect("write");

        enforce_read_only(&path, false).expect("enforce data");

        let mode = fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o444, "data master must have mode 0o444");
    }
}
