use std::collections::VecDeque;
use std::ffi::c_void;

use eyre::{eyre, ContextCompat, Report, Result, WrapErr};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY
    , FILE_ID_128
    ,
};
use windows::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V2,
    USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME, USN_RECORD_UNION, USN_RECORD_V3,
};
use windows::Win32::System::IO::DeviceIoControl;

use crate::ntfs::try_close_handle;
use crate::ntfs::volume::Volume;

const MAX_UNMATCHED_RENAMES: usize = 2000;

pub struct Journal {
    handle: HANDLE,
    next_usn: i64,
    journal_id: u64,
    unmatched_renames: VecDeque<u64>,
}

unsafe impl Send for Journal {}
unsafe impl Sync for Journal {}

impl Journal {
    pub fn new(vol: Volume) -> Result<Self> {
        let handle = vol.create_read_handle()?;

        let mut data = USN_JOURNAL_DATA_V2::default();
        unsafe {
            let query = DeviceIoControl(
                handle,
                FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some(&mut data as *mut _ as *mut c_void),
                size_of_val(&data) as u32,
                None,
                None,
            );

            if query.is_err() {
                return Err(Report::new(std::io::Error::last_os_error()))
                    .with_context(|| "DeviceIoControl failed trying to query journal data");
            }
        }

        Ok(Self {
            handle,
            next_usn: data.NextUsn,
            journal_id: data.UsnJournalID,
            unmatched_renames: VecDeque::new(),
        })
    }

    pub fn read_entries(&mut self) -> Result<Vec<JournalEntry>> {
        unsafe {
            let mut read_input = READ_USN_JOURNAL_DATA_V1 {
                StartUsn: self.next_usn,
                ReasonMask: USN_REASON_FILE_CREATE
                    | USN_REASON_FILE_DELETE
                    | USN_REASON_RENAME_NEW_NAME
                    | USN_REASON_RENAME_OLD_NAME,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 1,
                UsnJournalID: self.journal_id,
                MinMajorVersion: 3,
                MaxMajorVersion: 3,
            };
            // TODO: Use cluster size from volume data?
            let mut buffer = [0u8; 4096];
            let mut bytes_read = 0u32;
            let query = DeviceIoControl(
                self.handle,
                FSCTL_READ_USN_JOURNAL,
                Some(&mut read_input as *mut _ as *mut c_void),
                size_of_val(&read_input) as u32,
                Some(&mut buffer as *mut _ as *mut c_void),
                size_of_val(&buffer) as u32,
                Some(&mut bytes_read as *mut u32),
                None,
            );

            if query.is_err() {
                return Err(Report::new(std::io::Error::last_os_error()))
                    .with_context(|| "DeviceIoControl failed trying to read journal entries");
            }

            let next_usn = i64::from_le_bytes(buffer[0..size_of::<i64>()].try_into()?);
            if next_usn == 0 || next_usn < self.next_usn {
                return Ok(Vec::new());
            }

            self.next_usn = next_usn;

            let mut entries = Vec::new();
            let mut offset = size_of::<i64>();

            while offset < bytes_read as usize {
                let union = buffer[offset..].as_ptr() as *const USN_RECORD_UNION;
                let header = (*union).Header;
                let record_length = header.RecordLength as usize;

                if record_length == 0 || header.MajorVersion != 3 {
                    return Err(eyre!("Invalid record length or major version {:?}", header));
                }

                let record = &(*union).V3;

                if record.Reason & USN_REASON_RENAME_OLD_NAME != 0 {
                    if self.unmatched_renames.len() >= MAX_UNMATCHED_RENAMES {
                        self.unmatched_renames.pop_front();
                    }

                    self.unmatched_renames
                        .push_back(get_mft_index_from_file_id(record.FileReferenceNumber));
                } else {
                    let is_directory = record.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
                    let reason = match record.Reason {
                        x if x & USN_REASON_FILE_CREATE == x => Ok(JournalEntry::FileCreate {
                            mft_index: get_mft_index_from_file_id(record.FileReferenceNumber),
                            parent_mft_index: get_mft_index_from_file_id(
                                record.ParentFileReferenceNumber,
                            ),
                            name: get_record_file_name(record),
                            is_directory,
                        }),
                        x if x & USN_REASON_FILE_DELETE != 0 => Ok(JournalEntry::FileDelete(
                            get_mft_index_from_file_id(record.FileReferenceNumber),
                        )),
                        x if x & USN_REASON_RENAME_NEW_NAME != 0 => self.match_rename(
                            get_mft_index_from_file_id(record.FileReferenceNumber),
                            get_record_file_name(record),
                            get_mft_index_from_file_id(record.ParentFileReferenceNumber),
                        ),
                        _ => Err(eyre!("")),
                    };

                    if let Ok(reason) = reason {
                        entries.push(reason);
                    }
                }

                offset += record_length;
            }

            // Match file creates to deletes
            let mut i = 0usize;
            while let Some((pos1, JournalEntry::FileCreate { mft_index, .. })) = entries
                .iter()
                .enumerate()
                .find(|(j, e)| *j >= i && matches!(e, JournalEntry::FileCreate { .. }))
            {
                if let Some(pos2) = entries.iter().skip(pos1).rposition(
                    |e| matches!(e, JournalEntry::FileDelete(mft_index2) if mft_index == mft_index2),
                ) {
                    entries.remove(pos2);
                    entries.remove(pos1);
                }

                i += pos1 + 1;
            }

            Ok(entries)
        }
    }

    fn match_rename(
        &mut self,
        mft_index: u64,
        new_name: String,
        new_parent_mft_index: u64,
    ) -> Result<JournalEntry> {
        let idx = self.unmatched_renames
            .iter()
            .position(|x| *x == mft_index)
            .with_context(|| {
                format!(
                    "Failed to find old name for rename {:?} {:?} {:?}",
                    mft_index, new_name, new_parent_mft_index
                )
            })?;
        
        self.unmatched_renames.remove(idx);

        // We can't immediately remove the rename from the queue because it can be used multiple times
        Ok(JournalEntry::Rename {
            mft_index,
            new_name,
            new_parent_mft_index,
        })
    }
}

/// Converts a FILE_ID_128 to an MFT index. The first 6 bytes contain the MFT index followed by a 2 
/// byte sequence number. The upper 8 bytes are only used on ReFS.
/// Only source I could find on this https://github.com/mgeeky/ntfs-journal-viewer/blob/master/journal.c#L559
fn get_mft_index_from_file_id(id: FILE_ID_128) -> u64 {
    u64::from_le_bytes(id.Identifier[..8].try_into().unwrap()) & 0xffff_ffff_ffffu64
}

impl Drop for Journal {
    fn drop(&mut self) {
        try_close_handle(self.handle).expect("Failed to close journal handle");
    }
}

fn get_record_file_name(record: &USN_RECORD_V3) -> String {
    let name_length = record.FileNameLength / 2;

    unsafe {
        let file_name = String::from_utf16_lossy(std::slice::from_raw_parts(
            record.FileName.as_ptr(),
            name_length as usize,
        ));

        file_name
    }
}

#[derive(Debug, Ord, PartialOrd, Eq, PartialEq)]
pub enum JournalEntry {
    FileCreate {
        mft_index: u64,
        parent_mft_index: u64,
        name: String,
        is_directory: bool,
    },
    FileDelete(u64),
    Rename {
        mft_index: u64,
        new_name: String,
        new_parent_mft_index: u64,
    },
}
