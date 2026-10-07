//! Building a [`VolumeIndex`] by reading the $MFT directly.

use std::ops::Range;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use eyre::Result;
use rayon::prelude::*;

use crate::index::{
    Links, Records, VolumeIndex, ATTRIBUTE_MASK, FLAG_DIRECTORY, FLAG_HAS_LINKS, FLAG_IN_USE,
    FLAG_SIZE_GUESSED,
};
use crate::ntfs::layout::MftLayout;
use crate::ntfs::record::{
    apply_fixup, filetime_to_unix, is_used_record, parse_record, primary_name, push_utf16le,
    NameRef, ParsedRecord,
};
use crate::ntfs::usn::query_journal;
use crate::ntfs::volume::{volume_data, Volume};

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub threads: usize,
    pub chunk_bytes: usize,
    pub no_buffering: bool,
    /// Off only for benchmarks that measure reading alone; the index stays empty then
    pub parse: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            // Measured on an NVMe drive: few large requests are much faster than many small ones
            // (~2 GB/s with 3x32 MB vs ~0.6 GB/s with 16x1 MB). Parsing is cheap in comparison.
            threads: 3,
            chunk_bytes: 32 * 1024 * 1024,
            no_buffering: true,
            parse: true,
        }
    }
}

impl ScanOptions {
    /// Defaults, overridable with `RV_THREADS`, `RV_CHUNK_KB` and `RV_NOBUF` for benchmarking.
    pub fn from_env() -> Self {
        let mut opts = Self::default();
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
        };
        if let Some(t) = var("RV_THREADS") {
            opts.threads = t.max(1);
        }
        if let Some(kb) = var("RV_CHUNK_KB") {
            opts.chunk_bytes = kb.max(4) * 1024;
        }
        if let Some(b) = var("RV_NOBUF") {
            opts.no_buffering = b != 0;
        }
        if let Some(p) = var("RV_PARSE") {
            opts.parse = p != 0;
        }
        opts
    }
}

#[derive(Debug, Default, Clone)]
pub struct ScanStats {
    pub open: Duration,
    pub read_parse: Duration,
    pub merge: Duration,
    pub sort: Duration,
    pub folder_sizes: Duration,
    pub chunks: usize,
    pub bytes_read: u64,
    pub used_records: u64,
    pub extension_records: usize,
    pub links: usize,
}

impl ScanStats {
    pub fn total(&self) -> Duration {
        self.open + self.read_parse + self.merge + self.sort + self.folder_sizes
    }
}

/// Output of one chunk. Name offsets in the record arrays are relative to `names` until merged.
struct ChunkOut {
    index: usize,
    records: Range<u64>,
    names: Vec<u8>,
    /// Additional names: record, parent, chunk relative name offset and length
    links: Vec<(u32, u32, u32, u16)>,
    extensions: Vec<Extension>,
}

/// Extension records hold attributes that did not fit into their base record.
struct Extension {
    base: u32,
    names: Vec<(u32, Vec<u8>)>,
    data_size: Option<u64>,
}

/// Raw pointers into [`Records`] so chunks parsed on different threads can fill their own
/// disjoint record ranges without locking.
struct RecordWriter {
    name_off: *mut u32,
    name_len: *mut u16,
    parent: *mut u32,
    flags: *mut u32,
    size: *mut u64,
    created: *mut u32,
    modified: *mut u32,
    sequence: *mut u16,
}

unsafe impl Send for RecordWriter {}
unsafe impl Sync for RecordWriter {}

impl RecordWriter {
    fn new(r: &mut Records) -> Self {
        Self {
            name_off: r.name_off.as_mut_ptr(),
            name_len: r.name_len.as_mut_ptr(),
            parent: r.parent.as_mut_ptr(),
            flags: r.flags.as_mut_ptr(),
            size: r.size.as_mut_ptr(),
            created: r.created.as_mut_ptr(),
            modified: r.modified.as_mut_ptr(),
            sequence: r.sequence.as_mut_ptr(),
        }
    }

