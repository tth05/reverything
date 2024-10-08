use std::ops::Range;

use crate::ntfs::file_attribute::AttributeType;
use crate::ntfs::file_record::FileRecord;
use crate::ntfs::journal::JournalEntry;
use crate::ntfs::mft::MftFile;
use crate::ntfs::try_close_handle;
use crate::ntfs::volume::{create_overlapped, Volume};
use eyre::{Context, Report, Result};
use rayon::prelude::*;
use smartstring::{Compact, SmartString};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Ioctl::NTFS_VOLUME_DATA_BUFFER;
use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};
use windows::Win32::System::IO::OVERLAPPED;

const ROOT_INDEX: u64 = 5;
const PAR_ITER_CHUNK_COUNT: usize = 64;

pub struct NtfsVolumeIndex {
    volume: Volume,
    infos: Vec<Option<FileInfo>>,
}

#[derive(Debug)]
pub struct FileInfo {
    pub name: SmartString<Compact>,
    parent: u64,
    size_and_directory: u64,
}

impl FileInfo {
    pub fn new(size: u64, is_directory: bool, parent: u64, name: SmartString<Compact>) -> Self {
        assert!(size <= 0x7FFF_FFFF_FFFF_FFFF);

        Self {
            name,
            parent,
            size_and_directory: size | (is_directory as u64) << 63,
        }
    }

    pub fn size(&self) -> u64 {
        self.size_and_directory & !(1 << 63)
    }

    #[allow(unused)]
    pub fn is_directory(&self) -> bool {
        self.size_and_directory & (1 << 63) != 0
    }
}

#[allow(unused)]
impl NtfsVolumeIndex {
    pub fn new(volume: Volume) -> Result<NtfsVolumeIndex> {
        let volume_data = volume.query_volume_data()?;
        let mft_file = MftFile::new(volume, volume_data)?;

        let files = process_mft_data(
            volume,
            mft_file
                .as_record()
                .read_data_runs(volume_data.BytesPerCluster as usize)?,
        )?;

        Ok(Self {
            volume,
            infos: files,
        })
    }

    pub fn process_journal_entries(&mut self, entries: &[JournalEntry]) {
        for e in entries {
            match e {
                JournalEntry::FileCreate {
                    mft_index,
                    is_directory,
                    parent_mft_index,
                    name,
                } => {
                    if self.find_by_index(*mft_index).is_some() {
                        eprintln!("File already exists: {}, {}", mft_index, name);
                        continue;
                    }

                    if self.find_by_index(*parent_mft_index).is_none() {
                        eprintln!("Parent not found: {}", parent_mft_index);
                        continue;
                    }

                    // Prevent out of bounds access
                    if *mft_index as usize >= self.infos.len() {
                        self.infos
                            .resize_with(*mft_index as usize + 1, Default::default);
                    }

                    self.infos[*mft_index as usize] = Some(FileInfo::new(
                        // TODO: Get size from somewhere
                        0,
                        *is_directory,
                        *parent_mft_index,
                        SmartString::from(name),
                    ));

                    #[cfg(feature = "journal_dbg")]
                    println!(
                        "Creating file: {}",
                        self.compute_full_path(self.infos[*mft_index as usize].as_ref().unwrap())
                    );
                }
                JournalEntry::Rename {
                    mft_index,
                    new_name,
                    new_parent_mft_index,
                } => {
                    if self.find_by_index(*new_parent_mft_index).is_none() {
                        eprintln!("Parent not found: {}", new_parent_mft_index);
                        continue;
                    }

                    #[cfg(feature = "journal_dbg")]
                    let old_path = self.compute_full_path(self.find_by_index(*mft_index).unwrap());

                    if let Some(Some(info)) = self.infos.get_mut(*mft_index as usize) {
                        info.name = SmartString::from(new_name);
                        info.parent = *new_parent_mft_index;
                    }

                    #[cfg(feature = "journal_dbg")]
                    println!(
                        "Renaming file: {} -> {}",
                        old_path,
                        self.compute_full_path(self.find_by_index(*mft_index).unwrap())
                    );
                }
                JournalEntry::FileDelete(index) => {
                    #[cfg(feature = "journal_dbg")]
                    if let Some(info) = self.find_by_index(*index) {
                        println!("Deleting file: {}", self.compute_full_path(info));
                    }

                    self.infos[*index as usize] = None;
                }
            }
        }
    }

