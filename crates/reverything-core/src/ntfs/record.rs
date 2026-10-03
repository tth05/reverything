//! Zero-allocation parsing of raw MFT file records.
//!
//! References: https://flatcap.github.io/linux-ntfs/ntfs/concepts/file_record.html

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_BITMAP: u32 = 0xB0;
const ATTR_END: u32 = 0xFFFF_FFFF;

pub const NAMESPACE_POSIX: u8 = 0;
pub const NAMESPACE_WIN32: u8 = 1;
pub const NAMESPACE_DOS: u8 = 2;
pub const NAMESPACE_WIN32_AND_DOS: u8 = 3;

/// File references are 48 bit record numbers followed by a 16 bit sequence number.
pub const MFT_REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// NTFS protects every 512 bytes of a record with the update sequence, independent of the
/// physical sector size.
const USA_STRIDE: usize = 512;

const RECORD_IN_USE: u16 = 0x1;
const RECORD_IS_DIRECTORY: u16 = 0x2;

#[inline]
pub fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

#[inline]
pub fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

#[inline]
pub fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Checks the magic and the in-use flag. Works before fixup because both live in the first sector.
#[inline]
pub fn is_used_record(rec: &[u8]) -> bool {
    rec.len() >= 48 && rec[0..4] == *b"FILE" && u16_at(rec, 22) & RECORD_IN_USE != 0
}

/// Restores the last two bytes of every 512 byte stride from the update sequence array.
///
/// With `validate`, returns false when a stride does not end with the update sequence number,
/// which means the record was torn while being written. Records returned by
/// `FSCTL_GET_NTFS_FILE_RECORD` are already fixed up; applying the fixup again without validation
/// is a no-op for them because the array holds the original bytes.
pub fn apply_fixup(rec: &mut [u8], validate: bool) -> bool {
    if rec.len() < 48 || rec[0..4] != *b"FILE" {
        return false;
    }

    let usa_offset = u16_at(rec, 4) as usize;
    let usa_count = u16_at(rec, 6) as usize;
    if usa_count < 2 || usa_offset + usa_count * 2 > rec.len() {
        return false;
    }

    let strides = usa_count - 1;
    if strides * USA_STRIDE > rec.len() {
        return false;
    }

    let usn = [rec[usa_offset], rec[usa_offset + 1]];
    for i in 0..strides {
        let end = (i + 1) * USA_STRIDE - 2;
        if validate && rec[end..end + 2] != usn {
            return false;
        }
        let src = usa_offset + 2 + i * 2;
        rec[end] = rec[src];
        rec[end + 1] = rec[src + 1];
    }

    true
}

/// Records served by `FSCTL_GET_NTFS_FILE_RECORD` come from NTFS' cache and are normally already
/// fixed up, in which case the update sequence array may be stale. Only apply the fixup if every
/// stride still ends with the update sequence number.
pub fn fixup_if_needed(rec: &mut [u8]) -> bool {
    if rec.len() < 48 || rec[0..4] != *b"FILE" {
        return false;
    }
    let usa_offset = u16_at(rec, 4) as usize;
    let usa_count = u16_at(rec, 6) as usize;
    if usa_count < 2 || usa_offset + 2 > rec.len() || (usa_count - 1) * USA_STRIDE > rec.len() {
        return false;
    }
    let usn = [rec[usa_offset], rec[usa_offset + 1]];
    let raw = (1..usa_count).all(|i| rec[i * USA_STRIDE - 2..i * USA_STRIDE] == usn);
    !raw || apply_fixup(rec, true)
}

/// A single attribute inside a record. All accessors are bounds checked by construction in
/// [`attributes`] (resident headers are at least 24 bytes, non-resident ones 64 bytes).
#[derive(Copy, Clone)]
pub struct Attr<'a> {
    pub kind: u32,
    pub data: &'a [u8],
}

impl<'a> Attr<'a> {
    #[inline]
    pub fn non_resident(&self) -> bool {
        self.data[8] != 0
    }

    #[inline]
    pub fn name_len(&self) -> u8 {
        self.data[9]
    }