    unsafe fn add_name_base(&self, i: usize, base: u32) {
        *self.name_off.add(i) += base;
    }
}

pub fn scan_volume(volume: Volume, opts: &ScanOptions) -> Result<(VolumeIndex, ScanStats)> {
    let mut stats = ScanStats::default();
    let t = Instant::now();

    let handle = volume.open(true, opts.no_buffering)?;
    let vd = volume_data(&handle)?;
    // Taken before reading so changes made during the scan are replayed from the journal later
    let journal = query_journal(&handle).ok();
    let layout = MftLayout::read(&handle, &vd)?;
    let chunk_records = (opts.chunk_bytes / layout.record_size).max(1) as u64;
    let chunks = layout.plan_chunks(chunk_records);

    stats.chunks = chunks.len();
    stats.bytes_read =
        chunks.iter().map(|c| c.end - c.start).sum::<u64>() * layout.record_size as u64;
    stats.open = t.elapsed();

    // Read and parse
    let t = Instant::now();
    let n = layout.record_count as usize;
    let mut records = Records::with_len(n);
    let writer = RecordWriter::new(&mut records);
    let outs = Mutex::new(Vec::with_capacity(chunks.len()));

    layout.read_parallel(&handle, &chunks, opts.threads, |index, range, buf| {
        if opts.parse {
            let out = unsafe { parse_chunk(&layout, &writer, index, range, buf) };
            outs.lock().unwrap().push(out);
        }
    })?;
    drop(handle);
    stats.read_parse = t.elapsed();

    // Merge the per chunk name buffers, links and extension records
    let t = Instant::now();
    let mut outs = outs.into_inner().unwrap();
    outs.sort_unstable_by_key(|o| o.index);

    let total = outs.iter().map(|o| o.names.len()).sum::<usize>();
    let mut names = Vec::with_capacity(total);
    let mut bases = Vec::with_capacity(outs.len());
    for out in &outs {
        bases.push(names.len() as u32);
        names.extend_from_slice(&out.names);
    }
    let w = &writer;
    outs.par_iter().zip(&bases).for_each(|(out, &base)| {
        for r in out.records.clone() {
            unsafe { w.add_name_base(r as usize, base) };
        }
    });

    let mut links = Links::default();
    for (out, &base) in outs.iter().zip(&bases) {
        for &(record, parent, off, len) in &out.links {
            links.push(record, parent, base + off, len);
        }
    }
    for ext in outs.iter_mut().flat_map(|o| o.extensions.drain(..)) {
        stats.extension_records += 1;
        merge_extension(&mut records, &mut links, &mut names, ext);
    }
    drop(outs);

    // Records whose name was never found are not usable
    records
        .flags
        .par_iter_mut()
        .zip(&records.name_len)
        .for_each(|(f, &len)| {
            if len == 0 {
                *f &= !FLAG_IN_USE;
            }
        });
    for &r in &links.record {
        records.flags[r as usize] |= FLAG_HAS_LINKS;
    }
    stats.used_records = layout.used_record_count();
    stats.links = links.len();
    stats.merge = t.elapsed();

    let t = Instant::now();
    let mut index = VolumeIndex {
        volume,
        volume_serial: vd.VolumeSerialNumber as u64,
        journal_id: journal.map_or(0, |j| j.id),
        next_usn: journal.map_or(0, |j| j.next_usn),
        record_size: layout.record_size as u32,
        records,
        links,
        names,
        sorted: Default::default(),
        garbage: 0,
        locations: Default::default(),
    };
    index.sort_and_compact();
    stats.sort = t.elapsed();

    let t = Instant::now();
    index.compute_folder_sizes();
    stats.folder_sizes = t.elapsed();

    Ok((index, stats))
}

