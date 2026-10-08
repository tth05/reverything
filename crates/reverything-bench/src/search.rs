//! Times searches in process, the way the service runs them, on a directory of saved indices.
//! Each query runs several times; the table shows the median and the slowest run, and the
//! median time of each `search.*` phase.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use eyre::{ensure, Result};
use serde::{Deserialize, Serialize};

use reverything_core::index::persist::load_offline;
use reverything_core::index::search::Query;
use reverything_core::ntfs::volume::Volume;
use reverything_core::results::Results;
use reverything_core::search::{exclusions, read_all, search_all, Sort, SortColumn};
use reverything_core::service::IndexSet;

use crate::phases;

/// Queries as typed, with the sort the app would use. Typing sequences show what each keystroke
/// costs; the default sort in the app is relevance.
const QUERIES: &[(&str, SortColumn)] = &[
    ("", SortColumn::Relevance),
    ("", SortColumn::Name),
    ("n", SortColumn::Relevance),
    ("no", SortColumn::Relevance),
    ("not", SortColumn::Relevance),
    ("note", SortColumn::Relevance),
    ("notep", SortColumn::Relevance),
    ("notepad", SortColumn::Relevance),
    ("e", SortColumn::Relevance),
    ("e", SortColumn::Name),
    ("s", SortColumn::Relevance),
    (".", SortColumn::Relevance),
    (".dll", SortColumn::Relevance),
    (".dll", SortColumn::Size),
    (".dll", SortColumn::Modified),
    ("*.jpg", SortColumn::Relevance),
    ("readme", SortColumn::Relevance),
    ("index.js", SortColumn::Relevance),
    ("node_modules", SortColumn::Relevance),
    ("zzqqxy", SortColumn::Relevance),
    ("Ärger", SortColumn::Relevance),
    (r"system32\", SortColumn::Relevance),
    (r"windows\system32\note", SortColumn::Relevance),
    (r"c:\windows\", SortColumn::Relevance),
    (r"c:\users\", SortColumn::Path),
    (r"notepad !winsxs", SortColumn::Relevance),
    (r".rs !target\", SortColumn::Relevance),
    (r"!C:\Windows !C:\Users", SortColumn::Relevance),
    ("size:>100mb", SortColumn::Relevance),
    ("dm:today", SortColumn::Relevance),
    // Sorting by a column instead of relevance
    ("", SortColumn::Size),
    ("", SortColumn::Modified),
    ("", SortColumn::Created),
    ("", SortColumn::Attributes),
    ("", SortColumn::Path),
    ("e", SortColumn::Size),
    ("e", SortColumn::Modified),
    (".dll", SortColumn::Path),
];

#[derive(Serialize, Deserialize)]
pub struct Sample {
    pub query: String,
    pub sort: String,
    pub hits: usize,
    pub median_us: u64,
    pub max_us: u64,
    /// Median per phase
    pub phases_us: BTreeMap<String, u64>,
}

pub fn run(dir: &Path, runs: usize, filter: Option<&str>, save: Option<&Path>) -> Result<()> {
    ensure!(runs > 0, "Need at least one run");
    let set = load(dir)?;
    crate::print_memory("after loading");

    // Warm up: builds the lazily computed ranking locations, touches the names once
    for _ in 0..2 {
        search_all(&set, &Query::parse(""), Sort::default(), &[]);
        search_all(&set, &Query::parse("a"), Sort::default(), &[]);
    }
    phases::take();

    let mut samples = Vec::new();
    // The service keeps the last result per session and drops it when the next one arrives
    let mut previous = Results::default();
    println!(
        "{:<28} {:>9} {:>10} {:>9} {:>9}  phases (median ms)",
        "query", "sort", "hits", "median", "max"
    );
    for &(text, column) in QUERIES {
        if filter.is_some_and(|f| !text.contains(f)) {
            continue;
        }
        let sort = Sort {
            column,
            ascending: true,
        };
        let mut times = Vec::with_capacity(runs);
        let mut phase_runs: BTreeMap<&'static str, Vec<Duration>> = BTreeMap::new();
        let mut hits = 0;
        for _ in 0..runs {
            let t = Instant::now();
            let query = Query::parse(text);
            let excluded = exclusions(&set, &query.folders);
            let result = search_all(&set, &query, sort, &excluded);
            hits = result.len();
            // The first two pages of rows, which the app reads right away
            for page in 0..2 {
                result.page(&read_all(&set), page * 256, 256);
            }
            {
                let _span = tracing::info_span!("search.drop_previous").entered();
                previous = result;
            }
            times.push(t.elapsed());
            for (name, d) in phases::take() {
                phase_runs.entry(name).or_default().push(d);
            }
        }
        times.sort();
        let phases_us = phase_runs
            .into_iter()
            .filter(|(name, _)| *name != "search")
            .map(|(name, mut d)| {
                d.sort();
                (
                    name.trim_start_matches("search.").to_string(),
                    median(&d).as_micros() as u64,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let sample = Sample {
            query: text.to_string(),
            sort: format!("{:?}", column),
            hits,
            median_us: median(&times).as_micros() as u64,
            max_us: times.last().unwrap().as_micros() as u64,
            phases_us,
        };
        println!(
            "{:<28} {:>9} {:>10} {:>9} {:>9}  {}",
            format!("{:?}", sample.query),
            sample.sort,
            sample.hits,
            ms(sample.median_us),
            ms(sample.max_us),
            sample
                .phases_us
                .iter()
                .filter(|(_, &us)| us >= 100)
                .map(|(name, &us)| format!("{} {}", name, ms(us)))
                .collect::<Vec<_>>()
                .join(", ")
        );
        samples.push(sample);
    }
    drop(previous);

    if let Some(path) = save {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&samples)?)?;
        println!("saved to {}", path.display());
    }
    Ok(())
}

/// Prints each query's median next to the one of an earlier `--save`d run.
pub fn compare(before: &Path, after: &Path) -> Result<()> {
    let read =
        |p: &Path| -> Result<Vec<Sample>> { Ok(serde_json::from_slice(&std::fs::read(p)?)?) };
    let (before, after) = (read(before)?, read(after)?);
    println!(
        "{:<28} {:>9} {:>10} {:>10} {:>8}",
        "query", "sort", "before", "after", "change"
    );
    for a in &after {
        let Some(b) = before
            .iter()
            .find(|b| b.query == a.query && b.sort == a.sort)
        else {
            continue;
        };
        let note = if b.hits != a.hits {
            "  (hit count differs!)"
        } else {
            ""
        };
        println!(
            "{:<28} {:>9} {:>10} {:>10} {:>7.0}%{}",
            format!("{:?}", a.query),
            a.sort,
            ms(b.median_us),
            ms(a.median_us),
            (a.median_us as f64 / b.median_us.max(1) as f64 - 1.0) * 100.0,
            note
        );
    }
    Ok(())
}

/// Loads every volume listed in the directory's `config.json`.
fn load(dir: &Path) -> Result<IndexSet> {
    #[derive(Deserialize)]
    struct Config {
        volumes: Vec<char>,
    }
    let config: Config = serde_json::from_slice(&std::fs::read(dir.join("config.json"))?)?;
    let set = IndexSet::new(dir.to_path_buf());
    let t = Instant::now();
    let mut total = 0;
    for &letter in &config.volumes {
        let index = load_offline(Volume { id: letter }, dir)?;
        println!("  {}: {} entries", letter, index.file_count());
        total += index.file_count();
        let slot = IndexSet::slot_of(letter).unwrap();
        *set.volumes[slot].index.write().unwrap() = index;
    }
    println!("loaded {} entries in {:?}", total, t.elapsed());
    Ok(std::sync::Arc::into_inner(set).expect("The index set is not shared yet"))
}

fn median(sorted: &[Duration]) -> Duration {
    sorted[sorted.len() / 2]
}

fn ms(us: u64) -> String {
    format!("{:.1}", us as f64 / 1000.0)
}
