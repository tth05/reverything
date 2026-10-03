//! Locating the $MFT on disk and reading it in parallel.

use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};

use eyre::{bail, ensure, eyre, Context, Result};
use windows::Win32::System::Ioctl::NTFS_VOLUME_DATA_BUFFER;

use crate::ntfs::io::{begin_read, finish_read, read_at, AlignedBuf, Event, Handle, PendingRead};
use crate::ntfs::record::{
    apply_fixup, attribute_list_records, attributes, Attr, Run, ATTR_ATTRIBUTE_LIST, ATTR_BITMAP,
    ATTR_DATA,
};

/// A contiguous piece of the $MFT on disk.
#[derive(Debug, Copy, Clone)]
struct Extent {
    mft_offset: u64,
    disk_offset: u64,
    len: u64,
}

pub struct MftLayout {
    pub record_size: usize,
    pub record_count: u64,
    cluster_size: u64,
    sector_size: usize,
    extents: Vec<Extent>,
    /// $MFT:$BITMAP, one bit per record. `None` if it could not be read.
    bitmap: Option<Vec<u8>>,
}

#[derive(Default)]
struct MftAttributes {
    data_pieces: Vec<(u64, Vec<Run>)>,
    data_size: Option<u64>,
    bitmap: Option<BitmapAttr>,
    attribute_list: Option<Vec<u8>>,
}

enum BitmapAttr {
    Resident(Vec<u8>),
    NonResident(Vec<Run>, u64),
}

impl MftLayout {
    pub fn read(handle: &Handle, vd: &NTFS_VOLUME_DATA_BUFFER) -> Result<Self> {
        let record_size = vd.BytesPerFileRecordSegment as usize;
        let cluster_size = vd.BytesPerCluster as u64;
        let sector_size = vd.BytesPerSector as usize;
        ensure!(
            record_size >= 1024 && cluster_size > 0,
            "Unexpected volume geometry"
        );

        let mut layout = MftLayout {
            record_size,
            record_count: 0,
            cluster_size,
            sector_size,
            extents: Vec::new(),
            bitmap: None,
        };

        // Record 0 is $MFT itself and describes where the rest of the table is.
        let mft_start = vd.MftStartLcn as u64 * cluster_size;
        let mut rec0 = layout.read_raw(handle, mft_start, record_size)?;
        ensure!(apply_fixup(&mut rec0, true), "$MFT record is corrupt");

        let mut attrs = MftAttributes::default();
        collect_mft_attributes(handle, &layout, &rec0, &mut attrs)?;

        // A very fragmented $MFT stores the rest of its data runs in extension records, listed in
        // the $ATTRIBUTE_LIST. Those records are near the start of the table, which the runs from
        // record 0 already cover.
        if let Some(list) = attrs.attribute_list.take() {
            layout.extents = build_extents(&attrs.data_pieces, cluster_size)?;
            for ext in attribute_list_records(&list, &[ATTR_DATA, ATTR_BITMAP], 0) {
                let offset = layout
                    .disk_offset(ext * record_size as u64)
                    .ok_or_else(|| eyre!("$MFT extension record {} is not mapped", ext))?;
                let mut rec = layout.read_raw(handle, offset, record_size)?;
                ensure!(
                    apply_fixup(&mut rec, true),
                    "$MFT extension record is corrupt"
                );
                collect_mft_attributes(handle, &layout, &rec, &mut attrs)?;
            }
        }

        layout.extents = build_extents(&attrs.data_pieces, cluster_size)?;
        let data_size = attrs
            .data_size
            .ok_or_else(|| eyre!("$MFT has no $DATA attribute"))?;
        let mapped = layout.extents.last().map_or(0, |e| e.mft_offset + e.len);
        layout.record_count = data_size.min(mapped) / record_size as u64;

        layout.bitmap = match attrs.bitmap {
            Some(BitmapAttr::Resident(v)) => Some(v),
            Some(BitmapAttr::NonResident(runs, size)) => {
                Some(read_runs(handle, &layout, &runs, size)?)
            }
            None => None,
        };

        Ok(layout)
    }

    /// Reads `len` bytes at `offset` with the alignment unbuffered I/O requires.
    fn read_raw(&self, handle: &Handle, offset: u64, len: usize) -> Result<Vec<u8>> {
        let aligned = len.next_multiple_of(self.sector_size);
        let mut buf = AlignedBuf::new(aligned)?;
        read_at(handle, offset, &mut buf)?;
        Ok(buf[..len].to_vec())
    }

    fn disk_offset(&self, mft_offset: u64) -> Option<u64> {
        self.extents
            .iter()
            .find(|e| (e.mft_offset..e.mft_offset + e.len).contains(&mft_offset))
            .map(|e| e.disk_offset + (mft_offset - e.mft_offset))
    }

