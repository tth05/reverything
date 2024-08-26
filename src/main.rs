#![feature(let_chains)]

use std::io::Write;
use std::time::Instant;

use crate::ntfs::index::{FileInfo, NtfsVolumeIndex};
use crate::ntfs::volume::get_volumes;
use eyre::{ContextCompat, Result};
use mimalloc_rust::GlobalMiMalloc;

mod ntfs;

#[global_allocator]
static GLOBAL: GlobalMiMalloc = GlobalMiMalloc;

fn main() -> Result<()> {
    let index = build_index()?;
    println!("{}", std::mem::size_of::<FileInfo>());
    println!(
        "File count: {}",
        index
            .iter()
            .map(|f| 40 + if !f.name.is_inline() { f.name.len() } else { 0 })
            .sum::<usize>()
    );
    println!(
        "File count: {}, {}",
        index.iter().count(),
        index
            .iter()
            .map(|f| index.compute_full_path(f).replace("C:\\", "").len())
            .sum::<usize>()
    );
    println!("bro");
    let now = Instant::now();
    println!("{}", index.find_by_name("bscstdlib").unwrap().size());
    println!("Elapsed: {:?}", now.elapsed());

    std::thread::sleep(std::time::Duration::from_secs(100));
    Ok(())
}

fn build_index() -> Result<NtfsVolumeIndex> {
    let t = Instant::now();
    let vol = get_volumes()
        .into_iter()
        .next()
        .with_context(|| "Cannot find first volume")?;
    println!("Volume: {:?}", vol);

    /*if true {
        let mut j = Journal::new(vol)?;
        let mut i = 0;
        loop {
            let vec = j.read_entries()?;
            if vec.is_empty() {
                break;
            }
            println!("{} {:?}", i, vec);
            i += 1;
        }
        return Ok(());
    }*/

    let index = NtfsVolumeIndex::new(vol)?;
    println!("Elapsed: {:?}", t.elapsed());

    Ok(index)
}
