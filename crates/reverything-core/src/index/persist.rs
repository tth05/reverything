//! Saving and loading an index, so startup only has to replay the journal instead of reading the
//! whole MFT.
//!
//! After a header, the file holds the index arrays one after another. Each array is split into
//! chunks of about [`CHUNK`] bytes that are compressed with zstd separately, so saving and loading
//! use all cores. An array is stored as its element count, the chunk count, the uncompressed and
//! compressed size of every chunk, then the compressed chunks. Name offsets are not stored: names
//! are saved in the order [`VolumeIndex::name_order`] gives, so the offsets follow from the
//! lengths.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use eyre::{bail, ensure, eyre, Context, Result};
use rayon::prelude::*;

use crate::index::{
    is_link, link_index, Links, Records, SyncPtr, VolumeIndex, FLAG_IN_USE, NO_RECORD,
};
use crate::ntfs::usn::query_journal;
use crate::ntfs::volume::{volume_data, Volume};

const MAGIC: [u8; 8] = *b"RVINDEX\0";
const VERSION: u32 = 4;
/// zstd level. Higher levels barely shrink the index further but take much longer.
const LEVEL: i32 = 1;
/// Uncompressed size of a chunk
const CHUNK: usize = 4 << 20;

/// Plain old data that can be written as raw bytes.
trait Pod: Copy + Default {}
impl Pod for u8 {}
impl Pod for u16 {}
impl Pod for u32 {}
impl Pod for u64 {}

pub fn db_path(dir: &Path, volume: Volume) -> PathBuf {
    dir.join(format!("{}.db", volume.id))
}

/// Directory for indices saved by development tools (benchmarks, offline mode), so they never
/// touch the ones of the installed service. `REVERYTHING_DEV_DIR` overrides it.
pub fn dev_db_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("REVERYTHING_DEV_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("reverything-dev")
}

/// Loads the saved index if it belongs to this volume and the journal still contains every
/// change since it was saved.
pub fn load_current(volume: Volume, dir: &Path) -> Result<VolumeIndex> {
    let handle = volume.open(false, false)?;
    let vd = volume_data(&handle)?;
    let journal = query_journal(&handle)?;
    let mut index = VolumeIndex::load(volume, dir, Some(vd.VolumeSerialNumber as u64))?;
    ensure!(index.journal_id == journal.id, "The journal was recreated");
    ensure!(
        index.next_usn >= journal.first_usn && index.next_usn <= journal.next_usn,
        "The journal no longer contains all changes since the index was saved"
    );
    // Cheap, and keeps small errors from incremental updates from adding up across restarts
    index.compute_folder_sizes();
    Ok(index)
}

/// The header of a saved index, readable without loading the whole file.
#[derive(Debug, Clone, Copy)]
pub struct SavedHeader {
    pub record_size: u32,
    pub volume_serial: u64,
    pub journal_id: u64,
    /// Journal position the saved index is up to date with
    pub next_usn: i64,
}

/// Reads the header of the saved index of `volume`, `None` if there is none.
pub fn read_header(volume: Volume, dir: &Path) -> Result<Option<SavedHeader>> {
    let path = db_path(dir, volume);
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Failed to open {}", path.display())),
    };
    read_header_from(&mut file, volume).map(Some)
}

fn read_header_from(file: &mut File, volume: Volume) -> Result<SavedHeader> {
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)?;
    ensure!(magic == MAGIC, "Not an index file");
    let version = read_u32(file)?;
    ensure!(
        version == VERSION,
        "Index file has version {}, expected {}",
        version,
        VERSION
    );
    ensure!(
        read_u32(file)? == volume.id as u32,
        "Index file is for another volume"
    );
    Ok(SavedHeader {
        record_size: read_u32(file)?,
        volume_serial: read_u64(file)?,
        journal_id: read_u64(file)?,
        next_usn: read_u64(file)? as i64,
    })
}

/// Loads the saved index of a volume with the given serial number, without checking the
/// journal. The caller brings it up to date.
pub fn load_saved(volume: Volume, dir: &Path, volume_serial: u64) -> Result<VolumeIndex> {
    let mut index = VolumeIndex::load(volume, dir, Some(volume_serial))?;
    index.compute_folder_sizes();
    Ok(index)
}

/// Loads a saved index without checking it against the volume, which needs no admin rights.
/// The index can not be kept up to date then.
pub fn load_offline(volume: Volume, dir: &Path) -> Result<VolumeIndex> {
    let mut index = VolumeIndex::load(volume, dir, None)?;
    index.compute_folder_sizes();
    Ok(index)
}

