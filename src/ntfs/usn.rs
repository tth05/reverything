//! USN change journal access.

use std::ffi::c_void;

use eyre::{eyre, Context, Result};
use windows::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED,
    ERROR_JOURNAL_NOT_ACTIVE,
};
use windows::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_FILE_RECORD, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
    READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V2, USN_REASON_BASIC_INFO_CHANGE, USN_REASON_CLOSE,
    USN_REASON_DATA_EXTEND, USN_REASON_DATA_OVERWRITE, USN_REASON_DATA_TRUNCATION,
    USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_HARD_LINK_CHANGE,
    USN_REASON_RENAME_NEW_NAME, USN_REASON_REPARSE_POINT_CHANGE,
};
use windows::Win32::System::IO::DeviceIoControl;

use crate::ntfs::io::{ioctl, ioctl_struct, Handle};
use crate::ntfs::record::{u16_at, u32_at, u64_at, MFT_REF_MASK};
use crate::ntfs::volume::Volume;

#[derive(Debug, Copy, Clone)]
pub struct JournalInfo {
    pub id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
}

pub fn query_journal(handle: &Handle) -> Result<JournalInfo> {
    let data: USN_JOURNAL_DATA_V2 = ioctl_struct(handle, FSCTL_QUERY_USN_JOURNAL)
        .with_context(|| "FSCTL_QUERY_USN_JOURNAL failed")?;
    Ok(JournalInfo {
        id: data.UsnJournalID,
        first_usn: data.FirstUsn,
        next_usn: data.NextUsn,
    })
}

#[derive(Debug)]
pub enum JournalError {
    /// The journal was deleted, recreated or wrapped past our position. The index has to be
    /// rebuilt from the MFT.
    Reset,
    Other(eyre::Report),
}

impl From<eyre::Report> for JournalError {
    fn from(e: eyre::Report) -> Self {
        JournalError::Other(e)
    }
}

/// Every reason that can change something we index. `CLOSE` makes sure the final size of a file
/// that was written to is picked up.
const REASON_MASK: u32 = USN_REASON_FILE_CREATE
    | USN_REASON_FILE_DELETE
    | USN_REASON_RENAME_NEW_NAME
    | USN_REASON_HARD_LINK_CHANGE
    | USN_REASON_BASIC_INFO_CHANGE
    | USN_REASON_DATA_EXTEND
    | USN_REASON_DATA_TRUNCATION
    | USN_REASON_DATA_OVERWRITE
    | USN_REASON_REPARSE_POINT_CHANGE
    | USN_REASON_CLOSE;

/// Reads change records from the journal, starting at a known position.
pub struct JournalReader {
    /// Synchronous handle, so a read can block until new records arrive
    handle: Handle,
    journal_id: u64,
    next_usn: i64,
    buf: Vec<u64>,
}

impl JournalReader {
    pub fn open(volume: Volume, journal_id: u64, start_usn: i64) -> Result<Self> {
        Ok(Self {
            handle: volume.open(false, false)?,
            journal_id,
            next_usn: start_usn,
            buf: vec![0u64; 64 * 1024 / 8],
        })
    }

    pub fn next_usn(&self) -> i64 {
        self.next_usn
    }

    /// Collects the record numbers of changed files into `changed`. With `wait`, blocks until at
    /// least one journal record exists past our position. Reads until the journal is drained.
    pub fn read_changes(&mut self, wait: bool, changed: &mut Vec<u32>) -> Result<(), JournalError> {
        let mut wait = wait;
        loop {
            let input = READ_USN_JOURNAL_DATA_V1 {
                StartUsn: self.next_usn,
                ReasonMask: REASON_MASK,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: wait as u64,
                UsnJournalID: self.journal_id,
                MinMajorVersion: 2,
                MaxMajorVersion: 3,
            };
            wait = false;

            let mut bytes = 0u32;
            let res = unsafe {
                DeviceIoControl(
                    self.handle.raw,
                    FSCTL_READ_USN_JOURNAL,
                    Some(&input as *const _ as *const c_void),
                    size_of_val(&input) as u32,
                    Some(self.buf.as_mut_ptr() as *mut c_void),
                    (self.buf.len() * 8) as u32,
                    Some(&mut bytes),
                    None,
                )
            };
            if let Err(e) = res {
                let reset = [
                    ERROR_JOURNAL_ENTRY_DELETED,
                    ERROR_JOURNAL_DELETE_IN_PROGRESS,
                    ERROR_JOURNAL_NOT_ACTIVE,
                    ERROR_INVALID_PARAMETER,
                ];
                if reset.iter().any(|r| r.to_hresult() == e.code()) {
                    return Err(JournalError::Reset);
                }
                return Err(eyre!(e).wrap_err("FSCTL_READ_USN_JOURNAL failed").into());
            }

            let data = unsafe {
                std::slice::from_raw_parts(self.buf.as_ptr() as *const u8, bytes as usize)
            };
            if data.len() < 8 {
                return Ok(());
            }
            let next_usn = u64_at(data, 0) as i64;
            parse_records(&data[8..], changed);

            let advanced = next_usn > self.next_usn;
            self.next_usn = next_usn.max(self.next_usn);
            if !advanced || data.len() <= 8 {
                return Ok(());
            }
        }
    }
}

fn parse_records(mut data: &[u8], changed: &mut Vec<u32>) {
    while data.len() >= 8 {
        let len = u32_at(data, 0) as usize;
        if len < 8 || len > data.len() {
            break;
        }
        let rec = &data[..len];
        let (frn, reason) = match u16_at(rec, 4) {
            2 if len >= 44 => (u64_at(rec, 8), u32_at(rec, 40)),
            // The low 8 bytes of the 128 bit id are the NTFS file reference
            3 if len >= 60 => (u64_at(rec, 8), u32_at(rec, 56)),
            _ => (0, 0),
        };
        if reason & REASON_MASK != 0 {
            let record = frn & MFT_REF_MASK;
            if record <= u32::MAX as u64 {
                changed.push(record as u32);
            }
        }
        data = &data[len..];
    }
}

/// Fetches the current state of a single record from NTFS, which serves it from its cache.
/// Returns `None` if the record is not in use.
pub fn read_file_record(handle: &Handle, record: u32, record_size: usize) -> Result<Option<Vec<u8>>> {
    // NTFS_FILE_RECORD_OUTPUT_BUFFER: FileReferenceNumber (8), FileRecordLength (4), buffer
    const HEADER: usize = 12;
    let input = (record as i64).to_le_bytes();
    let mut output = vec![0u8; HEADER + record_size + 4];
    let len = ioctl(handle, FSCTL_GET_NTFS_FILE_RECORD, Some(&input), &mut output)
        .map_err(|e| eyre!(e).wrap_err("FSCTL_GET_NTFS_FILE_RECORD failed"))? as usize;
    if len < HEADER {
        return Ok(None);
    }

    // NTFS returns the closest in-use record at or below the requested one
    let returned = u64_at(&output, 0) & MFT_REF_MASK;
    if returned != record as u64 {
        return Ok(None);
    }

    let rec_len = (u32_at(&output, 8) as usize).min(len - HEADER);
    output.copy_within(HEADER..HEADER + rec_len, 0);
    output.truncate(rec_len);
    Ok(Some(output))
}