    pub fn find_by_name(&self, name: &str) -> Option<&FileInfo> {
        self.par_iter()
            .find_first(|info| matches!(info, Some(info) if info.name == name))
            .and_then(|info| info.as_ref().copied())
    }

    pub fn find_by_index(&self, index: u64) -> Option<&FileInfo> {
        self.infos
            .get(index as usize)
            .and_then(|info| info.as_ref())
    }

    pub fn compute_full_path(&self, file_info: &FileInfo) -> String {
        let mut path_size = 0usize;
        let mut path = Vec::with_capacity(5);
        self.iter_with_parents(file_info).for_each(|f| {
            path.push(&f.name);
            path_size += f.name.len() + 1;
        });

        let mut out = String::with_capacity(2 + path_size);
        out.push(self.volume.id.to_ascii_uppercase());
        out.push(':');
        path.iter().rev().for_each(|&s| {
            out.push('\\');
            out.push_str(s);
        });

        out
    }

    pub fn iter_with_parents<'a>(
        &'a self,
        file_info: &'a FileInfo,
    ) -> impl Iterator<Item = &'a FileInfo> {
        HierarchyIter::<'a> {
            index: self,
            current: Some(file_info),
        }
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = Option<&FileInfo>> {
        self.infos.iter().map(|info| info.as_ref())
    }

    pub fn par_iter(&self) -> impl IndexedParallelIterator<Item = Option<&FileInfo>> {
        self.infos
            .par_iter()
            .map(|info| info.as_ref())
            .with_min_len(self.infos.len() / PAR_ITER_CHUNK_COUNT)
    }

    pub fn volume(&self) -> Volume {
        self.volume
    }

    pub fn file_info_count(&self) -> usize {
        self.infos.len()
    }

    // TODO: Cache this
    pub fn real_file_count(&self) -> usize {
        self.infos.iter().filter(|i| i.is_some()).count()
    }
}

struct HierarchyIter<'a> {
    index: &'a NtfsVolumeIndex,
    current: Option<&'a FileInfo>,
}

impl<'a> Iterator for HierarchyIter<'a> {
    type Item = &'a FileInfo;

    fn next(&mut self) -> Option<Self::Item> {
        match self.current {
            None => None,
            Some(current) => {
                let next = current;
                self.current = if current.parent == ROOT_INDEX {
                    None
                } else {
                    Some(self.index.find_by_index(current.parent)?)
                };

                Some(next)
            }
        }
    }
}

fn process_mft_data(
    volume: Volume,
    (total_size, runs): (usize, Vec<Range<usize>>),
) -> Result<Vec<Option<FileInfo>>> {
    let volume_data = volume.query_volume_data()?;

    let run_groups = distribute_runs_to_cpus(volume_data, (total_size, runs));

    let file_infos = std::thread::scope(|s| {
        // Spawn all threads
        let threads = run_groups
            .into_iter()
            .map(|runs| {
                s.spawn(move || {
                    if runs.is_empty() {
                        return Ok(Vec::default());
                    }

                    let mut buffer = read_runs_from_disk(volume, runs)?;

                    Ok(buffer
                        .chunks_mut(volume_data.BytesPerFileRecordSegment as usize)
                        .map(|chunk| {
                            let record = FileRecord::new(chunk);
                            // Should be fine to determine without fixup
                            if !record.is_valid() || !record.is_used() {
                                return None;
                            }

                            FileRecord::fixup(chunk, volume_data.BytesPerSector as usize);
                            let record = FileRecord::new(chunk);
                            let (real_size, parent, name) =
                                record.destructure_file_name_attribute()?;

                            // We get the size from the data attribute or the file name attribute.
                            // Some files don't have a data attribute, others don't have the size
                            // stored in the file name attribute.
                            Some(FileInfo::new(
                                record.get_data_attribute_size().max(real_size),
                                record.is_directory(),
                                parent,
                                name,
                            ))
                        })
                        .collect())
                })
            })
            .collect::<Vec<_>>();

        // Join all threads
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            // Vec<Result<...>> -> Result<Vec<...>>
            .collect::<Result<Vec<_>>>()
    })?
    .into_iter()
    .fold(
        Vec::with_capacity(total_size / volume_data.BytesPerFileRecordSegment as usize),
        |mut acc, vec| {
            acc.extend(vec);
            acc
        },
    );

    Ok(file_infos)
}

