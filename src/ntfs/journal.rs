use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::size_of;
use std::path::PathBuf;

use eyre::{eyre, ContextCompat, Report, Result, WrapErr};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    ExtendedFileIdType, GetFinalPathNameByHandleW, OpenFileById, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_GENERIC_READ, FILE_ID_128, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_NAME,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V2,
    USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME, USN_RECORD_UNION, USN_RECORD_V3,
};
use windows::Win32::System::WindowsProgramming::VOLUME_NAME_NONE;
use windows::Win32::System::IO::DeviceIoControl;

use crate::ntfs::try_close_handle;
use crate::ntfs::volume::Volume;

const MAX_HISTORY_SIZE: usize = 1000;

pub struct Journal {
    handle: HANDLE,
    next_usn: i64,
    journal_id: u64,
    unmatched_renames: VecDeque<UnmatchedRename>,
}

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
                std::mem::size_of_val(&data) as u32,
                None,
                None,
            );

            if !query.as_bool() {
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
                std::mem::size_of_val(&read_input) as u32,
                Some(&mut buffer as *mut _ as *mut c_void),
                std::mem::size_of_val(&buffer) as u32,
                Some(&mut bytes_read as *mut u32),
                None,
            );

            if !query.as_bool() {
                return Err(Report::new(std::io::Error::last_os_error()))
                    .with_context(|| "DeviceIoControl failed trying to read journal entries");
            }

            let next_usn = i64::from_le_bytes(buffer[0..size_of::<i64>()].try_into().unwrap());
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
                let file_path = self.compute_full_path(record)?;

                if record.Reason & USN_REASON_RENAME_OLD_NAME != 0 {
                    if self.unmatched_renames.len() >= MAX_HISTORY_SIZE {
                        self.unmatched_renames.pop_front();
                    }

                    self.unmatched_renames.push_back(UnmatchedRename {
                        file_id: record.FileReferenceNumber,
                        old_path: file_path,
                    });
                } else {
                    let reason = match record.Reason {
                        x if x & USN_REASON_FILE_CREATE != 0 => JournalEntry::FileCreate(file_path),
                        x if x & USN_REASON_FILE_DELETE != 0 => JournalEntry::FileDelete(file_path),
                        x if x & USN_REASON_RENAME_NEW_NAME != 0 => {
                            self.match_rename(file_path, record.FileReferenceNumber)?
                        }
                        _ => unreachable!("Invalid reason"),
                    };

                    entries.push(reason);
                }

                offset += record_length;
            }

            Ok(entries)
        }
    }

    fn match_rename(&mut self, new_path: PathBuf, file_id: FILE_ID_128) -> Result<JournalEntry> {
        let rename = self
            .unmatched_renames
            .iter()
            .find(|x| x.file_id.Identifier == file_id.Identifier)
            .with_context(|| {
                format!(
                    "Failed to find old name for rename {:?} {:?}",
                    new_path, file_id
                )
            })?;

        // We can't immediately remove the rename from the queue because it can be used multiple times
        Ok(JournalEntry::Rename {
            old_path: rename.old_path.clone(),
            new_path,
        })
    }

    fn compute_full_path(&self, record: &USN_RECORD_V3) -> Result<PathBuf> {
        unsafe {
            let desc = FILE_ID_DESCRIPTOR {
                dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
                Type: ExtendedFileIdType,
                Anonymous: FILE_ID_DESCRIPTOR_0 {
                    ExtendedFileId: record.ParentFileReferenceNumber,
                },
            };

            let file_handle = OpenFileById(
                self.handle,
                &desc as *const _,
                FILE_GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                FILE_FLAGS_AND_ATTRIBUTES(0),
            )
            .with_context(|| {
                eyre!(
                    "OpenFileById failed for parent of {:?} ({:?})",
                    record,
                    get_record_file_name(record)
                )
            })?;

            let mut path_buf = [0u16; 2048];
            let path_length =
                GetFinalPathNameByHandleW(file_handle, &mut path_buf, FILE_NAME(VOLUME_NAME_NONE));
            try_close_handle(file_handle)?;

            if path_length == 0 {
                return Err(eyre!(
                    "Failed to get path for parent of {:?} ({:?})",
                    record,
                    get_record_file_name(record)
                ));
            } else if path_length >= 2048 {
                return Err(eyre!(
                    "Path too long {:?} ({:?}, {:?})",
                    record,
                    get_record_file_name(record),
                    path_buf
                ));
            }

            debug_assert!(
                path_buf[0] == '\\' as u16,
                "{:?}",
                String::from_utf16_lossy(&path_buf[0..path_length as usize])
            );

            let path_buf =
                PathBuf::from(String::from_utf16_lossy(&path_buf[1..path_length as usize]));
            let path_buf = path_buf.join(get_record_file_name(record));
            Ok(path_buf)
        }
    }
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

#[derive(Debug)]
pub enum JournalEntry {
    FileCreate(PathBuf),
    FileDelete(PathBuf),
    Rename {
        old_path: PathBuf,
        new_path: PathBuf,
    },
}

struct UnmatchedRename {
    file_id: FILE_ID_128,
    old_path: PathBuf,
}
