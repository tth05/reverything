use std::time::Instant;

use eyre::{eyre, ContextCompat, Result};
use rayon::prelude::*;
use widestring::Utf16Str;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{ReadFile, FILE_GENERIC_READ, FILE_READ_DATA};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::ntfs::file_record::FileRecord;
use crate::ntfs::get_last_error_message;
use crate::ntfs::volume::{create_file_handle, create_overlapped, get_volumes, query_volume_data};

mod ntfs;

fn main() -> Result<()> {
    let t = Instant::now();
    unsafe {
        let vol = *get_volumes()
            .first()
            .with_context(|| "Cannot find first volume")?;

        let handle = create_file_handle(&format!("\\\\.\\{}:", vol), FILE_GENERIC_READ)?;
        let data = query_volume_data(handle)?;

        // Move to $MFT start
        let mut ov = create_overlapped(data.BytesPerCluster as usize * data.MftStartLcn as usize);
        let event_handle = CreateEventW(None, false, false, None).expect("CreateEventW failed");
        ov.hEvent = event_handle;

        let mut mft_file_buf = Vec::<u8>::with_capacity(data.BytesPerFileRecordSegment as usize);

        ReadFile(
            handle,
            Some(mft_file_buf.as_mut_ptr() as *mut _),
            mft_file_buf.capacity() as u32,
            None,
            Some(&ov as *const _ as *mut _),
        );

        if WaitForSingleObject(event_handle, 1000) != WAIT_OBJECT_0 {
            return Err(eyre!(
                "ReadFile failed '{:?}'",
                get_last_error_message().unwrap()
            ));
        }

        mft_file_buf.set_len(mft_file_buf.capacity());
        FileRecord::fixup(&mut mft_file_buf, data.BytesPerSector as usize);

        let mft_file = FileRecord::new(&mft_file_buf);
        let mut mft_data = mft_file
            .read_data_attribute(handle, data.BytesPerCluster as usize)
            .unwrap();

        // let mut names: Vec<&Utf16Str> =
        //     Vec::with_capacity(mft_data.capacity() / data.BytesPerFileRecordSegment as usize);

        let mut names: Vec<&Utf16Str> = mft_data
            .par_chunks_mut(data.BytesPerFileRecordSegment as usize)
            .filter_map(|chunk| {
                let record = FileRecord::new(chunk);
                // Should be fine to determine without fixup
                if !record.is_valid() || !record.is_used() {
                    return None;
                }

                FileRecord::fixup(chunk, data.BytesPerSector as usize);
                let record = FileRecord::new(chunk);
                record.get_file_name().map(|name| std::mem::transmute::<&'_ _, &'static _>(name))
            }).collect();

        names.iter().find(|&n| n == "idea64.exe").unwrap();
        println!("{:?} / {:?}", names.len(), mft_data.capacity() / data.BytesPerFileRecordSegment as usize);
        assert!(CloseHandle(handle).as_bool());
    }

    println!("Elapsed: {:?}", t.elapsed());

    Ok(())
}
