use std::ffi::c_void;
use std::process::exit;
use std::time::Instant;

use eyre::{eyre, ContextCompat, Result};
use widestring::Utf16Str;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{ReadFile, FILE_READ_DATA, FILE_GENERIC_READ};

use crate::ntfs::file_attribute::AttributeType;
use crate::ntfs::file_record::FileRecord;
use crate::ntfs::volume::{
    create_file_handle, get_volumes, offset_file_pointer, query_volume_data,
};

mod ntfs;

fn main() -> Result<()> {
    let t = Instant::now();
    unsafe {
        let vol = *get_volumes()
            .first()
            .with_context(|| "Cannot find first volume")?;

        let handle = create_file_handle(&format!("\\\\.\\{}:", vol), FILE_READ_DATA | FILE_GENERIC_READ)?;
        let data = query_volume_data(handle)?;
        println!("{:?}", data);
        // Move to $MFT start
        offset_file_pointer(handle, data.BytesPerCluster as i64 * data.MftStartLcn)?;

        let mut mft_file_buf = Vec::<u8>::with_capacity(data.BytesPerFileRecordSegment as usize);

        let read = 0usize;
        let res = ReadFile(
            handle,
            Some(mft_file_buf.as_mut_ptr() as *mut _),
            mft_file_buf.capacity() as u32,
            Some(&read as *const usize as *mut _),
            None,
        );
        if !res.as_bool() {
            return Err(eyre!("ReadFile failed",));
        }

        mft_file_buf.set_len(read);

        let mft_file = FileRecord::new(&mft_file_buf);
        let mft_data = mft_file.read_data_attribute(handle, data.BytesPerCluster as usize).unwrap();

        let mut names: Vec<&Utf16Str> = Vec::with_capacity(mft_data.capacity() / data.BytesPerFileRecordSegment as usize);
        for offset in (0..mft_data.capacity()).step_by(data.BytesPerFileRecordSegment as usize) {
            let record = FileRecord::new(&mft_data[offset..]);
            if !record.is_valid() {
                // println!("Invalid record at offset {}", offset);
                let slice = &mft_data[offset..offset + 1024];
                if !slice.iter().all(|&b| b== 0) {
                    println!("{:?}", slice);
                    exit(0);
                }
                continue
            }

            if let Some(name) = record.get_file_name() {
                // Lifetime should be bound to "mft_data" and not "record"...
                names.push(std::mem::transmute::<&'_ _, &'static _>(name));
            }
        }

        names.iter().find(|&n| n == "21.png").unwrap();
        println!("{:?} / {:?}", names.len(), names.capacity());
        assert!(CloseHandle(handle).as_bool());
    }

    println!("Elapsed: {:?}", t.elapsed());

    Ok(())
}
