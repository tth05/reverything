//! Headless benchmark mode (`--bench`). Indexes every NTFS volume, prints timings and memory
//! usage, then exits. Needs to run elevated.

use std::time::Instant;

use eyre::Result;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::index::build::{scan_volume, ScanOptions};
use crate::index::VolumeIndex;
use crate::ntfs::volume::ntfs_volumes;

const MB: f64 = 1024.0 * 1024.0;

pub fn run() -> Result<()> {
    load_env_file();
    let opts = ScanOptions::from_env();
    println!("options: {:?}", opts);
    let volumes = ntfs_volumes();

    if let Ok(sweep) = std::env::var("RV_SWEEP") {
        // RV_SWEEP=threads:chunk_kb,threads:chunk_kb,... on the first volume
        for combo in sweep.split(',') {
            let Some((t, kb)) = combo.split_once(':') else { continue };
            let opts = ScanOptions {
                threads: t.parse()?,
                chunk_bytes: kb.parse::<usize>()? * 1024,
                ..opts.clone()
            };
            let t = Instant::now();
            let (index, stats) = scan_volume(volumes[0], &opts)?;
            println!(
                "sweep threads={} chunk={}KB: {:?} (read+parse {:?}, {:.0} MB/s), {} entries",
                opts.threads,
                opts.chunk_bytes / 1024,
                t.elapsed(),
                stats.read_parse,
                stats.bytes_read as f64 / MB / stats.read_parse.as_secs_f64(),
                index.file_count()
            );
        }
        return Ok(());
    }

    if std::env::var("RV_STARTUP").is_ok() {
        return bench_startup(volumes);
    }

    if std::env::var("RV_JOURNAL").is_ok() {
        let vol = volumes[0];
        let (mut index, _) = scan_volume(vol, &opts)?;
        return journal_selftest(&mut index);
    }

    // Each volume on its own, to get clean per volume numbers
    for &vol in &volumes {
        let t = Instant::now();
        let (index, stats) = match scan_volume(vol, &opts) {
            Ok(r) => r,
            Err(e) => {
                println!("{}: skipped ({:#})", vol.id, e);
                continue;
            }
        };
        let elapsed = t.elapsed();
        println!(
            "{}: {} entries in {:?} (open {:?}, read+parse {:?}, merge {:?}, sort {:?})",
            vol.id,
            index.file_count(),
            elapsed,
            stats.open,
            stats.read_parse,
            stats.merge,
            stats.sort
        );
        println!(
            "  {} chunks, {:.1} MB read ({:.0} MB/s), {} used records, {} extension records, {} hard links, folder sizes in {:?}",
            stats.chunks,
            stats.bytes_read as f64 / MB,
            stats.bytes_read as f64 / MB / stats.read_parse.as_secs_f64(),
            stats.used_records,
            stats.extension_records,
            stats.links,
            stats.folder_sizes
        );
        print_index_memory(&index);
        print_samples(&index);
        bench_persist(&index);
        print_memory("after scan");
    }

    // All volumes at once, which is what the app does
    let t = Instant::now();
    let opts = &opts;
    let indices = std::thread::scope(|s| {
        volumes
            .iter()
            .map(|&vol| s.spawn(move || scan_volume(vol, opts)))
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|h| h.join().unwrap().ok())
            .collect::<Vec<_>>()
    });
    println!(
        "all volumes in parallel: {} entries in {:?}",
        indices.iter().map(|(i, _)| i.file_count()).sum::<usize>(),
        t.elapsed()
    );
    print_memory("after parallel scan");

    let set = crate::service::IndexSet::new(volumes.clone());
    for (slot, (index, _)) in set.volumes.iter().zip(indices) {
        *slot.index.write().unwrap() = index;
    }
    bench_search(&set);
    print_memory("final");
    Ok(())
}

