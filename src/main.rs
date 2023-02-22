#![feature(let_chains)]

use std::time::Instant;

use eyre::{ContextCompat, Result};
use mimalloc::MiMalloc;
use widestring::Utf16String;

use crate::ntfs::index::NtfsVolumeIndex;
use crate::ntfs::volume::get_volumes;

mod ntfs;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

pub struct FileInfo {
    name: Utf16String,
    parent: u64,
}

fn main() -> Result<()> {
    let t = Instant::now();
    let vol = get_volumes()
        .into_iter()
        .next()
        .with_context(|| "Cannot find first volume")?;

    let index = NtfsVolumeIndex::new(vol)?;

    let info = index.find_by_name("idea64.exe").unwrap();
    println!(
        "{:?}",
        index.compute_full_path(info),
    );

    let mut s = 0usize;
    index.iter().for_each(|info| {
        s += index.compute_full_path(info).len();
    });

    println!("{}", s);
    println!("Elapsed: {:?}", t.elapsed());

    Ok(())
}