impl VolumeIndex {
    pub fn save(&self, dir: &Path) -> Result<u64> {
        let path = db_path(dir, self.volume);
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("tmp");

        let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        w.write_all(&MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        w.write_all(&(self.volume.id as u32).to_le_bytes())?;
        w.write_all(&self.record_size.to_le_bytes())?;
        w.write_all(&self.volume_serial.to_le_bytes())?;
        w.write_all(&self.journal_id.to_le_bytes())?;
        w.write_all(&self.next_usn.to_le_bytes())?;

        let (r, l) = (&self.records, &self.links);
        let order = self.name_order();
        let sections = [
            array(&r.name_len),
            array(&r.parent),
            array(&r.flags),
            array(&r.size),
            array(&r.created),
            array(&r.modified),
            array(&r.sequence),
            array(&l.record),
            array(&l.parent),
            array(&l.name_len),
            self.name_chunks(&order),
            array(self.sorted.as_slice()),
        ];
        let compressed = sections
            .iter()
            .flat_map(|(_, chunks)| chunks)
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|chunk| self.compress(chunk))
            .collect::<Result<Vec<_>>>()?;

        let mut compressed = compressed.into_iter();
        for (count, chunks) in &sections {
            let chunks = compressed.by_ref().take(chunks.len()).collect::<Vec<_>>();
            w.write_all(&count.to_le_bytes())?;
            w.write_all(&(chunks.len() as u32).to_le_bytes())?;
            for (raw_len, data) in &chunks {
                w.write_all(&raw_len.to_le_bytes())?;
                w.write_all(&(data.len() as u32).to_le_bytes())?;
            }
            for (_, data) in &chunks {
                w.write_all(data)?;
            }
        }

        let file = w.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        let size = file.metadata()?.len();
        drop(file);

        std::fs::rename(&tmp, &path)
            .with_context(|| format!("Failed to replace {}", path.display()))?;
        Ok(size)
    }

    /// Loads the saved index of `volume` from `dir`. With `volume_serial`, the index has to
    /// belong to that volume.
    pub fn load(volume: Volume, dir: &Path, volume_serial: Option<u64>) -> Result<Self> {
        let path = db_path(dir, volume);
        let mut file =
            File::open(&path).with_context(|| format!("Failed to open {}", path.display()))?;

        let SavedHeader {
            record_size,
            volume_serial: serial,
            journal_id,
            next_usn,
        } = read_header_from(&mut file, volume)?;
        ensure!(
            volume_serial.is_none_or(|s| s == serial),
            "Index file is for another volume"
        );
        let volume_serial = serial;

        let mut data = Vec::with_capacity(file.metadata()?.len() as usize);
        file.read_to_end(&mut data)?;
        let mut reader = Reader {
            data: &data,
            pos: 0,
            jobs: Vec::new(),
        };
        // Allocates the arrays and collects their chunks, then decompresses all chunks at once
        let name_len: Vec<u16> = reader.array()?;
        let records = Records {
            name_off: vec![u32::MAX; name_len.len()],
            name_len,
            parent: reader.array()?,
            flags: reader.array()?,
            size: reader.array()?,
            created: reader.array()?,
            modified: reader.array()?,
            sequence: reader.array()?,
        };
        let record: Vec<u32> = reader.array()?;
        let mut links = Links {
            name_off: vec![u32::MAX; record.len()],
            record,
            parent: reader.array()?,
            name_len: reader.array()?,
            free: Vec::new(),
        };
        let names = reader.array()?;
        let sorted = reader.array()?;
        ensure!(reader.pos == data.len(), "Index file has trailing data");
        reader.decompress()?;

        links.free = (0..links.record.len() as u32)
            .filter(|&l| links.record[l as usize] == NO_RECORD)
            .collect();

        let mut index = VolumeIndex {
            volume,
            volume_serial,
            journal_id,
            next_usn,
            record_size,
            records,
            links,
            names,
            sorted: sorted.into(),
            garbage: 0,
            locations: Default::default(),
        };
        index.place_names()?;
        index.validate()?;
        Ok(index)
    }

