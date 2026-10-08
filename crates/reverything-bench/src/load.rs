//! Times loading saved indices the way the service does when it wakes up, per phase, and
//! building what the first ranked search needs.

use std::path::Path;
use std::time::{Duration, Instant};

use eyre::Result;
use serde::Deserialize;

use reverything_core::index::persist::load_offline;
use reverything_core::ntfs::volume::Volume;

use crate::phases;

pub fn run(dir: &Path, runs: usize) -> Result<()> {
    #[derive(Deserialize)]
    struct Config {
        volumes: Vec<char>,
    }
    let config: Config = serde_json::from_slice(&std::fs::read(dir.join("config.json"))?)?;
    for letter in config.volumes {
        for _ in 0..runs {
            let t = Instant::now();
            let index = load_offline(Volume { id: letter }, dir)?;
            let load = t.elapsed();
            let t = Instant::now();
            index.locations();
            let locations = t.elapsed();
            let phases = phases::take()
                .into_iter()
                .filter(|(name, _)| name.starts_with("load."))
                .map(|(name, d)| format!("{} {:.1}", name.trim_start_matches("load."), ms(d)))
                .collect::<Vec<_>>();
            println!(
                "{}: {} entries loaded in {:.1} ms ({}), ranking locations {:.1} ms",
                letter,
                index.file_count(),
                ms(load),
                phases.join(", "),
                ms(locations)
            );
        }
    }
    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
