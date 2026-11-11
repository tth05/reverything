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

/// The unnamed attributes of $MFT that say where the table and its bitmap are. A large
/// non-resident attribute can be split into pieces in several records, each with the runs from
/// its starting VCN on. Only the piece at VCN 0 has the attribute's size, the others store 0.
#[derive(Default)]
struct MftAttributes {
    data_pieces: Vec<(u64, Vec<Run>)>,
    data_size: Option<u64>,
    bitmap_pieces: Vec<(u64, Vec<Run>)>,
    bitmap_size: Option<u64>,
    bitmap_resident: Option<Vec<u8>>,
    /// A piece of the bitmap had runs that could not be decoded
    bitmap_invalid: bool,
}

impl MftAttributes {
    /// The runs of the non-resident bitmap and its size, `None` if there is none.
    fn bitmap_runs(&self) -> Result<Option<(Vec<Run>, u64)>> {
        ensure!(!self.bitmap_invalid, "Invalid bitmap runs");
        if self.bitmap_pieces.is_empty() {
            return Ok(None);
        }
        let runs = join_pieces(&self.bitmap_pieces, "bitmap")?;
        let size = self
            .bitmap_size
            .ok_or_else(|| eyre!("No piece of the bitmap starts at vcn 0"))?;
        Ok(Some((runs, size)))
    }
}

/// A bitmap too short for the table would leave the records past its end out as unused.
fn check_bitmap(bitmap: &[u8], record_count: u64) -> Result<()> {
    ensure!(
        bitmap.len() as u64 * 8 >= record_count,
        "The bitmap has {} bits for {} records",
        bitmap.len() as u64 * 8,
        record_count
    );
    Ok(())
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
        let list = collect_mft_attributes(&rec0, &mut attrs)?
            .map(|attr| read_attribute_value(handle, &layout, &attr))
            .transpose()?;

        // A very fragmented $MFT stores the rest of its data and bitmap runs in extension
        // records, listed in the $ATTRIBUTE_LIST. Those records are near the start of the table,
        // which the runs from record 0 already cover.
        if let Some(list) = list {
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
                // Extension records have no list of their own
                collect_mft_attributes(&rec, &mut attrs)?;
            }
        }

        layout.extents = build_extents(&attrs.data_pieces, cluster_size)?;
        let data_size = attrs
            .data_size
            .ok_or_else(|| eyre!("$MFT has no $DATA attribute"))?;
        let mapped = layout.extents.last().map_or(0, |e| e.mft_offset + e.len);
        layout.record_count = data_size.min(mapped) / record_size as u64;

        // Without a usable bitmap every record is read, and the in-use flag in its header decides
        layout.bitmap = match layout.read_bitmap(handle, attrs) {
            Ok(bitmap) => bitmap,
            Err(e) => {
                log::warn!("The $MFT bitmap is unusable, reading every record: {:#}", e);
                None
            }
        };

        Ok(layout)
    }

    fn read_bitmap(&self, handle: &Handle, attrs: MftAttributes) -> Result<Option<Vec<u8>>> {
        let bitmap = match &attrs.bitmap_resident {
            Some(value) => value.clone(),
            None => match attrs.bitmap_runs()? {
                Some((runs, size)) => read_runs(handle, self, &runs, size)?,
                None => return Ok(None),
            },
        };
        check_bitmap(&bitmap, self.record_count)?;
        Ok(Some(bitmap))
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

/// Adds the pieces of $MFT's unnamed $DATA and $BITMAP in `rec` to `out`. Returns its
/// $ATTRIBUTE_LIST, which only record 0 has.
fn collect_mft_attributes<'a>(rec: &'a [u8], out: &mut MftAttributes) -> Result<Option<Attr<'a>>> {
    let mut list = None;
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
            ATTR_BITMAP if attr.non_resident() => {
                // Unlike broken data runs this is no reason to give up, see `bitmap_runs`
                match attr.runs() {
                    Some(runs) => out.bitmap_pieces.push((attr.starting_vcn(), runs)),
                    None => out.bitmap_invalid = true,
                }
                if attr.starting_vcn() == 0 {
                    out.bitmap_size = Some(attr.real_size());
                }
            }
            ATTR_BITMAP => {
                out.bitmap_resident = Some(attr.value().unwrap_or_default().to_vec());
            }
            ATTR_ATTRIBUTE_LIST if list.is_none() => list = Some(attr),
            _ => {}
        }
    }
    Ok(list)
}

fn read_attribute_value(handle: &Handle, layout: &MftLayout, attr: &Attr) -> Result<Vec<u8>> {
    if !attr.non_resident() {
        return Ok(attr.value().unwrap_or_default().to_vec());
    }
    let runs = attr.runs().ok_or_else(|| eyre!("Invalid data runs"))?;
    read_runs(handle, layout, &runs, attr.real_size())
}

