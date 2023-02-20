use eyre::{eyre, Context, Result};
use windows::core::HSTRING;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetVolumeNameForVolumeMountPointW, SetFilePointer, FILE_ACCESS_FLAGS, FILE_BEGIN,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, INVALID_SET_FILE_POINTER,
    OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};
use windows::Win32::System::IO::DeviceIoControl;

use super::get_last_error_message;

pub fn create_file_handle(path: &str, access: FILE_ACCESS_FLAGS) -> Result<HANDLE> {
    unsafe {
        CreateFileW(
            &HSTRING::from(path),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES::default(),
            None,
        )
        .with_context(|| format!("CreateFileW failed for '{path}'"))
    }
}

pub fn offset_file_pointer(handle: HANDLE, move_distance: i64) -> Result<()> {
    let low = move_distance as i32;
    let mut high = (move_distance >> 32) as i32;
    if unsafe { SetFilePointer(handle, low, Some(&mut high as *mut i32), FILE_BEGIN) }
        == INVALID_SET_FILE_POINTER
    {
        Err(eyre!(
            "SetFilePointer failed with '{}'",
            get_last_error_message().unwrap()
        ))
    } else {
        Ok(())
    }
}

pub fn query_volume_data(handle: HANDLE) -> Result<NTFS_VOLUME_DATA_BUFFER> {
    let data = NTFS_VOLUME_DATA_BUFFER::default();
    let res = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_GET_NTFS_VOLUME_DATA,
            None,
            0,
            Some(&data as *const NTFS_VOLUME_DATA_BUFFER as *mut _),
            std::mem::size_of_val(&data) as u32,
            None,
            None,
        )
    };

    if !res.as_bool() {
        return Err(eyre!(
            "DeviceIoControl failed with '{}'",
            get_last_error_message().unwrap()
        ));
    }

    Ok(data)
}

pub fn get_volumes() -> Vec<char> {
    let mut buf = [0u16; 255];

    ('a'..='z')
        .filter(|c| {
            unsafe {
                GetVolumeNameForVolumeMountPointW(&HSTRING::from(&format!("{}:\\", c)), &mut buf)
            }
            .as_bool()
        })
        .collect()
}
