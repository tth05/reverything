use std::ffi::c_void;
use std::ops::Range;

use eyre::{eyre, Result};
use rayon::prelude::*;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Ioctl::NTFS_VOLUME_DATA_BUFFER;
use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};
use windows::Win32::System::IO::OVERLAPPED;

use crate::ntfs::file_record::FileRecord;
use crate::ntfs::mft::MftFile;
use crate::ntfs::volume::{create_overlapped, Volume};
use crate::FileInfo;

const ROOT_INDEX: u64 = 5;

pub struct NtfsVolumeIndex {
    volume: Volume,
    infos: Vec<Option<FileInfo>>,
}

impl NtfsVolumeIndex {
    pub fn new(volume: Volume) -> Result<NtfsVolumeIndex> {
        let volume_data = volume.query_volume_data()?;
        let mft_file = MftFile::new(volume, volume_data)?;

        Ok(Self {
            volume,
            infos: process_mft_data(
                volume,
                mft_file
                    .as_record()
                    .read_data_runs(volume_data.BytesPerCluster as usize)?,
            )?,
        })
    }

    pub fn find_by_name(&self, name: &str) -> Option<&FileInfo> {
        self.infos.par_iter().find_map_first(|info| {
            info.as_ref()
                .and_then(|info| if info.name == name { Some(info) } else { None })
        })
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

        debug_assert!(out.capacity() == 2 + path_size);

        out
    }

    pub fn iter_with_parents<'a>(
        &'a self,
        file_info: &'a FileInfo,
    ) -> impl Iterator<Item = &'a FileInfo> {
        HierarchyIter::<'a> {
            index: self,
            current: file_info,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &FileInfo> {
        self.infos.iter().filter_map(|info| info.as_ref())
    }

    pub fn volume(&self) -> Volume {
        self.volume
    }
}

struct HierarchyIter<'a> {
    index: &'a NtfsVolumeIndex,
    current: &'a FileInfo,
}

impl<'a> Iterator for HierarchyIter<'a> {
    type Item = &'a FileInfo;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current.parent == ROOT_INDEX {
            return None;
        }

        let parent = self.index.find_by_index(self.current.parent)?;
        let next = self.current;
        self.current = parent;
        Some(next)
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
                            let Some((parent, name)) = record.get_file_name_info() else {
                                return None;
                            };

                            Some(FileInfo { name, parent })
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
    let mut buffer = Vec::with_capacity(runs.iter().map(|r| r.len()).sum::<usize>());
    let mut write_offset = 0usize;
    for run in runs {
        unsafe {
            let mut ov = create_overlapped(run.start);
            ov.hEvent = CreateEventW(None, true, false, None)?;

            let res = ReadFile(
                handle,
                Some((buffer.as_mut_ptr() as *mut c_void).add(write_offset)),
                run.len() as u32,
                None,
                Some(&mut ov as *mut OVERLAPPED),
            );

            // Might return true if the read is completed immediately
            if !res.as_bool() {
                events.push(ov.hEvent);
            }

            write_offset += run.len();
        }
    }

    unsafe {
        let res = WaitForMultipleObjects(&events, true, 50000);
        events.iter().for_each(|&e| {
            CloseHandle(e);
        });
        CloseHandle(handle);

        if res != WAIT_OBJECT_0 {
            return Err(eyre!(
                "WaitForMultipleObjects failed {:?} {:?}",
                res.0 as i32,
                std::thread::current().id()
            ));
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
    let cpus = num_cpus::get_physical();
    let run_size = total_size / cpus;

    // Distribute runs evenly
    let mut run_groups = Vec::with_capacity(cpus);
    // Last thread gets the rest after the loop
    for _ in 0..(cpus - 1) {
        let mut run_group = Vec::with_capacity(2);
        let mut run_group_size = 0usize;
        while run_group_size < run_size && !runs.is_empty() {
            let run = runs.remove(0);

            let run_len = run.len();
            // Get as close as possible to the run size, but don't split runs into non-cluster
            // aligned chunks
            if run_group_size + run_len > run_size
                && run_len % volume_data.BytesPerCluster as usize == 0
            {
                let split = run_group_size + run.len() - run_size;
                // Round to cluster boundary
                let split = split - (split % volume_data.BytesPerCluster as usize);
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
