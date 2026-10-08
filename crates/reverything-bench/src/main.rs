//! Development tool for measuring search performance on large indices. Needs no admin rights.
//!
//! ```text
//! reverything-bench synth [--from DIR] [--to DIR] [--spec SPEC] [--seed N]
//!     builds synthetic indices from saved ones, see synth.rs
//! reverything-bench search [--dir DIR] [--runs N] [--only TEXT] [--save FILE]
//!     times the query suite in process
//! reverything-bench compare BEFORE.json AFTER.json
//!     compares two saved runs
//! reverything-bench journal [--dir DIR] [--volume C] [--runs N]
//!     times applying batches of journal changes to one volume
//! reverything-bench load [--dir DIR] [--runs N]
//!     times loading saved indices like waking up, per phase
//! reverything-bench replay serial|pipeline|cancel [--interval MS]
//!     types queries against a running service (REVERYTHING_PIPE), see replay.rs
//! ```
//!
//! The saved indices come from the elevated `reverything-service --bench`, which writes them
//! to `%LOCALAPPDATA%\reverything-dev`. Synthetic ones go to `%LOCALAPPDATA%\reverything-synth`
//! by default.

use std::path::PathBuf;

use eyre::{bail, Result};
use mimalloc::MiMalloc;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::GetCurrentProcess;

use reverything_core::index::persist::dev_db_dir;

mod journal;
mod load;
mod phases;
mod replay;
mod search;
mod synth;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn synth_dir() -> PathBuf {
    dev_db_dir().with_file_name("reverything-synth")
}

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let value = |name: &str| -> Option<&str> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    };
    match args.first().map(String::as_str) {
        Some("synth") => synth::run(
            &value("--from").map_or_else(dev_db_dir, PathBuf::from),
            &value("--to").map_or_else(synth_dir, PathBuf::from),
            value("--spec").unwrap_or(synth::DEFAULT_SPEC),
            value("--seed").map_or(Ok(1), str::parse)?,
        ),
        Some("search") => {
            phases::install();
            search::run(
                &value("--dir").map_or_else(synth_dir, PathBuf::from),
                value("--runs").map_or(Ok(5), str::parse)?,
                value("--only"),
                value("--save").map(PathBuf::from).as_deref(),
            )
        }
        Some("journal") => {
            phases::install();
            journal::run(
                &value("--dir").map_or_else(synth_dir, PathBuf::from),
                value("--volume")
                    .and_then(|v| v.chars().next())
                    .unwrap_or('C'),
                value("--runs").map_or(Ok(5), str::parse)?,
            )
        }
        Some("load") => {
            phases::install();
            load::run(
                &value("--dir").map_or_else(synth_dir, PathBuf::from),
                value("--runs").map_or(Ok(3), str::parse)?,
            )
        }
        Some("replay") if args.len() >= 2 => replay::run(
            &args[1],
            std::time::Duration::from_millis(value("--interval").map_or(Ok(80), str::parse)?),
        ),
        Some("compare") if args.len() == 3 => search::compare(args[1].as_ref(), args[2].as_ref()),
        _ => bail!("Usage: see the top of crates/reverything-bench/src/main.rs"),
    }
}

pub fn print_memory(label: &str) {
    let mut pmc = PROCESS_MEMORY_COUNTERS::default();
    unsafe {
        let _ = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut pmc,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        );
    }
    println!(
        "memory {}: working set {:.0} MB (peak {:.0} MB)",
        label,
        pmc.WorkingSetSize as f64 / 1048576.0,
        pmc.PeakWorkingSetSize as f64 / 1048576.0
    );
}