    /// Splits the names, in the order they are saved in, into chunks.
    fn name_chunks<'a>(&self, order: &'a [u32]) -> (u64, Vec<Chunk<'a>>) {
        let (mut chunks, mut start, mut bytes, mut total) = (Vec::new(), 0, 0, 0);
        for (i, &id) in order.iter().enumerate() {
            bytes += self.name(id).len();
            if bytes >= CHUNK || i + 1 == order.len() {
                chunks.push(Chunk::Names(&order[start..i + 1]));
                total += bytes as u64;
                (start, bytes) = (i + 1, 0);
            }
        }
        (total, chunks)
    }

    /// Returns the uncompressed size and the compressed data of a chunk.
    fn compress(&self, chunk: &Chunk) -> Result<(u32, Vec<u8>)> {
        let names;
        let raw = match *chunk {
            Chunk::Bytes(bytes) => bytes,
            Chunk::Names(ids) => {
                names = ids
                    .iter()
                    .flat_map(|&id| self.name(id))
                    .copied()
                    .collect::<Vec<_>>();
                &names
            }
        };
        Ok((raw.len() as u32, zstd::bulk::compress(raw, LEVEL)?))
    }

    /// Sets the name offsets of a loaded index from the order the names were saved in.
    fn place_names(&mut self) -> Result<()> {
        let mut off = 0usize;
        for id in self.name_order() {
            let (slot, len) = if is_link(id) {
                let l = link_index(id);
                ensure!(
                    l < self.links.len(),
                    "Sorted list references unknown entries"
                );
                (&mut self.links.name_off[l], self.links.name_len[l])
            } else {
                let i = id as usize;
                ensure!(
                    i < self.records.len(),
                    "Sorted list references unknown entries"
                );
                (&mut self.records.name_off[i], self.records.name_len[i])
            };
            *slot = off as u32;
            off += len as usize;
            ensure!(
                off <= self.names.len(),
                "Names are shorter than their lengths"
            );
        }
        ensure!(
            off == self.names.len(),
            "Names are longer than their lengths"
        );
        // Entries that were not saved with a name, e.g. free link slots
        let r = &mut self.records;
        let l = &mut self.links;
        for (off, len) in r.name_off.iter_mut().zip(&mut r.name_len) {
            if *off == u32::MAX {
                (*off, *len) = (0, 0);
            }
        }
        for (off, len) in l.name_off.iter_mut().zip(&mut l.name_len) {
            if *off == u32::MAX {
                (*off, *len) = (0, 0);
            }
        }
        Ok(())
    }

    /// Makes sure a loaded index can not cause out of bounds accesses.
    fn validate(&self) -> Result<()> {
        let r = &self.records;
        let n = r.flags.len();
        ensure!(
            [
                r.name_off.len(),
                r.name_len.len(),
                r.parent.len(),
                r.size.len(),
                r.created.len(),
                r.modified.len(),
                r.sequence.len()
            ]
            .iter()
            .all(|&len| len == n),
            "Record arrays have different lengths"
        );
        for i in 0..n {
            if r.flags[i] & FLAG_IN_USE != 0
                && r.name_off[i] as usize + r.name_len[i] as usize > self.names.len()
            {
                bail!("Name of record {} is out of bounds", i);
            }
        }
        let l = &self.links;
        let links = l.record.len();
        ensure!(
            [l.parent.len(), l.name_off.len(), l.name_len.len()]
                .iter()
                .all(|&len| len == links),
            "Link arrays have different lengths"
        );
        for i in 0..links {
            if l.record[i] != NO_RECORD
                && ((l.record[i] as usize) >= n
                    || l.name_off[i] as usize + l.name_len[i] as usize > self.names.len())
            {
                bail!("Link {} is invalid", i);
            }
        }
        ensure!(
            self.sorted.iter().all(|&id| if is_link(id) {
                link_index(id) < links
            } else {
                (id as usize) < n
            }),
            "Sorted list references unknown entries"
        );
        Ok(())
    }
}

/// Uncompressed data of a chunk
enum Chunk<'a> {
    Bytes(&'a [u8]),
    /// The names of these entries, back to back
    Names(&'a [u32]),
}

/// The element count and the chunks of an array.
fn array<T: Pod>(data: &[T]) -> (u64, Vec<Chunk<'_>>) {
    let bytes =
        unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, size_of_val(data)) };
    (
        data.len() as u64,
        bytes.chunks(CHUNK).map(Chunk::Bytes).collect(),
    )
}

/// Reads the arrays of a saved index from its file contents.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    jobs: Vec<Job<'a>>,
}

