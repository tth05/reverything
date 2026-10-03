use eyre::{bail, Result};
use mimalloc_rust::GlobalMiMalloc;

use crate::ntfs::volume::ntfs_volumes;
use crate::service::IndexSet;

mod bench;
mod index;
mod ntfs;
mod search;
mod service;
mod ui;

#[global_allocator]
static GLOBAL: GlobalMiMalloc = GlobalMiMalloc;

fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--bench") {
        return bench::run();
    }

    let volumes = ntfs_volumes();
    if volumes.is_empty() {
        bail!("No fixed NTFS volumes found");
    }

    let set = IndexSet::new(volumes);
    set.start();
    ui::run_ui(set.clone())?;
    set.save_all();
    Ok(())
}
