use crate::ntfs::try_close_handle;
use crate::ntfs::volume::Volume;
use eyre::{Report, Result, WrapErr};
use std::ffi::c_void;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V2, USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME};
use windows::Win32::System::IO::DeviceIoControl;

pub struct Journal {
    handle: HANDLE,
    data: USN_JOURNAL_DATA_V2,
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

        Ok(Self { handle, data })
    }

    pub fn read_entry(&self) -> Result<String> {
        unsafe {
            let mut read_input = READ_USN_JOURNAL_DATA_V1 {
                StartUsn: self.data.FirstUsn,
                ReasonMask: USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE | USN_REASON_RENAME_NEW_NAME | USN_REASON_RENAME_OLD_NAME,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: self.data.UsnJournalID,
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

            // TODO: Parse buffer as USN_RECORD_UNIONs
        }

        Ok(String::new())
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        try_close_handle(self.handle).expect("Failed to close journal handle");
    }
}
