use eyre::{Context, Report, Result};
use windows::core::HSTRING;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetVolumeNameForVolumeMountPointW, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};
use windows::Win32::System::IO::{DeviceIoControl, OVERLAPPED};

use crate::ntfs::try_close_handle;

#[derive(Debug, Copy, Clone)]
pub struct Volume {
    pub id: char,
}

impl Volume {
    pub fn create_read_handle(&self) -> Result<HANDLE> {
        unsafe {
            CreateFileW(
                &HSTRING::from(&format!("\\\\.\\{}:", self.id)),
                FILE_GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
            .with_context(|| format!("CreateFileW failed for '{}'", self.id))
        }
    }

    pub fn query_volume_data(&self) -> Result<NTFS_VOLUME_DATA_BUFFER> {
        let handle = self.create_read_handle()?;
        let data = NTFS_VOLUME_DATA_BUFFER::default();
        let res = unsafe {
            let res = DeviceIoControl(
                handle,
                FSCTL_GET_NTFS_VOLUME_DATA,
                None,
                0,
                Some(&data as *const NTFS_VOLUME_DATA_BUFFER as *mut _),
                std::mem::size_of_val(&data) as u32,
                None,
                None,
            );
            try_close_handle(handle)?;
            res
        };

        if res.is_err() {
            return Err(Report::new(std::io::Error::last_os_error()))
                .with_context(|| "DeviceIoControl failed");
        }

        Ok(data)
    }
}

pub fn create_overlapped(offset: usize) -> OVERLAPPED {
    let low = offset & 0xffffffff;
    let high = offset >> 32;
    let mut ov = OVERLAPPED::default();
    unsafe {
        ov.Anonymous.Anonymous.Offset = low as u32;
        ov.Anonymous.Anonymous.OffsetHigh = high as u32;
    }

    ov
}

pub fn get_volumes() -> Vec<Volume> {
    let mut buf = [0u16; 255];

    ('a'..='z')
        .filter(|c| {
            unsafe {
                GetVolumeNameForVolumeMountPointW(&HSTRING::from(&format!("{}:\\", c)), &mut buf)
            }
            .is_ok()
        })
        .map(|c| Volume { id: c })
        .collect()
}
