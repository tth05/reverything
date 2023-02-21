use std::ffi::c_void;
use std::ops::Range;
use std::time::Instant;

use eyre::{eyre, ContextCompat, Result};
use rustc_hash::FxHashMap;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Ioctl::NTFS_VOLUME_DATA_BUFFER;
use windows::Win32::System::Threading::{
    CreateEventW, WaitForMultipleObjects,
};
use windows::Win32::System::IO::OVERLAPPED;

use crate::ntfs::file_record::FileRecord;
use crate::ntfs::mft::MftFile;
use crate::ntfs::volume::{create_overlapped, get_volumes, query_volume_data, Volume};

mod ntfs;

struct FileInfo {
    name: String,
    parent: u64,
}

fn main() -> Result<()> {
    let t = Instant::now();
    unsafe {
        let vol = get_volumes()
            .into_iter()
            .next()
            .with_context(|| "Cannot find first volume")?;

        let data = {
            let handle = vol.create_read_handle()?;
            let res = query_volume_data(handle)?;
            CloseHandle(handle);
            res
        };

        let mft_file = MftFile::new(vol, data)?;
        let mft_data = mft_file
            .as_record()
            .read_data_runs(data.BytesPerCluster as usize)
            .unwrap();

        let map = process_mft_data(vol, mft_data)?;
        let info = map.values().find(|&f| f.name == "idea64.exe").unwrap();
        println!("{}", map.len());
        println!("{:?}/{:?}", map.get(&info.parent).unwrap().name, info.name);
    }

    println!("Elapsed: {:?}", t.elapsed());

    Ok(())
}

fn process_mft_data(
    volume: Volume,
    (total_size, runs): (usize, Vec<Range<usize>>),
) -> Result<FxHashMap<u64, FileInfo>> {
    let volume_data = {
        let handle = volume.create_read_handle()?;
        query_volume_data(handle)?
    };

    let run_groups = distribute_runs_to_cpus(volume_data, (total_size, runs));

    let file_infos = std::thread::scope(|s| {
        let threads = run_groups
            .into_iter()
            .map(
                |RunGroup {
                     mut file_record_index,
                     runs,
                 }| {
                    s.spawn(move || {
                        if runs.is_empty() {
                            return Ok(FxHashMap::default());
                        }

                        let handle = volume.create_read_handle()?;
                        let mut events = Vec::with_capacity(runs.len());
                        let mut buffer =
                            Vec::with_capacity(runs.iter().map(|r| r.len()).sum::<usize>());
                        let mut write_offset = 0usize;
                        for run in runs {
                            unsafe {
                                let mut ov = Box::leak(Box::new(create_overlapped(run.start)));

                                ov.hEvent = CreateEventW(None, true, false, None)?;

                                let res = ReadFile(
                                    handle,
                                    Some((buffer.as_mut_ptr() as *mut c_void).add(write_offset)),
                                    run.len() as u32,
                                    None,
                                    Some(ov as *mut OVERLAPPED),
                                );

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

                        let mut map = FxHashMap::default();
                        for chunk in
                            buffer.chunks_mut(volume_data.BytesPerFileRecordSegment as usize)
                        {
                            file_record_index += 1;

                            let record = FileRecord::new(chunk);
                            // Should be fine to determine without fixup
                            if !record.is_valid() || !record.is_used() {
                                continue;
                            }

                            FileRecord::fixup(chunk, volume_data.BytesPerSector as usize);
                            let record = FileRecord::new(chunk);
                            let Some((parent, name)) = record.get_file_name_info() else {
                                continue;
                            };

                            map.insert(file_record_index - 1, FileInfo { name, parent });
                        }

                        Ok(map)
                    })
                },
            )
            .collect::<Vec<_>>();

        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Result<Vec<_>>>()
    })?
    .into_iter()
    .fold(
        FxHashMap::with_capacity_and_hasher(
            total_size / volume_data.BytesPerFileRecordSegment as usize,
            Default::default(),
        ),
        |mut acc, map| {
            acc.extend(map);
            acc
        },
    );

    Ok(file_infos)
}

#[derive(Debug)]
struct RunGroup {
    file_record_index: u64,
    runs: Vec<Range<usize>>,
}

fn distribute_runs_to_cpus(
    volume_data: NTFS_VOLUME_DATA_BUFFER,
    (total_size, mut runs): (usize, Vec<Range<usize>>),
) -> Vec<RunGroup> {
    let cpus = num_cpus::get_physical();
    let run_size = total_size / cpus;

    // Distribute runs evenly
    let mut run_groups = Vec::with_capacity(cpus);
    let mut file_record_index = 0u64;
    // Last thread gets the rest after the loop
    for _ in 0..(cpus - 1) {
        let mut run_group = Vec::with_capacity(2);
        let mut run_group_size = 0usize;
        while run_group_size < run_size && !runs.is_empty() {
            let run = runs.remove(0);

            let run_len = run.len();
            // Get as close as possible to the run size, but don't split runs into non-cluster
            // aligned chunks
            if run_group_size + run_len > run_size && run_len % volume_data.BytesPerCluster as usize == 0 {
                let split = run_group_size + run.len() - run_size;
                // Round to cluster boundary
                let split = split - (split % volume_data.BytesPerCluster as usize);
                runs.insert(0, run.start + split..run.end);
                run_group.push(run.start..run.start + split);
                run_group_size += split;
            } else {
                run_group_size += run_len;
                run_group.push(run);
            }
        }

        run_groups.push(RunGroup {
            file_record_index,
            runs: run_group,
        });

        file_record_index += run_group_size as u64 / volume_data.BytesPerFileRecordSegment as u64;
    }

    // Everything that remains is given to the last thread
    run_groups.push(RunGroup {
        file_record_index,
        runs,
    });

    run_groups
}