    /// Value of a resident attribute.
    #[inline]
    pub fn value(&self) -> Option<&'a [u8]> {
        if self.non_resident() {
            return None;
        }
        let len = u32_at(self.data, 16) as usize;
        let off = u16_at(self.data, 20) as usize;
        self.data.get(off..off.checked_add(len)?)
    }

    #[inline]
    pub fn starting_vcn(&self) -> u64 {
        u64_at(self.data, 16)
    }

    #[inline]
    pub fn allocated_size(&self) -> u64 {
        u64_at(self.data, 40)
    }

    #[inline]
    pub fn real_size(&self) -> u64 {
        u64_at(self.data, 48)
    }

    /// Mapping pairs of a non-resident attribute.
    pub fn runs(&self) -> Option<Vec<Run>> {
        let off = u16_at(self.data, 32) as usize;
        decode_runs(self.data.get(off..)?)
    }
}

/// Iterates the attributes of a fixed-up record, stopping at the first malformed one.
pub fn attributes(rec: &[u8]) -> impl Iterator<Item = Attr<'_>> {
    let used = (u32_at(rec, 24) as usize).min(rec.len());
    let mut off = u16_at(rec, 20) as usize;

    std::iter::from_fn(move || {
        if off + 16 > used {
            return None;
        }
        let kind = u32_at(rec, off);
        if kind == ATTR_END {
            return None;
        }
        let len = u32_at(rec, off + 4) as usize;
        let min_len = if rec[off + 8] != 0 { 64 } else { 24 };
        if len < min_len || off + len > used {
            return None;
        }

        let attr = Attr {
            kind,
            data: &rec[off..off + len],
        };
        off += len;
        Some(attr)
    })
}

/// A data run. `lcn` is `None` for sparse runs.
#[derive(Debug, Copy, Clone)]
pub struct Run {
    pub lcn: Option<u64>,
    pub clusters: u64,
}

pub fn decode_runs(mut d: &[u8]) -> Option<Vec<Run>> {
    let mut runs = Vec::new();
    let mut lcn = 0i64;

    loop {
        let header = *d.first()?;
        if header == 0 {
            break;
        }
        let len_size = (header & 0xF) as usize;
        let off_size = (header >> 4) as usize;
        if len_size == 0 || len_size > 8 || off_size > 8 || d.len() < 1 + len_size + off_size {
            return None;
        }

        let mut buf = [0u8; 8];
        buf[..len_size].copy_from_slice(&d[1..1 + len_size]);
        let clusters = u64::from_le_bytes(buf);

        if off_size == 0 {
            runs.push(Run {
                lcn: None,
                clusters,
            });
        } else {
            let bytes = &d[1 + len_size..1 + len_size + off_size];
            // Sign extend
            let fill = if bytes[off_size - 1] & 0x80 != 0 {
                0xFF
            } else {
                0
            };
            let mut buf = [fill; 8];
            buf[..off_size].copy_from_slice(bytes);
            lcn += i64::from_le_bytes(buf);
            runs.push(Run {
                lcn: Some(lcn as u64),
                clusters,
            });
        }

        d = &d[1 + len_size + off_size..];
    }

    Some(runs)
}

#[derive(Debug, Default, Copy, Clone)]
pub struct ParsedRecord {
    pub base_record: u64,
    /// Incremented by NTFS every time the record is reused
    pub sequence: u16,
    pub is_directory: bool,
    pub has_attribute_list: bool,
    /// FILE_ATTRIBUTE_* flags from $STANDARD_INFORMATION
    pub file_attributes: u32,
    /// FILETIME values from $STANDARD_INFORMATION
    pub created: u64,
    pub modified: u64,
    /// Size of the unnamed $DATA stream, if its first piece is in this record
    pub data_size: Option<u64>,
    pub allocated_size: Option<u64>,
    /// Byte range of a resident $ATTRIBUTE_LIST value inside the record
    pub attribute_list: Option<(usize, usize)>,
}

/// Position of a $FILE_NAME inside the record. The name is `units` UTF-16 code units at `start`.
#[derive(Debug, Copy, Clone)]
pub struct NameRef {
    pub parent: u64,
    pub namespace: u8,
    pub start: usize,
    pub units: usize,
    /// Size cached in the $FILE_NAME attribute, often stale
    pub size: u64,
}

impl NameRef {
    pub fn bytes<'a>(&self, rec: &'a [u8]) -> &'a [u8] {
        &rec[self.start..self.start + self.units * 2]
    }
}

