#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // Hide console window on Windows in release
#![feature(let_chains)]

use std::time::{Duration, Instant};

use eyre::{ContextCompat, Result};
use mimalloc::MiMalloc;

use crate::ntfs::index::NtfsVolumeIndex;
use crate::ntfs::journal::Journal;
use crate::ntfs::volume::get_volumes;
use crate::ui::start_ui;


mod ntfs;
mod ui;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Debug)]
pub struct FileInfo {
    name: String,
    parent: u64,
    is_directory: bool,
}

fn main() -> Result<()> {
    color_eyre::install()?;

    start_ui(build_index()?);

    Ok(())
}

fn build_index() -> Result<NtfsVolumeIndex> {
    let t = Instant::now();
    let vol = get_volumes()
        .into_iter()
        .next()
        .with_context(|| "Cannot find first volume")?;

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