/// # Safety
/// Only one call may write to a given record range at a time.
unsafe fn parse_chunk(
    layout: &MftLayout,
    w: &RecordWriter,
    index: usize,
    range: Range<u64>,
    buf: &mut [u8],
) -> ChunkOut {
    let mut names = Vec::with_capacity((range.end - range.start) as usize * 24);
    let mut links = Vec::new();
    let mut extensions = Vec::new();
    let mut parsed = ParsedRecord::default();
    let mut refs = Vec::with_capacity(4);

    for (k, rec) in buf.chunks_exact_mut(layout.record_size).enumerate() {
        let r = range.start + k as u64;
        if !layout.is_in_use(r)
            || !is_used_record(rec)
            || !apply_fixup(rec, true)
            || !parse_record(rec, &mut parsed, &mut refs)
        {
            continue;
        }
        let rec = &*rec;
        refs.retain(|n| n.parent <= u32::MAX as u64);

        if parsed.base_record != 0 {
            extensions.push(Extension {
                base: parsed.base_record as u32,
                names: refs
                    .iter()
                    .map(|n| {
                        let mut name = Vec::new();
                        push_utf16le(n.bytes(rec), &mut name);
                        (n.parent as u32, name)
                    })
                    .collect(),
                data_size: parsed.data_size,
            });
            continue;
        }

        let push_name = |n: &NameRef, names: &mut Vec<u8>| {
            let off = names.len();
            push_utf16le(n.bytes(rec), names);
            let len = (names.len() - off).min(u16::MAX as usize);
            names.truncate(off + len);
            (off as u32, len as u16)
        };

        // Without a name the record stays nameless (and gets dropped) unless an extension
        // record provides one
        let primary = primary_name(&refs);
        let (name_off, name_len, parent, fn_size) = match primary {
            Some(p) => {
                let (off, len) = push_name(&refs[p], &mut names);
                (off, len, refs[p].parent as u32, refs[p].size)
            }
            None => (0, 0, 0, 0),
        };
        for (j, n) in refs.iter().enumerate() {
            if Some(j) != primary {
                let (off, len) = push_name(n, &mut names);
                links.push((r as u32, n.parent as u32, off, len));
            }
        }

        let (flags, size) = flags_and_size(&parsed, fn_size);

        let i = r as usize;
        *w.name_off.add(i) = name_off;
        *w.name_len.add(i) = name_len;
        *w.parent.add(i) = parent;
        *w.flags.add(i) = flags;
        *w.size.add(i) = size;
        *w.created.add(i) = filetime_to_unix(parsed.created);
        *w.modified.add(i) = filetime_to_unix(parsed.modified);
        *w.sequence.add(i) = parsed.sequence;
    }

    ChunkOut {
        index,
        records: range,
        names,
        links,
        extensions,
    }
}

fn merge_extension(records: &mut Records, links: &mut Links, names: &mut Vec<u8>, ext: Extension) {
    let b = ext.base as usize;
    if b >= records.len() || records.flags[b] & FLAG_IN_USE == 0 {
        return;
    }

    for (parent, name) in ext.names {
        let off = names.len() as u32;
        let len = name.len().min(u16::MAX as usize) as u16;
        names.extend_from_slice(&name[..len as usize]);
        if records.name_len[b] == 0 {
            records.name_off[b] = off;
            records.name_len[b] = len;
            records.parent[b] = parent;
        } else {
            links.push(b as u32, parent, off, len);
        }
    }

    if let Some(size) = ext.data_size {
        if records.flags[b] & FLAG_SIZE_GUESSED != 0 {
            records.size[b] = size;
            records.flags[b] &= !FLAG_SIZE_GUESSED;
        }
    }
}

/// Index flags and the file size for a parsed base record. Directories get size 0.
pub fn flags_and_size(parsed: &ParsedRecord, fn_size: u64) -> (u32, u64) {
    let mut flags = FLAG_IN_USE | (parsed.file_attributes & ATTRIBUTE_MASK & !FLAG_DIRECTORY);
    let size = if parsed.is_directory {
        flags |= FLAG_DIRECTORY;
        0
    } else if let Some(size) = parsed.data_size {
        size
    } else {
        flags |= FLAG_SIZE_GUESSED;
        fn_size
    };
    (flags, size)
}