    #[inline]
    pub fn is_in_use(&self, record: u64) -> bool {
        match &self.bitmap {
            Some(b) => b
                .get((record / 8) as usize)
                .is_some_and(|byte| byte & (1 << (record % 8)) != 0),
            None => true,
        }
    }

    pub fn used_record_count(&self) -> u64 {
        match &self.bitmap {
            Some(b) => {
                let full = (self.record_count / 8) as usize;
                let tail = (full as u64 * 8..self.record_count).filter(|&r| self.is_in_use(r));
                b[..full.min(b.len())]
                    .iter()
                    .map(|x| x.count_ones() as u64)
                    .sum::<u64>()
                    + tail.count() as u64
            }
            None => self.record_count,
        }
    }

    /// Splits the table into chunks of at most `max_records` records, skipping and trimming runs
    /// of unused records according to the bitmap.
    pub fn plan_chunks(&self, max_records: u64) -> Vec<Range<u64>> {
        let n = self.record_count;
        let mut chunks = Vec::with_capacity((n / max_records) as usize + 1);
        let mut r = 0;

        while r < n {
            // Skip whole free bytes of the bitmap quickly
            if let Some(b) = &self.bitmap {
                while r % 8 == 0 && r < n && b.get((r / 8) as usize) == Some(&0) {
                    r += 8;
                }
            }
            if r >= n {
                break;
            }
            if !self.is_in_use(r) {
                r += 1;
                continue;
            }

            let limit = (r + max_records).min(n);
            let mut end = limit;
            while end > r + 1 && !self.is_in_use(end - 1) {
                end -= 1;
            }
            chunks.push(r..end);
            r = limit;
        }

        chunks
    }

    /// Disk segments `(disk_offset, buffer_offset, len)` that make up a record range.
    fn segments(&self, records: &Range<u64>, out: &mut Vec<(u64, usize, usize)>) {
        out.clear();
        let rs = self.record_size as u64;
        let mut pos = records.start * rs;
        let end = records.end * rs;

        let mut i = self
            .extents
            .partition_point(|e| e.mft_offset + e.len <= pos);
        while pos < end {
            let e = self.extents[i];
            let len = (e.mft_offset + e.len).min(end) - pos;
            out.push((
                e.disk_offset + (pos - e.mft_offset),
                (pos - records.start * rs) as usize,
                len as usize,
            ));
            pos += len;
            i += 1;
        }
    }

    /// Reads all chunks with `threads` workers. Each worker keeps two reads in flight (one being
    /// parsed, one being read) so parsing overlaps with I/O. `f` gets the chunk index, its record
    /// range and the raw (not yet fixed up) records.
    pub fn read_parallel<F>(
        &self,
        handle: &Handle,
        chunks: &[Range<u64>],
        threads: usize,
        f: F,
    ) -> Result<()>
    where
        F: Fn(usize, Range<u64>, &mut [u8]) + Sync,
    {
        let max_len = chunks
            .iter()
            .map(|c| (c.end - c.start) as usize)
            .max()
            .unwrap_or(0)
            * self.record_size;
        let next = AtomicUsize::new(0);
        let claim = || {
            let i = next.fetch_add(1, Ordering::Relaxed);
            (i < chunks.len()).then_some(i)
        };

        std::thread::scope(|s| {
            let workers = (0..threads.max(1))
                .map(|_| {
                    s.spawn(|| -> Result<()> {
                        let mut slots = [ReadSlot::new(max_len)?, ReadSlot::new(max_len)?];
                        let mut segments = Vec::new();
                        let mut cur = 0;

                        if let Some(i) = claim() {
                            self.segments(&chunks[i], &mut segments);
                            slots[cur].issue(handle, i, &segments)?;
                        }

                        while let Some(i) = slots[cur].chunk {
                            let other = 1 - cur;
                            if let Some(j) = claim() {
                                self.segments(&chunks[j], &mut segments);
                                slots[other].issue(handle, j, &segments)?;
                            }

                            slots[cur].wait(handle)?;
                            let len = (chunks[i].end - chunks[i].start) as usize * self.record_size;
                            f(i, chunks[i].clone(), &mut slots[cur].buf[..len]);
                            slots[cur].chunk = None;
                            cur = other;
                        }

                        Ok(())
                    })
                })
                .collect::<Vec<_>>();

            workers
                .into_iter()
                .try_for_each(|w| w.join().map_err(|_| eyre!("MFT reader panicked"))?)
        })
    }
}

/// A buffer together with the reads currently targeting it.
struct ReadSlot {
    buf: AlignedBuf,
    events: Vec<Event>,
    pending: Vec<PendingRead>,
    chunk: Option<usize>,
}

