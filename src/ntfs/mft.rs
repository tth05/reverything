use eyre::{Result, eyre};
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Ioctl::NTFS_VOLUME_DATA_BUFFER;
use windows::Win32::System::Threading::WaitForSingleObject;

use crate::ntfs::file_record::FileRecord;
use crate::ntfs::volume::{create_overlapped, Volume};

use super::get_last_error_message;

pub struct MftFile {
    data: Vec<u8>,
}

impl MftFile {
    pub fn new(vol: Volume, data: NTFS_VOLUME_DATA_BUFFER) -> Result<Self> {
        let handle = vol.create_read_handle()?;

        // Move to $MFT start
        let mut ov = create_overlapped(data.BytesPerCluster as usize * data.MftStartLcn as usize);
        let mut mft_file_buf = Vec::<u8>::with_capacity(data.BytesPerFileRecordSegment as usize);

        unsafe {
            ReadFile(
                handle,
                Some(mft_file_buf.as_mut_ptr() as *mut _),
                mft_file_buf.capacity() as u32,
                None,
                Some(&mut ov as *mut _),
            );

            if WaitForSingleObject(handle, 1000) != WAIT_OBJECT_0 {
                return Err(eyre!(
                "ReadFile failed '{:?}'",
                get_last_error_message().unwrap()
            ));
            }
            assert!(CloseHandle(handle).as_bool());

            mft_file_buf.set_len(mft_file_buf.capacity());
        }

        FileRecord::fixup(&mut mft_file_buf, data.BytesPerSector as usize);
        Ok(MftFile { data: mft_file_buf })
    }

    pub fn as_record(&self) -> FileRecord {
        FileRecord::new(&self.data)
    }
}
