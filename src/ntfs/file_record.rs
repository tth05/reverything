use std::ffi::OsString;
use std::ops::Range;
use std::os::windows::ffi::OsStringExt;
use std::process::exit;

use eyre::{ContextCompat, Result};
use widestring::Utf16Str;

use crate::ntfs::file_attribute::{Attribute, AttributeType};

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

    pub fn is_used(&self) -> bool {
        self.header.flags & 0x1 != 0
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

    pub fn read_data_runs(&self, bytes_per_cluster: usize) -> Result<(usize, Vec<Range<usize>>)> {
        self.get_attribute(AttributeType::Data)
            .and_then(|a| a.decode_data_runs(bytes_per_cluster))
            .with_context(|| "Cannot find data attribute")
    }

    pub fn get_file_name_info(&self) -> Option<(u64, String)> {
        self.attributes()
            .filter(|a| {
                let attribute_type = a.header.attribute_type;
                attribute_type == AttributeType::FileName && !a.header.non_resident
            })
            .filter(|a| unsafe {
                let base = a.header.last.resident.value_offset as usize + 0x38;
                let flags = a.data[base..base + 4].align_to::<u32>().1[0];
                // Skip reparse points
                flags & 0x0400 == 0
            })
            .last()
            .map(|a| unsafe {
                let base = a.header.last.resident.value_offset as usize + 0x40;
                let length = a.data[base] as usize * 2;
                let name = &a.data[base + 2..base + 2 + length];
                let base = base - 0x40;
                let parent: u64 =
                    u64::from_le_bytes(a.data[base..base + 8].try_into().unwrap()) & 0x0000_ffff_ffff_ffff;

               /* if OsString::from_wide(name.align_to().1).to_string_lossy().to_string() {
                    println!("Invalid file name: {:?} {:?}", name, name.align_to::<u16>());
                    exit(0);
                }*/

                (
                    parent,
                    OsString::from_wide(name.align_to().1).to_string_lossy().into_owned(),
                )
            })
    }

    pub fn fixup(data: &mut [u8], sector_size: usize) {
        let file = FileRecord::new(data);
        if !file.is_valid() {
            return;
        }

        let us_offset = file.header.usa_offset as usize;
        let usa_size = file.header.usa_word_count as usize * 2;

        let start = us_offset + 2;
        let end = start + (usa_size - 2);

        let mut sector_offset = sector_size - 2;
        for offset in (start..end).step_by(2) {
            let mut buf = [0u8; 2];
            buf.copy_from_slice(&data[offset..offset + 2]);

            debug_assert!(data[sector_offset..sector_offset + 2] == data[start - 2..start]);

            data[sector_offset..sector_offset + 2].copy_from_slice(&buf);
            sector_offset += sector_size;
        }
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
