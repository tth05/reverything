#![feature(let_chains)]

use std::time::{Duration, Instant};

use eyre::{ContextCompat, Result};
use mimalloc::MiMalloc;

use crate::ntfs::index::NtfsVolumeIndex;
use crate::ntfs::volume::get_volumes;

mod ntfs;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

pub struct FileInfo {
    name: String,
    parent: u64,
}

pub struct FileInfo2 {
    name: Better,
    parent: u64,
}

enum Better {
    Valid { len: u8, str_pool_idx: u32 },
    Invalid,
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let t = Instant::now();
    let vol = get_volumes()
        .into_iter()
        .next()
        .with_context(|| "Cannot find first volume")?;

    let index = NtfsVolumeIndex::new(vol)?;

    let info = index.find_by_name("idea64.exe").unwrap();
    println!("{:?}", index.compute_full_path(info),);

    let mut s = 0usize;
    index.iter().for_each(|info| {
        s += index.compute_full_path(info).len();
    });

    println!("{}", s);
    println!("Elapsed: {:?}", t.elapsed());
    std::thread::sleep(Duration::from_secs(10));

    Ok(())
}
