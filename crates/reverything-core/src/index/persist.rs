//! Saving and loading an index, so startup only has to replay the journal instead of reading the
//! whole MFT.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use eyre::{bail, ensure, Context, Result};

use crate::index::{is_link, link_index, Links, Records, VolumeIndex, FLAG_IN_USE, NO_RECORD};
use crate::ntfs::usn::query_journal;
use crate::ntfs::volume::{volume_data, Volume};

const MAGIC: [u8; 8] = *b"RVINDEX\0";
const VERSION: u32 = 3;

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
/// touch the ones of the installed service.
pub fn dev_db_dir() -> PathBuf {
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

        let r = &self.records;
        write_slice(&mut w, &r.name_off)?;
        write_slice(&mut w, &r.name_len)?;
        write_slice(&mut w, &r.parent)?;
        write_slice(&mut w, &r.flags)?;
        write_slice(&mut w, &r.size)?;
        write_slice(&mut w, &r.created)?;
        write_slice(&mut w, &r.modified)?;
        write_slice(&mut w, &r.sequence)?;
        write_slice(&mut w, &self.links.record)?;
        write_slice(&mut w, &self.links.parent)?;
        write_slice(&mut w, &self.links.name_off)?;
        write_slice(&mut w, &self.links.name_len)?;
        write_slice(&mut w, &self.names)?;
        write_slice(&mut w, &self.sorted)?;

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

        let mut magic = [0u8; 8];
        file.read_exact(&mut magic)?;
        ensure!(magic == MAGIC, "Not an index file");
        let version = read_u32(&mut file)?;
        ensure!(
            version == VERSION,
            "Index file has version {}, expected {}",
            version,
            VERSION
        );
        ensure!(
            read_u32(&mut file)? == volume.id as u32,
            "Index file is for another volume"
        );
        let record_size = read_u32(&mut file)?;
        let serial = read_u64(&mut file)?;
        ensure!(
            volume_serial.is_none_or(|s| s == serial),
            "Index file is for another volume"
        );
        let volume_serial = serial;
        let journal_id = read_u64(&mut file)?;
        let next_usn = read_u64(&mut file)? as i64;

        let records = Records {
            name_off: read_vec(&mut file)?,
            name_len: read_vec(&mut file)?,
            parent: read_vec(&mut file)?,
            flags: read_vec(&mut file)?,
            size: read_vec(&mut file)?,
            created: read_vec(&mut file)?,
            modified: read_vec(&mut file)?,
            sequence: read_vec(&mut file)?,
        };
        let mut links = Links {
            record: read_vec(&mut file)?,
            parent: read_vec(&mut file)?,
            name_off: read_vec(&mut file)?,
            name_len: read_vec(&mut file)?,
            free: Vec::new(),
        };
        links.free = (0..links.record.len() as u32)
            .filter(|&l| links.record[l as usize] == NO_RECORD)
            .collect();
        let names = read_vec(&mut file)?;
        let sorted = read_vec(&mut file)?;

        let index = VolumeIndex {
            volume,
            volume_serial,
            journal_id,
            next_usn,
            record_size,
            records,
            links,
            names,
            sorted,
            garbage: 0,
        };
        index.validate()?;
        Ok(index)
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

fn write_slice<T: Pod>(w: &mut impl Write, data: &[T]) -> Result<()> {
    w.write_all(&(data.len() as u64).to_le_bytes())?;
    let bytes =
        unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, size_of_val(data)) };
    w.write_all(bytes)?;
    Ok(())
}

fn read_vec<T: Pod>(r: &mut impl Read) -> Result<Vec<T>> {
    let len = read_u64(r)? as usize;
    ensure!(len < (1 << 34), "Implausible array length {}", len);
    let mut v = vec![T::default(); len];
    let bytes =
        unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr() as *mut u8, len * size_of::<T>()) };
    r.read_exact(bytes)?;
    Ok(v)
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