/// The runs of an attribute's pieces in VCN order, which must follow each other without gaps.
fn join_pieces(pieces: &[(u64, Vec<Run>)], what: &str) -> Result<Vec<Run>> {
    let mut pieces = pieces.iter().collect::<Vec<_>>();
    pieces.sort_by_key(|p| p.0);

    let mut joined = Vec::new();
    let mut vcn = 0u64;
    for (start_vcn, runs) in pieces {
        if *start_vcn != vcn {
            bail!(
                "Gap in $MFT {} runs at vcn {} (expected {})",
                what,
                start_vcn,
                vcn
            );
        }
        for run in runs {
            joined.push(*run);
            vcn += run.clusters;
        }
    }
    Ok(joined)
}

fn build_extents(pieces: &[(u64, Vec<Run>)], cluster_size: u64) -> Result<Vec<Extent>> {
    let mut extents = Vec::new();
    let mut vcn = 0u64;
    for run in join_pieces(pieces, "data")? {
        let lcn = run.lcn.ok_or_else(|| eyre!("$MFT contains a sparse run"))?;
        extents.push(Extent {
            mft_offset: vcn * cluster_size,
            disk_offset: lcn * cluster_size,
            len: run.clusters * cluster_size,
        });
        vcn += run.clusters;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A record with unnamed non-resident attributes of `(kind, starting vcn, real size, run)`,
    /// each with the single run `(clusters, lcn)`.
    fn record(attrs: &[(u32, u64, u64, (u8, u8))]) -> Vec<u8> {
        let mut rec = vec![0u8; 1024];
        let mut off = 56;
        rec[20..22].copy_from_slice(&(off as u16).to_le_bytes());
        for &(kind, vcn, size, (clusters, lcn)) in attrs {
            let a = &mut rec[off..off + 72];
            a[0..4].copy_from_slice(&kind.to_le_bytes());
            a[4..8].copy_from_slice(&72u32.to_le_bytes());
            a[8] = 1;
            a[10..12].copy_from_slice(&64u16.to_le_bytes());
            a[16..24].copy_from_slice(&vcn.to_le_bytes());
            a[32..34].copy_from_slice(&64u16.to_le_bytes());
            a[48..56].copy_from_slice(&size.to_le_bytes());
            a[64..68].copy_from_slice(&[0x11, clusters, lcn, 0]);
            off += 72;
        }
        rec[off..off + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        rec[24..28].copy_from_slice(&(off as u32 + 8).to_le_bytes());
        rec
    }

    fn clusters(runs: &[Run]) -> Vec<(Option<u64>, u64)> {
        runs.iter().map(|r| (r.lcn, r.clusters)).collect()
    }

    /// A grown $MFT can have its bitmap split over extension records, where every piece after the
    /// first says its size is 0
    #[test]
    fn split_bitmap() {
        let rec0 = record(&[
            (ATTR_DATA, 0, 1 << 20, (100, 10)),
            (ATTR_BITMAP, 0, 3000, (2, 50)),
        ]);
        let ext1 = record(&[(ATTR_BITMAP, 3, 0, (1, 70))]);
        let ext2 = record(&[(ATTR_BITMAP, 2, 0, (1, 60))]);

        let mut attrs = MftAttributes::default();
        for rec in [&rec0, &ext1, &ext2] {
            assert!(collect_mft_attributes(rec, &mut attrs).unwrap().is_none());
        }
        assert_eq!(attrs.data_size, Some(1 << 20));
        let (runs, size) = attrs.bitmap_runs().unwrap().unwrap();
        assert_eq!(size, 3000);
        assert_eq!(
            clusters(&runs),
            [(Some(50), 2), (Some(60), 1), (Some(70), 1)]
        );
    }

    /// Bitmaps that would leave records out are not used
    #[test]
    fn unusable_bitmap() {
        let collect = |recs: &[Vec<u8>]| {
            let mut attrs = MftAttributes::default();
            for rec in recs {
                collect_mft_attributes(rec, &mut attrs).unwrap();
            }
            attrs
        };
        // Without the piece at VCN 0 the size is unknown
        let attrs = collect(&[record(&[(ATTR_BITMAP, 2, 0, (1, 60))])]);
        assert!(attrs.bitmap_runs().is_err());
        // Runs that can not be decoded
        let mut rec = record(&[(ATTR_BITMAP, 0, 3000, (2, 50))]);
        rec[56 + 64] = 0x19;
        let attrs = collect(&[rec]);
        assert!(attrs.bitmap_runs().is_err());
        // No bitmap at all is fine, every record is read
        let attrs = collect(&[record(&[(ATTR_DATA, 0, 1 << 20, (100, 10))])]);
        assert!(attrs.bitmap_runs().unwrap().is_none());

        assert!(check_bitmap(&[0xFF; 2], 16).is_ok());
        assert!(check_bitmap(&[0xFF; 2], 17).is_err());
        assert!(check_bitmap(&[], 1).is_err());
    }

    #[test]
    fn gap_between_pieces() {
        let pieces = [
            (
                0,
                vec![Run {
                    lcn: Some(5),
                    clusters: 2,
                }],
            ),
            (
                3,
                vec![Run {
                    lcn: Some(9),
                    clusters: 1,
                }],
            ),
        ];
        assert!(join_pieces(&pieces, "bitmap").is_err());
        assert!(build_extents(&pieces, 4096).is_err());
        assert_eq!(build_extents(&pieces[..1], 4096).unwrap().len(), 1);
    }
}
