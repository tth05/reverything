use std::ops::Range;

const ATTRIBUTE_DATA_OFFSET: usize = 0x18;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u32)]
pub enum AttributeType {
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

pub struct Attribute<'a> {
    pub header: &'a AttributeHeader,
    pub data: &'a [u8],
}

impl<'a> Attribute<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        unsafe {
            let header = (data.as_ptr() as *const AttributeHeader).as_ref().unwrap();
            Attribute { header, data }
        }
    }

    pub fn decode_data_runs(&self, bytes_per_cluster: usize) -> Option<(usize, Vec<Range<usize>>)> {
        unsafe {
            let attribute_type = self.header.attribute_type;
            if attribute_type != AttributeType::Data || !self.header.non_resident {
                return None;
            }

            let mut data_runs = Vec::new();
            let mut offset = self.header.last.non_resident.data_runs_offset as usize;
            let mut total_size = 0usize;
            let mut previous_offset = 0usize;

            while self.data[offset] != 0 {
                // Read header
                let cluster_count_size = (self.data[offset] & 0xF) as usize;
                let cluster_offset_size = (self.data[offset] >> 4) as usize;
                offset += 1;

                // Read run length
                let mut buf: [u8; 8] = [0; 8];
                buf[..cluster_count_size]
                    .copy_from_slice(&self.data[offset..offset + cluster_count_size]);
                let cluster_count = i64::from_le_bytes(buf);

                offset += cluster_count_size;

                // Read run offset
                buf[..cluster_offset_size]
                    .copy_from_slice(&self.data[offset..offset + cluster_offset_size]);
                let cluster_offset = i64::from_le_bytes(buf);

                offset += cluster_offset_size;

                // Create range
                let start = if cluster_offset >= 0 {
                    previous_offset + (cluster_offset as usize * bytes_per_cluster)
                } else {
                    previous_offset - ((-cluster_offset) as usize * bytes_per_cluster)
                };
                previous_offset = start;

                let run_size = cluster_count as usize * bytes_per_cluster;
                data_runs.push(start..start + run_size);
                total_size += run_size;
            }

            Some((total_size, data_runs))
        }
    }
}

#[repr(C, packed)]
pub struct AttributeHeader {
    pub attribute_type: AttributeType,
    pub length: u32,
    pub non_resident: bool,
    pub name_length: u8,
    pub name_offset: u16,
    pub flags: u16,
    pub attribute_id: u16,
    pub last: AttributeHeader2,
}

#[repr(C)]
pub union AttributeHeader2 {
    pub resident: ResidentAttributeHeader,
    pub non_resident: NonResidentAttributeHeader,
}

#[derive(Debug, Copy, Clone)]
#[repr(C, packed)]
pub struct ResidentAttributeHeader {
    pub value_length: u32,
    pub value_offset: u16,
    pub indexed_flag: u8,
}

#[derive(Debug, Copy, Clone)]
#[repr(C, packed)]
pub struct NonResidentAttributeHeader {
    pub starting_vcn: u64,
    pub ending_vcn: u64,
    pub data_runs_offset: u16,
    pub compression_unit_size: u16,
    pub padding: [u8; 4],
    pub allocated_size: u64,
    pub real_size: u64,
    pub initialized_size: u64,
}