/// Parses a fixed-up record in a single pass over its attributes. Short (DOS) names are skipped.
/// Returns false if the record is not in use.
pub fn parse_record(rec: &[u8], out: &mut ParsedRecord, names: &mut Vec<NameRef>) -> bool {
    names.clear();
    *out = ParsedRecord::default();

    if !is_used_record(rec) {
        return false;
    }

    out.is_directory = u16_at(rec, 22) & RECORD_IS_DIRECTORY != 0;
    out.base_record = u64_at(rec, 32) & MFT_REF_MASK;
    out.sequence = u16_at(rec, 16);

    for attr in attributes(rec) {
        match attr.kind {
            ATTR_STANDARD_INFORMATION => {
                if let Some(v) = attr.value() {
                    if v.len() >= 36 {
                        out.created = u64_at(v, 0);
                        out.modified = u64_at(v, 8);
                        out.file_attributes = u32_at(v, 32);
                    }
                }
            }
            ATTR_ATTRIBUTE_LIST => {
                out.has_attribute_list = true;
                if let Some(v) = attr.value() {
                    let start = v.as_ptr() as usize - rec.as_ptr() as usize;
                    out.attribute_list = Some((start, start + v.len()));
                }
            }
            ATTR_FILE_NAME => {
                let Some(v) = attr.value() else { continue };
                if v.len() < 66 {
                    continue;
                }
                let units = v[64] as usize;
                let namespace = v[65];
                if namespace == NAMESPACE_DOS || 66 + units * 2 > v.len() {
                    continue;
                }
                names.push(NameRef {
                    parent: u64_at(v, 0) & MFT_REF_MASK,
                    namespace,
                    start: v.as_ptr() as usize - rec.as_ptr() as usize + 66,
                    units,
                    size: u64_at(v, 48),
                });
            }
            ATTR_DATA if attr.name_len() == 0 => {
                if attr.non_resident() {
                    if attr.starting_vcn() == 0 {
                        out.data_size = Some(attr.real_size());
                        out.allocated_size = Some(attr.allocated_size());
                    }
                } else if let Some(v) = attr.value() {
                    out.data_size = Some(v.len() as u64);
                    out.allocated_size = Some(0);
                }
            }
            _ => {}
        }
    }

    true
}

/// Picks the name that represents the file: the first Win32 name, falling back to the first POSIX
/// name. Other non-DOS names are additional hard links.
pub fn primary_name(names: &[NameRef]) -> Option<usize> {
    names
        .iter()
        .position(|n| n.namespace == NAMESPACE_WIN32 || n.namespace == NAMESPACE_WIN32_AND_DOS)
        .or_else(|| names.iter().position(|n| n.namespace == NAMESPACE_POSIX))
}

/// Appends a UTF-16LE name to `dst` as UTF-8, with a fast path for ASCII names.
#[inline]
pub fn push_utf16le(src: &[u8], dst: &mut Vec<u8>) {
    if src
        .as_chunks::<2>()
        .0
        .iter()
        .all(|c| c[1] == 0 && c[0] < 0x80)
    {
        dst.extend(src.as_chunks::<2>().0.iter().map(|c| c[0]));
        return;
    }

    let units = src
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]));
    for c in char::decode_utf16(units) {
        let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
        let mut buf = [0u8; 4];
        dst.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
}

/// Converts a FILETIME to seconds since the unix epoch, clamped to the u32 range.
#[inline]
pub fn filetime_to_unix(ft: u64) -> u32 {
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    (ft / 10_000_000)
        .saturating_sub(EPOCH_DIFF_SECS)
        .min(u32::MAX as u64) as u32
}

/// Records referenced by an $ATTRIBUTE_LIST that hold attributes of the given kinds, excluding
/// `own_record`.
pub fn attribute_list_records(list: &[u8], kinds: &[u32], own_record: u64) -> Vec<u64> {
    let mut records = Vec::new();
    let mut off = 0;
    while off + 26 <= list.len() {
        let kind = u32_at(list, off);
        let len = u16_at(list, off + 4) as usize;
        if len == 0 {
            break;
        }
        let record = u64_at(list, off + 16) & MFT_REF_MASK;
        if kinds.contains(&kind) && record != own_record && !records.contains(&record) {
            records.push(record);
        }
        off += len;
    }
    records
}