/// A chunk to decompress into an array allocated by [`Reader::array`]
struct Job<'a> {
    dst: SyncPtr<u8>,
    len: usize,
    src: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let bytes = self
            .data
            .get(self.pos..self.pos.saturating_add(len))
            .ok_or_else(|| eyre!("Index file is truncated"))?;
        self.pos += len;
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    /// Allocates the next array. Its contents are filled in by [`Reader::decompress`], the
    /// array must not be touched before.
    fn array<T: Pod>(&mut self) -> Result<Vec<T>> {
        let count = u64::from_le_bytes(self.take(8)?.try_into().unwrap()) as usize;
        let chunks = self.u32()? as usize;
        let mut sizes = Vec::with_capacity(chunks.min(self.data.len() / 8));
        for _ in 0..chunks {
            let (raw, compressed) = (self.u32()? as usize, self.u32()? as usize);
            // Name chunks end after the name that reaches CHUNK
            ensure!(
                raw <= CHUNK + u16::MAX as usize,
                "Implausible chunk size {}",
                raw
            );
            sizes.push((raw, compressed));
        }
        ensure!(
            sizes.iter().map(|&(raw, _)| raw).sum::<usize>() == count * size_of::<T>(),
            "Chunk sizes do not match the array length"
        );

        let mut array = vec![T::default(); count];
        let mut dst = SyncPtr(array.as_mut_ptr() as *mut u8);
        for (raw, compressed) in sizes {
            let src = self.take(compressed)?;
            self.jobs.push(Job { dst, len: raw, src });
            dst = SyncPtr(unsafe { dst.get().add(raw) });
        }
        Ok(array)
    }

    /// Decompresses the chunks of all arrays.
    fn decompress(self) -> Result<()> {
        self.jobs.par_iter().try_for_each(|job| {
            // The chunks of an array cover it exactly once and the array is still alive
            let dst = unsafe { std::slice::from_raw_parts_mut(job.dst.get(), job.len) };
            let len = zstd::bulk::decompress_to_buffer(job.src, dst)?;
            ensure!(len == job.len, "Chunk is shorter than its size");
            Ok(())
        })
    }
}

fn read_u32(r: &mut impl Read) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::update::{RecordState, RecordUpdate};
    use crate::index::FLAG_DIRECTORY;
    use crate::ntfs::ROOT_RECORD;

    fn state(names: &[(u32, &str)], directory: bool, size: u64) -> Option<RecordState> {
        Some(RecordState {
            names: names
                .iter()
                .map(|&(p, n)| (p, n.as_bytes().to_vec()))
                .collect(),
            flags: FLAG_IN_USE | if directory { FLAG_DIRECTORY } else { 0 },
            size: (!directory).then_some(size),
            created: 1,
            modified: 2,
            sequence: 1,
        })
    }

    #[test]
    fn save_and_load() {
        let volume = Volume { id: 'Z' };
        let mut index = VolumeIndex::empty(volume);
        let update = |record, state| RecordUpdate { record, state };
        index.apply_updates(
            &[
                update(ROOT_RECORD, state(&[(ROOT_RECORD, ".")], true, 0)),
                update(40, state(&[(ROOT_RECORD, "Users")], true, 0)),
                update(41, state(&[(40, "notes.txt")], false, 10)),
                update(
                    42,
                    state(&[(40, "a.dll"), (ROOT_RECORD, "b.dll")], false, 20),
                ),
                update(
                    43,
                    state(
                        &[(40, "Ärger.md"), (40, "c.md"), (ROOT_RECORD, "d.md")],
                        false,
                        30,
                    ),
                ),
                update(44, state(&[(40, "gone.txt")], false, 40)),
            ],
            1,
        );
        // Leaves names out of sorted order, garbage and a free link slot behind
        index.apply_updates(
            &[
                update(41, state(&[(ROOT_RECORD, "renamed.txt")], false, 11)),
                update(43, state(&[(40, "Ärger.md")], false, 30)),
                update(44, None),
                update(45, state(&[(40, "zz new")], false, 50)),
            ],
            2,
        );

        let dir = std::env::temp_dir().join(format!("rv-persist-test-{}", std::process::id()));
        index.save(&dir).unwrap();
        let loaded = load_offline(volume, &dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(loaded.sorted, index.sorted);
        assert_eq!(loaded.next_usn, 2);
        assert_eq!(loaded.links.record, index.links.record);
        assert_eq!(loaded.links.free.len(), index.links.free.len());
        assert!(!index.links.free.is_empty());
        for id in index.name_order() {
            assert_eq!(loaded.name_str(id), index.name_str(id));
            assert_eq!(loaded.parent(id), index.parent(id));
            assert_eq!(loaded.flags(id), index.flags(id));
        }
        assert_eq!(loaded.records.size, index.records.size);
        assert_eq!(loaded.records.modified, index.records.modified);
        // Saving dropped the garbage
        assert!(loaded.names.len() < index.names.len());
    }
}
