use std::time::Instant;

use crate::ntfs::index::NtfsVolumeIndex;
use crate::ntfs::volume::get_volumes;
use eyre::{ContextCompat, Result};
use mimalloc_rust::GlobalMiMalloc;

mod ntfs;
mod ui;

#[global_allocator]
static GLOBAL: GlobalMiMalloc = GlobalMiMalloc;

fn main() -> Result<()> {
    let index = build_index()?;
    ui::run_ui(index)?;
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
