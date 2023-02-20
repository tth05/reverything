use crate::ntfs::file_attribute::{Attribute, AttributeType};
use crate::ntfs::volume::offset_file_pointer;
use std::ffi::c_void;
use eyre::{Result, eyre};
use widestring::Utf16Str;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::ReadFile;
use crate::ntfs::get_last_error_message;

pub struct FileRecord<'a> {
    pub header: &'a FileRecordHeader,
    pub data: &'a [u8],
}

#[derive(Debug, Copy, Clone)]
#[repr(C, packed)]
pub struct FileRecordHeader {
    pub magic: [u8; 4],
    pub usa_offset: u16,
    pub usa_word_count: u16,
    pub log_file_sequence_number: u64,
    pub sequence_number: u16,
    pub hard_link_count: u16,
    pub first_attribute_offset: u16,
    pub flags: u16,
    pub bytes_used: u32,
    pub bytes_allocated: u32,
    pub base_file_record: u64,
    pub next_attribute_id: u16,
}

impl<'a> FileRecord<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        unsafe {
            FileRecord {
                header: (data.as_ptr() as *const FileRecordHeader).as_ref().unwrap(),
                data,
            }
        }
    }

    pub fn is_valid(&self) -> bool {
        self.header.magic == *b"FILE"
    }

    pub fn attributes(&self) -> impl Iterator<Item = Attribute> {
        AttributeIterator {
            file: self,
            offset: self.header.first_attribute_offset as usize,
        }
    }

    pub fn get_attribute(&self, attribute_type: AttributeType) -> Option<Attribute> {
        self.attributes().find(|attr| {
            let t = attr.header.attribute_type;
            t == attribute_type
        })
    }

    pub fn read_data_attribute(&self, handle: HANDLE, bytes_per_cluster: usize) -> Result<Vec<u8>> {
        let Some((total_size, runs)) = self
            .get_attribute(AttributeType::Data)
            .and_then(|a| a.decode_data_runs(bytes_per_cluster)) else {
            return Err(eyre!("Cannot find data attribute"));
        };

        let mut buf = Vec::<u8>::with_capacity(total_size);
        let mut read_offset = 0usize;
        for run in runs {
            offset_file_pointer(handle, run.start as i64)?;

            let mut just_read = 0u32;
            let res = unsafe {
                ReadFile(
                    handle,
                    Some((buf.as_mut_ptr() as *mut c_void).add(read_offset)),
                    run.len() as u32,
                    Some(&mut just_read as *mut u32),
                    None,
                )
            };

            if !res.as_bool() {
                return Err(eyre!("ReadFile failed"));
            }

            println!("just_read: {} {} {:?} {:?}", res.0, just_read, run, run.len());

            read_offset += run.len();
        }

        unsafe { buf.set_len(total_size) };
        Ok(buf)
    }

    pub fn get_file_name(&self) -> Option<&Utf16Str> {
        self.attributes().filter(|a| {
            let attribute_type = a.header.attribute_type;
            attribute_type == AttributeType::FileName && !a.header.non_resident
        }).filter_map(|a| unsafe {
            let base = a.header.last.resident.value_offset as usize + 0x38;
            let flags = a.data[base..base + 4].align_to::<u32>().1[0];
            if flags & 0x0400 != 0 {
                return None;
            }

            let base = a.header.last.resident.value_offset as usize + 0x40;
            let length = a.data[base] as usize * 2;
            let name = &a.data[base + 2..base + 2 + length];

            Some(Utf16Str::from_slice_unchecked(name.align_to::<u16>().1))
        }).last()
    }
}

struct AttributeIterator<'a> {
    file: &'a FileRecord<'a>,
    offset: usize,
}

impl<'a> Iterator for AttributeIterator<'a> {
    type Item = Attribute<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.file.header.bytes_used as usize {
            return None;
        }

        let data = self.file.data;

        let attr = Attribute::new(&data[self.offset..]);
        let attr_header = attr.header;

        let attribute_type = attr_header.attribute_type;
        if attribute_type == AttributeType::End {
            return None;
        }

        self.offset += attr_header.length as usize;
        Some(attr)
    }
}