fn read_runs_from_disk(volume: Volume, runs: RunGroup) -> Result<Vec<u8>> {
    let handle = volume.create_read_handle()?;
    let mut events = Vec::with_capacity(runs.len());
    let mut buffer: Vec<u8> = Vec::with_capacity(runs.iter().map(|r| r.len()).sum::<usize>());
    let mut write_offset = 0usize;
    for run in runs {
        unsafe {
            let mut ov = create_overlapped(run.start);
            ov.hEvent = CreateEventW(None, true, false, None)?;

            let res = ReadFile(
                handle,
                Some(std::slice::from_raw_parts_mut(
                    buffer.as_mut_ptr().add(write_offset),
                    run.len(),
                )),
                None,
                Some(&mut ov as *mut OVERLAPPED),
            );

            // Might return true if the read is completed immediately
            if res.is_err() {
                events.push(ov.hEvent);
            } else {
                try_close_handle(ov.hEvent)?;
            }

            write_offset += run.len();
        }
    }

    unsafe {
        let res = WaitForMultipleObjects(&events, true, 50000);
        events
            .iter()
            .chain(std::iter::once(&handle))
            .try_for_each(|&e| try_close_handle(e))?;

        if res != WAIT_OBJECT_0 {
            return Err(Report::new(std::io::Error::last_os_error())).with_context(|| {
                format!(
                    "WaitForMultipleObjects failed {:?} {:?}",
                    res.0 as i32,
                    std::thread::current().id()
                )
            });
        }

        buffer.set_len(buffer.capacity());
    }

    Ok(buffer)
}

type RunGroup = Vec<Range<usize>>;

fn distribute_runs_to_cpus(
    volume_data: NTFS_VOLUME_DATA_BUFFER,
    (total_size, mut runs): (usize, RunGroup),
) -> Vec<RunGroup> {
    let cpus = num_cpus::get_physical() - 1 /* Save one thread for UI */;
    let run_size = total_size / cpus;
    let run_size = run_size - (run_size % volume_data.BytesPerCluster as usize);

    // Distribute runs evenly
    let mut run_groups = Vec::with_capacity(cpus);
    // Last thread gets the rest after the loop
    for _ in 0..(cpus - 1) {
        let mut run_group = Vec::with_capacity(2);
        let mut run_group_size = 0usize;
        while run_group_size < run_size && !runs.is_empty() {
            let run = runs.remove(0);

            let run_len = run.len();
            assert_eq!(run_len % volume_data.BytesPerCluster as usize, 0);
            // Get as close as possible to the run size, but don't split runs into non-cluster
            // aligned chunks
            if run_group_size + run_len > run_size {
                let split = run_size - run_group_size;
                // Push back the remaining part of the run
                runs.insert(0, run.start + split..run.end);
                // Give the second part to the current group
                run_group.push(run.start..run.start + split);
                run_group_size += split;
            } else {
                run_group_size += run_len;
                run_group.push(run);
            }
        }

        run_groups.push(run_group);
    }

    // Everything that remains is given to the last thread
    run_groups.push(runs);

    run_groups
}
