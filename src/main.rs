use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::ntfs::index::NtfsVolumeIndex;
use crate::ntfs::journal::Journal;
use crate::ntfs::volume::get_volumes;
use eyre::{ContextCompat, Result};
use mimalloc_rust::GlobalMiMalloc;

mod ntfs;
mod ui;

#[global_allocator]
static GLOBAL: GlobalMiMalloc = GlobalMiMalloc;

fn main() -> Result<()> {
    let vol = get_volumes()
        .into_iter()
        .next()
        .with_context(|| "Cannot find first volume")?;
    let journal = Journal::new(vol)?;

    let t = Instant::now();
    let index = Arc::new(Mutex::new(NtfsVolumeIndex::new(vol)?));
    println!("Building index took: {:?}", t.elapsed());

    start_journal_thread(journal, index.clone());
    
    ui::run_ui(index.clone())?;
    Ok(())
}

fn start_journal_thread(mut journal: Journal, index: Arc<Mutex<NtfsVolumeIndex>>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));

            let vec = journal.read_entries().unwrap();
            if vec.is_empty() {
                continue;
            }

            let mut index = index.lock().unwrap();
            index.process_journal_entries(&vec);
        }
    });
}
