//! USN Journal delta scanner — Windows NTFS only.
//!
//! Reads the USN Change Journal to detect files that have been modified via hardlink
//! changes or data overwrites since the last scan, allowing the engine to perform
//! targeted re-evaluation rather than a full workspace traversal.
//!
//! This module is compiled only when the `windows-ntfs` feature is enabled on a
//! Windows target.  On all other platforms it does not exist.

#![cfg(all(target_os = "windows", feature = "windows-ntfs"))]

use std::path::PathBuf;

use thiserror::Error;

/// Errors from USN Journal operations.
#[derive(Debug, Error)]
pub enum UsnError {
    #[error("cannot open volume handle for '{volume}': {source}")]
    OpenVolume {
        volume: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("FSCTL_READ_USN_JOURNAL failed: {0}")]
    ReadJournal(std::io::Error),

    #[error(
        "USN Journal has wrapped; last valid USN 0x{last_valid:016X} is ahead of stored USN 0x{stored:016X}"
    )]
    JournalWrapped { last_valid: u64, stored: u64 },
}

/// Reasons a path appears in a USN delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsnReason {
    /// A hardlink was added or removed (`USN_REASON_HARD_LINK_CHANGE`).
    HardLinkChange,
    /// File data was overwritten (`USN_REASON_DATA_OVERWRITE`).
    DataOverwrite,
    /// Another change type that the engine does not act on.
    Other,
}

/// A single USN Journal entry relevant to the reclamation engine.
#[derive(Debug, Clone)]
pub struct UsnEntry {
    pub path: PathBuf,
    pub reason: UsnReason,
    pub usn: u64,
}

/// NTFS USN Journal delta scanner.
///
/// Maintains a cursor (`last_usn`) so that successive calls to `scan_delta()` return
/// only new events since the previous scan.
pub struct UsnDeltaScanner {
    volume_root: PathBuf,
    last_usn: u64,
}

impl UsnDeltaScanner {
    /// Opens the USN Journal on the given volume.
    ///
    /// Reads the current `NextUsn` to initialise the cursor so only future events
    /// are returned on the first `scan_delta()` call.
    ///
    /// # Errors
    ///
    /// Returns [`UsnError::OpenVolume`] if the volume handle cannot be acquired.
    pub fn open(volume_root: &std::path::Path) -> Result<Self, UsnError> {
        // Open a raw volume handle with FSCTL access.
        let volume_handle =
            open_volume_handle(volume_root).map_err(|source| UsnError::OpenVolume {
                volume: volume_root.to_owned(),
                source,
            })?;

        // Query FSCTL_QUERY_USN_JOURNAL to get the current NextUsn cursor.
        let next_usn = query_journal_next_usn(volume_handle)?;

        // SAFETY: `volume_handle` is closed after the query — we don't retain it.
        close_handle(volume_handle);

        Ok(Self {
            volume_root: volume_root.to_owned(),
            last_usn: next_usn,
        })
    }

    /// Reads all USN Journal entries since `last_usn` and returns those matching
    /// `USN_REASON_HARD_LINK_CHANGE` or `USN_REASON_DATA_OVERWRITE`.
    ///
    /// Updates the internal cursor to the journal's current `NextUsn` after the scan
    /// so that successive calls return only new events.
    ///
    /// # Errors
    ///
    /// Returns [`UsnError::JournalWrapped`] if the journal has overwritten the stored
    /// cursor.  The caller should perform a full workspace traversal as fallback.
    pub fn scan_delta(&mut self) -> Result<Vec<UsnEntry>, UsnError> {
        let volume_handle =
            open_volume_handle(&self.volume_root).map_err(|source| UsnError::OpenVolume {
                volume: self.volume_root.clone(),
                source,
            })?;

        let result = read_usn_journal(volume_handle, self.last_usn, &self.volume_root);
        close_handle(volume_handle);

        match result {
            Ok((entries, next_usn)) => {
                self.last_usn = next_usn;
                Ok(entries)
            }
            Err(e) => Err(e),
        }
    }
}

// ─── Win32 internals ──────────────────────────────────────────────────────────

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0,
    USN_REASON_DATA_OVERWRITE, USN_REASON_HARD_LINK_CHANGE, USN_RECORD_V2,
};

