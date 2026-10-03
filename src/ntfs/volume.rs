use eyre::{Context, Result};
use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetDriveTypeW, GetVolumeInformationW, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};
use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;

use crate::ntfs::io::{ioctl_struct, Handle};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Volume {
    /// Upper case drive letter
    pub id: char,
}

impl Volume {
    /// Opens the volume. `overlapped` handles are needed for parallel reads, `no_buffering` keeps
    /// gigabytes of MFT data out of the file cache.
    pub fn open(&self, overlapped: bool, no_buffering: bool) -> Result<Handle> {
        let mut flags = FILE_FLAGS_AND_ATTRIBUTES(0);
        if overlapped {
            flags |= FILE_FLAG_OVERLAPPED;
        }
        if no_buffering {
            flags |= FILE_FLAG_NO_BUFFERING;
        }

        let raw = unsafe {
            CreateFileW(
                &HSTRING::from(format!(r"\\.\{}:", self.id)),
                FILE_GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                flags,
                None,
            )
        }
        .with_context(|| format!("Failed to open volume {}:", self.id))?;

        Ok(Handle { raw, overlapped })
    }
}

pub fn volume_data(handle: &Handle) -> Result<NTFS_VOLUME_DATA_BUFFER> {
    ioctl_struct(handle, FSCTL_GET_NTFS_VOLUME_DATA)
        .with_context(|| "FSCTL_GET_NTFS_VOLUME_DATA failed")
}

/// All fixed volumes formatted with NTFS.
pub fn ntfs_volumes() -> Vec<Volume> {
    ('A'..='Z')
        .filter(|c| {
            let root = HSTRING::from(format!(r"{}:\", c));
            let mut fs_name = [0u16; 32];
            unsafe {
                GetDriveTypeW(&root) == DRIVE_FIXED
                    && GetVolumeInformationW(&root, None, None, None, None, Some(&mut fs_name))
                        .is_ok()
                    && fs_name.starts_with(&"NTFS".encode_utf16().collect::<Vec<_>>())
                    && fs_name[4] == 0
            }
        })
        .map(|id| Volume { id })
        .collect()
}