fn bench_search(set: &crate::service::IndexSet) {
    use crate::search::{search_all, Sort, SortColumn};

    let queries = [
        ("", SortColumn::Name),
        ("e", SortColumn::Name),
        ("notepad", SortColumn::Name),
        ("zzqqxy", SortColumn::Name),
        (".dll", SortColumn::Size),
        (".dll", SortColumn::Modified),
        (r"system32\", SortColumn::Name),
        (r"windows\system32\note", SortColumn::Name),
        (r"c:\windows\", SortColumn::Name),
        (r"c:\users\", SortColumn::Path),
        ("Ärger", SortColumn::Name),
    ];
    for (q, column) in queries {
        let sort = Sort { column, ascending: true };
        let t = Instant::now();
        let hits = search_all(set, q, sort);
        println!("  search {:?} by {:?}: {} hits in {:?}", q, column, hits.len(), t.elapsed());
    }
}

/// The scheduled task runs a fixed command line, so variations are passed through
/// `target/bench.env` (`KEY=VALUE` lines) instead.
fn load_env_file() {
    let Ok(text) = std::fs::read_to_string("target/bench.env") else {
        return;
    };
    for line in text.lines() {
        if let Some((k, v)) = line.trim().split_once('=') {
            std::env::set_var(k.trim(), v.trim());
        }
    }
}

/// What the app does on start: load every saved index (or scan) and catch up with the journal.
fn bench_startup(volumes: Vec<crate::ntfs::volume::Volume>) -> Result<()> {
    use crate::service::JournalFollower;

    let t = Instant::now();
    let loaded = std::thread::scope(|s| {
        volumes
            .iter()
            .map(|&vol| {
                s.spawn(move || -> Result<VolumeIndex> {
                    let t = Instant::now();
                    let mut index = crate::index::persist::load_current(vol)?;
                    let loaded = t.elapsed();
                    let t = Instant::now();
                    let mut follower = JournalFollower::new(&index)?;
                    let changed = follower.poll_changes().map_err(|e| eyre::eyre!("{:?}", e))?;
                    let updates = follower.fetch(&changed);
                    index.apply_updates(&updates, follower.reader.next_usn());
                    println!(
                        "{}: loaded {} entries in {:?}, replayed {} journal records ({} updates) in {:?}",
                        vol.id,
                        index.file_count(),
                        loaded,
                        changed.len(),
                        updates.len(),
                        t.elapsed()
                    );
                    Ok(index)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Result<Vec<_>>>()
    })?;
    println!("startup: {:?}", t.elapsed());
    for index in &loaded {
        print_index_memory(index);
    }
    print_memory("after startup");
    Ok(())
}

fn bench_persist(index: &VolumeIndex) {
    let t = Instant::now();
    match index.save() {
        Ok(size) => println!("  saved {:.1} MB in {:?}", size as f64 / MB, t.elapsed()),
        Err(e) => println!("  save failed: {:#}", e),
    }
    let t = Instant::now();
    match crate::index::persist::load_current(index.volume) {
        Ok(loaded) => println!(
            "  loaded {} entries in {:?} (identical: {})",
            loaded.file_count(),
            t.elapsed(),
            loaded.sorted == index.sorted && loaded.names == index.names
        ),
        Err(e) => println!("  load failed: {:#}", e),
    }
}

fn print_index_memory(index: &VolumeIndex) {
    println!(
        "  index memory: {:.1} MB ({:.1} MB names, {:.1} MB records, {:.1} MB sorted) for {} record slots",
        index.heap_bytes() as f64 / MB,
        index.names.capacity() as f64 / MB,
        index.records.heap_bytes() as f64 / MB,
        index.sorted.capacity() as f64 * 4.0 / MB,
        index.records.len()
    );
}

/// Prints a few well known files as a sanity check.
fn print_samples(index: &VolumeIndex) {
    for folder in ["Windows", "Users", "Program Files"] {
        if let Some(&id) = index
            .sorted
            .iter()
            .find(|&&id| index.parent(id) == crate::ntfs::ROOT_RECORD && index.name(id) == folder.as_bytes())
        {
            println!("  {}: {:.2} GB", index.full_path(id), index.size(id) as f64 / (MB * 1024.0));
        }
    }
    for wanted in ["notepad.exe", "explorer.exe", "ntoskrnl.exe"] {
        let hits = index
            .sorted
            .iter()
            .filter(|&&id| index.name(id).eq_ignore_ascii_case(wanted.as_bytes()))
            .filter(|&&id| !index.folder_path(id).contains("WinSxS"))
            .take(3)
            .map(|&id| format!("{} ({} B)", index.full_path(id), index.size(id)))
            .collect::<Vec<_>>();
        if !hits.is_empty() {
            println!("  {}: {}", wanted, hits.join(", "));
        }
    }
}

pub fn print_memory(label: &str) {
    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    unsafe {
        let _ = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        );
    }
    println!(
        "  mem [{}]: working set {:.1} MB (peak {:.1} MB), private {:.1} MB (peak {:.1} MB)",
        label,
        counters.WorkingSetSize as f64 / MB,
        counters.PeakWorkingSetSize as f64 / MB,
        counters.PagefileUsage as f64 / MB,
        counters.PeakPagefileUsage as f64 / MB,
    );
}

/// Makes real changes in a temp directory and checks that the journal brings the index in sync.
fn journal_selftest(index: &mut VolumeIndex) -> Result<()> {
    use crate::service::JournalFollower;
    use std::io::Write;

    let dir = std::env::temp_dir().join(format!("rv-journal-test-{}", std::process::id()));
    let dir_name = dir.file_name().unwrap().to_string_lossy().to_string();
    let mut follower = JournalFollower::new(index)?;

    let mut sync = |index: &mut VolumeIndex| -> Result<usize> {
        let t = Instant::now();
        let changed = follower.poll_changes().map_err(|e| eyre::eyre!("{:?}", e))?;
        let updates = follower.fetch(&changed);
        index.apply_updates(&updates, follower.reader.next_usn());
        println!("  synced {} records ({} updates) in {:?}", changed.len(), updates.len(), t.elapsed());
        Ok(updates.len())
    };
    let find = |index: &VolumeIndex, name: &str| -> Vec<u32> {
        index
            .sorted
            .iter()
            .copied()
            .filter(|&id| index.name(id) == name.as_bytes())
            .filter(|&id| index.folder_path(id).ends_with(&dir_name))
            .collect()
    };
    let mut ok = true;
    let mut check = |what: &str, cond: bool| {
        println!("  [{}] {}", if cond { "ok" } else { "FAIL" }, what);
        ok &= cond;
    };

    // Create, write, rename
    std::fs::create_dir(&dir)?;
    std::fs::write(dir.join("alpha.txt"), vec![b'a'; 100])?;
    std::fs::write(dir.join("beta.txt"), b"beta")?;
    std::fs::rename(dir.join("beta.txt"), dir.join("gamma.txt"))?;
    std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("alpha.txt"))?
        .write_all(&[b'b'; 200])?;
    sync(index)?;

    let alpha = find(index, "alpha.txt");
    check("alpha.txt indexed", alpha.len() == 1);
    check(
        "alpha.txt has size 300",
        alpha.first().is_some_and(|&id| index.size(id) == 300),
    );
    check("beta.txt gone after rename", find(index, "beta.txt").is_empty());
    check("gamma.txt indexed", find(index, "gamma.txt").len() == 1);
    let dir_id = index
        .sorted
        .iter()
        .copied()
        .find(|&id| index.name(id) == dir_name.as_bytes());
    check(
        "folder size is 304",
        dir_id.is_some_and(|id| index.size(id) == 304),
    );

    // Hard links show up as their own entries and count towards folder sizes
    std::fs::hard_link(dir.join("alpha.txt"), dir.join("alpha-link.txt"))?;
    sync(index)?;
    check("hard link indexed", find(index, "alpha-link.txt").len() == 1);
    check("original name still indexed", find(index, "alpha.txt").len() == 1);
    check(
        "folder size counts the link",
        dir_id.is_some_and(|id| index.size(id) == 604),
    );
    std::fs::remove_file(dir.join("alpha-link.txt"))?;
    sync(index)?;
    check("hard link gone after delete", find(index, "alpha-link.txt").is_empty());
    check("original survives link delete", find(index, "alpha.txt").len() == 1);
    check(
        "sorted order intact",
        index.sorted.windows(2).all(|w| index.cmp_entries(w[0], w[1]).is_lt()),
    );

    // Move a file into a new sub directory and delete another one
    std::fs::create_dir(dir.join("sub"))?;
    std::fs::rename(dir.join("gamma.txt"), dir.join("sub").join("delta.txt"))?;
    std::fs::remove_file(dir.join("alpha.txt"))?;
    sync(index)?;

    check("alpha.txt gone after delete", find(index, "alpha.txt").is_empty());
    check(
        "folder size follows move and delete",
        dir_id.is_some_and(|id| index.size(id) == 4),
    );
    let delta = index
        .sorted
        .iter()
        .copied()
        .filter(|&id| index.name(id) == b"delta.txt")
        .find(|&id| index.folder_path(id).ends_with(&format!(r"{}\sub", dir_name)));
    check("delta.txt indexed in sub directory", delta.is_some());

    std::fs::remove_dir_all(&dir)?;
    sync(index)?;
    check(
        "test directory gone",
        !index.sorted.iter().any(|&id| index.name(id) == dir_name.as_bytes()),
    );

    println!("journal self test: {}", if ok { "PASSED" } else { "FAILED" });
    Ok(())
}
