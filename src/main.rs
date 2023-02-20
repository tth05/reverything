use eyre::{eyre, ContextCompat, Result, WrapErr};
use std::fs::File;
use std::io::BufReader;
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetVolumeNameForVolumeMountPointW, ReadFile, SetFilePointer, FILE_ACCESS_FLAGS,
    FILE_BEGIN, FILE_FLAGS_AND_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_READ, FILE_SHARE_WRITE,
    INVALID_SET_FILE_POINTER, OPEN_EXISTING,
};
use windows::Win32::System::Diagnostics::Debug::{FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM};
use windows::Win32::System::Ioctl::NTFS_VOLUME_DATA_BUFFER;
use windows::Win32::System::IO::DeviceIoControl;

const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x00090064;

#[derive(Debug, Copy, Clone)]
#[repr(C, packed)]
struct FileRecordHeader {
    magic: [u8; 4],
    usa_offset: u16,
    usa_word_count: u16,
    log_file_sequence_number: u64,
    sequence_number: u16,
    hard_link_count: u16,
    first_attribute_offset: u16,
    flags: u16,
    bytes_used: u32,
    bytes_allocated: u32,
    base_file_record: u64,
    next_attribute_id: u16,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u32)]
enum AttributeType {
    StandardInformation = 0x10,
    AttributeList = 0x20,
    FileName = 0x30,
    ObjectId = 0x40,
    SecurityDescriptor = 0x50,
    VolumeName = 0x60,
    VolumeInformation = 0x70,
    Data = 0x80,
    IndexRoot = 0x90,
    IndexAllocation = 0xA0,
    Bitmap = 0xB0,
    ReparsePoint = 0xC0,
    EAInformation = 0xD0,
    EA = 0xE0,
    PropertySet = 0xF0,
    LoggedUtilityStream = 0x100,
    End = 0xFFFFFFFF,
}

const ATTRIBUTE_DATA_OFFSET: usize = 0x18;

#[repr(C, packed)]
struct AttributeHeader {
    attribute_type: AttributeType,
    length: u32,
    non_resident: bool,
    name_length: u8,
    name_offset: u16,
    flags: u16,
    attribute_id: u16,
}

fn main() -> Result<()> {
    unsafe {
        let vol = *get_volumes()
            .first()
            .with_context(|| "Cannot find first volume")?;

        let handle = create_file_handle(&format!("\\\\.\\{}:", vol), FILE_READ_DATA)?;
        let data = query_volume_data(handle)?;
        // Move to $MFT start
        offset_file_pointer(handle, data.BytesPerCluster as i64 * data.MftStartLcn)?;

        let mut file_buf = Vec::<u8>::with_capacity(data.BytesPerFileRecordSegment as usize);

        let read = 0usize;
        let res = ReadFile(
            handle,
            Some(file_buf.as_mut_ptr() as *mut _),
            file_buf.capacity() as u32,
            Some(&read as *const usize as *mut _),
            None,
        );
        if !res.as_bool() {
            return Err(eyre!(
                "ReadFile failed with '{}'",
                get_last_error_message().unwrap()
            ));
        }

        file_buf.set_len(read);
        let header = (file_buf.as_ptr() as *const FileRecordHeader)
            .as_ref()
            .unwrap();
        println!(
            "File Record Magic: {:?}",
            String::from_utf8(header.magic.to_vec())?
        ); // "FILE" !!
        println!("File Record Header: {:?}", header);

        let mut file_buf = &file_buf[header.first_attribute_offset as usize..];

        loop {
            let attr_header = (file_buf.as_ptr() as *const AttributeHeader)
                .as_ref()
                .unwrap();

            let attribute_type = attr_header.attribute_type;
            if attribute_type == AttributeType::End {
                break;
            }

            if attribute_type == AttributeType::FileName {
                let length = file_buf[ATTRIBUTE_DATA_OFFSET + 0x40] as usize;
                println!("File Name Length: {}", length);
                let name = String::from_utf8(file_buf[(ATTRIBUTE_DATA_OFFSET + 0x42)..(ATTRIBUTE_DATA_OFFSET + 0x42 + length * 2)].to_vec())?;
                println!("File Name: {}", name);
            }

            println!("Attribute Type: {:?} {}", attribute_type, attr_header.non_resident);
            file_buf = &file_buf[attr_header.length as usize..];
        }

        assert!(CloseHandle(handle).as_bool());
    }

    Ok(())
}

unsafe fn offset_file_pointer(handle: HANDLE, move_distance: i64) -> Result<()> {
    let low = move_distance as i32;
    let mut high = (move_distance >> 32) as i32;
    if SetFilePointer(handle, low, Some(&mut high as *mut i32), FILE_BEGIN)
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

unsafe fn query_volume_data(handle: HANDLE) -> Result<NTFS_VOLUME_DATA_BUFFER> {
    let data = NTFS_VOLUME_DATA_BUFFER::default();
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

    if !res.as_bool() {
        return Err(eyre!(
            "DeviceIoControl failed with '{}'",
            get_last_error_message().unwrap()
        ));
    }

    Ok(data)
}

unsafe fn create_file_handle(path: &str, access: FILE_ACCESS_FLAGS) -> Result<HANDLE> {
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

unsafe fn get_volumes() -> Vec<char> {
    let mut buf = [0u16; 255];

    ('a'..='z')
        .filter(|c| {
            GetVolumeNameForVolumeMountPointW(&HSTRING::from(&format!("{}:\\", c)), &mut buf)
                .as_bool()
        })
        .collect()
}

unsafe fn get_last_error_message() -> Result<String> {
    let mut buf = [0u16; 500];
    let err = GetLastError().0;
    let n = FormatMessageW(
        FORMAT_MESSAGE_FROM_SYSTEM,
        None,
        err,
        0,
        PWSTR(buf.as_mut_ptr()),
        buf.len() as u32,
        None,
    );

    if n == 0 {
        return Err(eyre!("FormatMessageW failed: {}", GetLastError().0));
    }

    Ok(format!("(0x{:x?}) {}", err, u16_buf_to_string(&buf)?))
}

fn u16_buf_to_string(buf: &[u16]) -> Result<String> {
    Ok(String::from_utf16_lossy(
        &buf[..buf
            .iter()
            .position(|&x| x == 0)
            .ok_or(eyre!("buf is not null-terminated"))?],
    ))
}