impl ReadSlot {
    fn new(len: usize) -> Result<Self> {
        Ok(Self {
            buf: AlignedBuf::new(len.max(4096))?,
            events: Vec::new(),
            pending: Vec::new(),
            chunk: None,
        })
    }

    fn issue(
        &mut self,
        handle: &Handle,
        chunk: usize,
        segments: &[(u64, usize, usize)],
    ) -> Result<()> {
        while self.events.len() < segments.len() {
            self.events.push(Event::new()?);
        }
        self.chunk = Some(chunk);
        for (k, &(disk_offset, buf_offset, len)) in segments.iter().enumerate() {
            let read = unsafe {
                begin_read(
                    handle,
                    disk_offset,
                    self.buf.as_mut_ptr().add(buf_offset),
                    len,
                    &self.events[k],
                )?
            };
            self.pending.push(read);
        }
        Ok(())
    }

    fn wait(&mut self, handle: &Handle) -> Result<()> {
        let mut result = Ok(());
        for read in self.pending.drain(..) {
            let r = finish_read(handle, read);
            if result.is_ok() {
                result = r;
            }
        }
        result
    }
}

impl Drop for ReadSlot {
    fn drop(&mut self) {
        // The kernel may still be writing into the buffer if we bailed out early
        for read in self.pending.drain(..) {
            read.wait_event();
        }
    }
}

fn collect_mft_attributes(
    handle: &Handle,
    layout: &MftLayout,
    rec: &[u8],
    out: &mut MftAttributes,
) -> Result<()> {
    for attr in attributes(rec) {
        if attr.name_len() != 0 {
            continue;
        }
        match attr.kind {
            ATTR_DATA if attr.non_resident() => {
                let runs = attr.runs().ok_or_else(|| eyre!("Invalid $MFT data runs"))?;
                if attr.starting_vcn() == 0 {
                    out.data_size = Some(attr.real_size());
                }
                out.data_pieces.push((attr.starting_vcn(), runs));
            }
            ATTR_BITMAP => {
                out.bitmap = Some(if attr.non_resident() {
                    let runs = attr
                        .runs()
                        .ok_or_else(|| eyre!("Invalid $MFT bitmap runs"))?;
                    BitmapAttr::NonResident(runs, attr.real_size())
                } else {
                    BitmapAttr::Resident(attr.value().unwrap_or_default().to_vec())
                });
            }
            ATTR_ATTRIBUTE_LIST if out.attribute_list.is_none() => {
                out.attribute_list = Some(read_attribute_value(handle, layout, &attr)?);
            }
            _ => {}
        }
    }
    Ok(())
}

fn read_attribute_value(handle: &Handle, layout: &MftLayout, attr: &Attr) -> Result<Vec<u8>> {
    if !attr.non_resident() {
        return Ok(attr.value().unwrap_or_default().to_vec());
    }
    let runs = attr.runs().ok_or_else(|| eyre!("Invalid data runs"))?;
    read_runs(handle, layout, &runs, attr.real_size())
}

fn build_extents(pieces: &[(u64, Vec<Run>)], cluster_size: u64) -> Result<Vec<Extent>> {
    let mut pieces = pieces.iter().collect::<Vec<_>>();
    pieces.sort_by_key(|p| p.0);

    let mut extents = Vec::new();
    let mut vcn = 0u64;
    for (start_vcn, runs) in pieces {
        if *start_vcn != vcn {
            bail!(
                "Gap in $MFT data runs at vcn {} (expected {})",
                start_vcn,
                vcn
            );
        }
        for run in runs {
            let lcn = run.lcn.ok_or_else(|| eyre!("$MFT contains a sparse run"))?;
            extents.push(Extent {
                mft_offset: vcn * cluster_size,
                disk_offset: lcn * cluster_size,
                len: run.clusters * cluster_size,
            });
            vcn += run.clusters;
        }
    }
    Ok(extents)
}

fn read_runs(handle: &Handle, layout: &MftLayout, runs: &[Run], size: u64) -> Result<Vec<u8>> {
    let total = runs.iter().map(|r| r.clusters).sum::<u64>() * layout.cluster_size;
    let mut buf = AlignedBuf::new((total as usize).max(layout.sector_size))?;
    let mut off = 0usize;
    for run in runs {
        let len = (run.clusters * layout.cluster_size) as usize;
        match run.lcn {
            Some(lcn) => read_at(handle, lcn * layout.cluster_size, &mut buf[off..off + len])
                .with_context(|| "Failed to read non-resident attribute")?,
            None => buf[off..off + len].fill(0),
        }
        off += len;
    }
    Ok(buf[..(size.min(total)) as usize].to_vec())
}