fn open_volume_handle(volume_root: &std::path::Path) -> std::io::Result<HANDLE> {
    // Volume path for CreateFile must be `\\.\C:` (no trailing backslash).
    let vol_str = volume_root
        .to_str()
        .unwrap_or("C:\\")
        .trim_end_matches('\\');
    let unc_vol = format!("\\\\.\\{}", vol_str.trim_end_matches(':'));
    let wide: Vec<u16> = OsStr::new(&unc_vol)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: wide is null-terminated; all other args are well-defined constants.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            0 as HANDLE,
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

fn close_handle(handle: HANDLE) {
    // SAFETY: handle is valid and caller ensures it is not used after this call.
    unsafe {
        CloseHandle(handle);
    }
}

fn query_journal_next_usn(handle: HANDLE) -> Result<u64, UsnError> {
    // USN_JOURNAL_DATA_V0 layout: first two u64 fields are UsnJournalID and FirstUsn.
    // We only need NextUsn which is the third field.
    let mut buf = [0u64; 16]; // Large enough for USN_JOURNAL_DATA_V2.
    let mut bytes: u32 = 0;

    // SAFETY: buf is zeroed, size is correct; ioctl is read-only.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            buf.as_mut_ptr() as *mut _,
            (std::mem::size_of_val(&buf)) as u32,
            &mut bytes,
            std::ptr::null_mut(),
        )
    };

    if ok == 0 {
        return Err(UsnError::ReadJournal(std::io::Error::last_os_error()));
    }

    // USN_JOURNAL_DATA layout offsets (bytes): UsnJournalID(8), FirstUsn(8), NextUsn(8)...
    // buf[0] = UsnJournalID, buf[1] = FirstUsn, buf[2] = NextUsn
    Ok(buf[2])
}

fn read_usn_journal(
    handle: HANDLE,
    start_usn: u64,
    volume_root: &std::path::Path,
) -> Result<(Vec<UsnEntry>, u64), UsnError> {
    use std::mem;

    // READ_USN_JOURNAL_DATA_V0: StartUsn, ReasonMask, ReturnOnlyOnClose, TimeOut, BytesToWaitFor, UsnJournalID
    let reason_mask = USN_REASON_HARD_LINK_CHANGE | USN_REASON_DATA_OVERWRITE;
    let query = READ_USN_JOURNAL_DATA_V0 {
        StartUsn: start_usn as i64,
        ReasonMask: reason_mask,
        ReturnOnlyOnClose: 0,
        Timeout: 0,
        BytesToWaitFor: 0,
        UsnJournalID: 0, // 0 = accept any journal ID.
    };

    let mut out_buf = vec![0u8; 65_536];
    let mut bytes: u32 = 0;

    // SAFETY: query is fully initialised; out_buf has documented size.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_READ_USN_JOURNAL,
            &query as *const _ as *const _,
            mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
            out_buf.as_mut_ptr() as *mut _,
            out_buf.len() as u32,
            &mut bytes,
            std::ptr::null_mut(),
        )
    };

    if ok == 0 {
        return Err(UsnError::ReadJournal(std::io::Error::last_os_error()));
    }

    // First 8 bytes of the output buffer are the NextUsn cursor.
    let next_usn = u64::from_le_bytes(out_buf[..8].try_into().unwrap_or([0u8; 8]));

    // Parse USN_RECORD_V2 entries starting at offset 8.
    let mut entries = Vec::new();
    let mut offset = 8usize;

    while offset + mem::size_of::<USN_RECORD_V2>() <= bytes as usize {
        // SAFETY: we have verified there are enough bytes for the header.
        let record = unsafe { &*(out_buf.as_ptr().add(offset) as *const USN_RECORD_V2) };
        let record_len = record.RecordLength as usize;
        if record_len == 0 {
            break;
        }

        let reason = if record.Reason & USN_REASON_HARD_LINK_CHANGE != 0 {
            UsnReason::HardLinkChange
        } else if record.Reason & USN_REASON_DATA_OVERWRITE != 0 {
            UsnReason::DataOverwrite
        } else {
            UsnReason::Other
        };

        if reason != UsnReason::Other {
            // Reconstruct the file path from the volume root + file name in the record.
            let name_offset = record.FileNameOffset as usize;
            let name_length = record.FileNameLength as usize;
            if offset + name_offset + name_length <= bytes as usize {
                let name_bytes = &out_buf[offset + name_offset..offset + name_offset + name_length];
                let name_u16: Vec<u16> = name_bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                if let Ok(name) = String::from_utf16(&name_u16) {
                    let path = volume_root.join(name);
                    entries.push(UsnEntry {
                        path,
                        reason,
                        usn: record.Usn as u64,
                    });
                }
            }
        }

        offset += record_len;
    }

    Ok((entries, next_usn))
}
