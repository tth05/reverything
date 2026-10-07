//! Times applying journal changes to a large index, the way the service applies a batch: files
//! modified, created, renamed and deleted, in batches of different sizes. Every case also runs
//! while a search result holds on to the sorted list, which is the usual state of a running
//! service.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use eyre::{ensure, Result};

use reverything_core::index::persist::load_offline;
use reverything_core::index::update::{RecordState, RecordUpdate};
use reverything_core::index::{VolumeIndex, FLAG_DIRECTORY, FLAG_HAS_LINKS, FLAG_IN_USE};
use reverything_core::ntfs::volume::Volume;

use crate::phases;

const BATCHES: [usize; 5] = [1, 10, 100, 1000, 10_000];

#[derive(Copy, Clone, Debug, PartialEq)]
enum Change {
    Modify,
    Create,
    Rename,
    Delete,
}

pub fn run(dir: &Path, letter: char, runs: usize) -> Result<()> {
    ensure!(runs > 0, "Need at least one run");
    let t = Instant::now();
    let mut index = load_offline(Volume { id: letter }, dir)?;
    println!(
        "loaded {}: {} entries, {} record slots in {:?}",
        letter,
        index.file_count(),
        index.records.len(),
        t.elapsed()
    );
    crate::print_memory("after loading");
    let mut rng = 0x2545f4914f6cdd1d_u64;
    let mut usn = 0;
    // Warm up the ranking locations, which updates keep current
    index.locations();
    phases::take();

    println!(
        "{:<8} {:>6} {:>5} {:>9} {:>9}  phases (median ms)",
        "change", "batch", "held", "median", "max"
    );
    for change in [
        Change::Modify,
        Change::Create,
        Change::Rename,
        Change::Delete,
    ] {
        for batch in BATCHES {
            for held in [false, true] {
                let mut times = Vec::new();
                let mut phase_runs: BTreeMap<&str, Vec<Duration>> = BTreeMap::new();
                for _ in 0..runs {
                    let updates = make_updates(&index, change, batch, &mut rng);
                    // A search result sharing the sorted list
                    let result = held.then(|| index.sorted.clone());
                    usn += 1;
                    let t = Instant::now();
                    index.apply_updates(&updates, usn);
                    times.push(t.elapsed());
                    drop(result);
                    for (name, d) in phases::take() {
                        phase_runs.entry(name).or_default().push(d);
                    }
                }
                times.sort();
                let phases = phase_runs
                    .into_iter()
                    .filter(|(name, _)| *name != "update")
                    .map(|(name, mut d)| {
                        d.sort();
                        (name.trim_start_matches("update."), d[d.len() / 2])
                    })
                    .filter(|(_, d)| *d >= Duration::from_micros(100))
                    .map(|(name, d)| format!("{} {:.1}", name, d.as_secs_f64() * 1000.0))
                    .collect::<Vec<_>>();
                println!(
                    "{:<8} {:>6} {:>5} {:>9.2} {:>9.2}  {}",
                    format!("{:?}", change),
                    batch,
                    if held { "yes" } else { "no" },
                    times[times.len() / 2].as_secs_f64() * 1000.0,
                    times.last().unwrap().as_secs_f64() * 1000.0,
                    phases.join(", ")
                );
            }
        }
    }
    crate::print_memory("at the end");
    Ok(())
}

fn next(rng: &mut u64) -> u64 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    *rng
}

/// A random file without hard links, and a random directory.
fn random_entry(index: &VolumeIndex, rng: &mut u64, directory: bool) -> u32 {
    loop {
        let id = (next(rng) % index.records.len() as u64) as u32;
        let flags = index.records.flags[id as usize];
        if flags & FLAG_IN_USE != 0
            && flags & FLAG_HAS_LINKS == 0
            && (flags & FLAG_DIRECTORY != 0) == directory
            && id > 16
        {
            return id;
        }
    }
}

fn make_updates(
    index: &VolumeIndex,
    change: Change,
    batch: usize,
    rng: &mut u64,
) -> Vec<RecordUpdate> {
    let state = |id: u32, parent: u32, name: Vec<u8>, modified: u32| {
        let r = id as usize;
        RecordState {
            names: vec![(parent, name)],
            flags: index.records.flags[r] & !FLAG_HAS_LINKS | FLAG_IN_USE,
            size: Some(index.records.size[r] + 1),
            created: index.records.created[r],
            modified,
            sequence: index.records.sequence[r],
        }
    };
    let now = 1_790_000_000;
    let mut new_record = index.records.len() as u32;
    (0..batch)
        .map(|_| match change {
            Change::Modify => {
                let id = random_entry(index, rng, false);
                let name = index.name(id).to_vec();
                RecordUpdate {
                    record: id,
                    state: Some(state(id, index.parent(id), name, now)),
                }
            }
            Change::Rename => {
                let id = random_entry(index, rng, false);
                let mut name = index.name(id).to_vec();
                name.extend_from_slice(format!("~{}", next(rng) % 1000).as_bytes());
                RecordUpdate {
                    record: id,
                    state: Some(state(id, index.parent(id), name, now)),
                }
            }
            Change::Create => {
                let parent = random_entry(index, rng, true);
                let record = new_record;
                new_record += 1;
                RecordUpdate {
                    record,
                    state: Some(RecordState {
                        names: vec![(parent, format!("new file {:x}.txt", next(rng)).into_bytes())],
                        flags: FLAG_IN_USE,
                        size: Some(1234),
                        created: now,
                        modified: now,
                        sequence: 1,
                    }),
                }
            }
            Change::Delete => RecordUpdate {
                record: random_entry(index, rng, false),
                state: None,
            },
        })
        .collect()
}
