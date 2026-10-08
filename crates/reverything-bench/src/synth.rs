//! Synthetic indices: scales real saved indices up to tens of millions of entries, so searches
//! can be measured at sizes beyond the development machine's own drives.
//!
//! A target volume is built from parts, each a copy of a source volume's tree. The first part
//! sits at the root of the target, later ones below `\copy<n>`. Copies keep the real name and
//! path distribution, including the many duplicate names real drives have, but a share of their
//! names gets a short token inserted before the extension so they are not all exact duplicates.
//! A part can keep only a fraction of the source: whole subtrees of at most [`SUBTREE`] entries
//! are kept or dropped. Everything is derived from a seed, so a spec always gives the same index.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::Path;

use eyre::{bail, ensure, eyre, Result};
use rayon::prelude::*;

use reverything_core::index::persist::load_offline;
use reverything_core::index::{
    Records, VolumeIndex, FLAG_HAS_LINKS, FLAG_IN_USE, LINK_BIT, MAX_DEPTH, NO_RECORD,
};
use reverything_core::ntfs::volume::Volume;
use reverything_core::ntfs::ROOT_RECORD;

/// Largest subtree a fractional part keeps or drops as a whole
const SUBTREE: u32 = 5_000;
/// Percentage of names changed in copies
const MUTATE_PERCENT: u64 = 30;

/// The default layout: about 22 million entries on four volumes of different sizes, from the
/// development machine's C: (4.2M entries) and D: (0.16M).
pub const DEFAULT_SPEC: &str = "C=C+C+C,D=D+C@0.6,E=C,F=C@0.5";

struct Part {
    source: char,
    fraction: f64,
}

struct Target {
    letter: char,
    parts: Vec<Part>,
}

/// Parses `C=C+C+C,E=C@0.5`: target volume `=` parts joined by `+`, each a source volume with
/// an optional `@fraction`.
fn parse_spec(spec: &str) -> Result<Vec<Target>> {
    let letter = |s: &str| -> Result<char> {
        let mut chars = s.trim().chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => Ok(c.to_ascii_uppercase()),
            _ => bail!("Not a drive letter: {:?}", s),
        }
    };
    spec.split(',')
        .map(|target| {
            let (name, parts) = target
                .split_once('=')
                .ok_or_else(|| eyre!("Expected <letter>=<parts> in {:?}", target))?;
            let parts = parts
                .split('+')
                .map(|part| {
                    let (source, fraction) = match part.split_once('@') {
                        Some((s, f)) => (s, f.trim().parse::<f64>()?),
                        None => (part, 1.0),
                    };
                    ensure!(
                        fraction > 0.0 && fraction <= 1.0,
                        "Fraction out of range in {:?}",
                        part
                    );
                    Ok(Part {
                        source: letter(source)?,
                        fraction,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Target {
                letter: letter(name)?,
                parts,
            })
        })
        .collect()
}

pub fn run(source_dir: &Path, out_dir: &Path, spec: &str, seed: u64) -> Result<()> {
    let targets = parse_spec(spec)?;
    ensure!(
        out_dir != source_dir,
        "The output directory must differ from the source directory"
    );

    let mut sources = HashMap::new();
    for part in targets.iter().flat_map(|t| &t.parts) {
        if let Entry::Vacant(slot) = sources.entry(part.source) {
            let t = std::time::Instant::now();
            let index = load_offline(Volume { id: part.source }, source_dir)?;
            println!(
                "loaded {}: {} entries in {:?}",
                part.source,
                index.file_count(),
                t.elapsed()
            );
            slot.insert(index);
        }
    }

    std::fs::create_dir_all(out_dir)?;
    let mut total = 0;
    for target in &targets {
        let t = std::time::Instant::now();
        let mut index = build(target, &sources, seed)?;
        index.sort_and_compact();
        index.compute_folder_sizes();
        let bytes = index.save(out_dir)?;
        println!(
            "{}: {} entries, {:.1} MB in memory, {:.1} MB saved, in {:?}",
            target.letter,
            index.file_count(),
            index.heap_bytes() as f64 / 1048576.0,
            bytes as f64 / 1048576.0,
            t.elapsed()
        );
        total += index.file_count();
    }

    // The offline service indexes the volumes listed here
    let letters = targets.iter().map(|t| t.letter).collect::<Vec<_>>();
    std::fs::write(
        out_dir.join("config.json"),
        serde_json::to_vec_pretty(&serde_json::json!({ "volumes": letters }))?,
    )?;
    println!("{} entries in {}", total, out_dir.display());
    Ok(())
}

fn build(target: &Target, sources: &HashMap<char, VolumeIndex>, seed: u64) -> Result<VolumeIndex> {
    let slots = target
        .parts
        .iter()
        .map(|p| sources[&p.source].records.len())
        .sum::<usize>();
    ensure!(
        slots < LINK_BIT as usize,
        "{}: too many records",
        target.letter
    );

    let mut out = VolumeIndex::empty(Volume { id: target.letter });
    out.volume_serial = seed ^ target.letter as u64;
    out.records = Records::with_len(slots);
    let mut base = 0u32;
    for (n, part) in target.parts.iter().enumerate() {
        let src = &sources[&part.source];
        let salt = mix(seed ^ mix(target.letter as u64) ^ mix(n as u64 + 1));
        let keep = keep_mask(src, part.fraction, salt);
        // The first part of a volume built from itself stays the original
        let mutate = n > 0 || part.source != target.letter;
        copy_part(&mut out, src, base, n, &keep, mutate.then_some(salt));
        base += src.records.len() as u32;
    }
    Ok(out)
}

/// Copies the kept entries of `src` into `out`, with record numbers shifted by `base`.
fn copy_part(
    out: &mut VolumeIndex,
    src: &VolumeIndex,
    base: u32,
    part: usize,
    keep: &[bool],
    mutate: Option<u64>,
) {
    let add_name = |names: &mut Vec<u8>, name: &[u8], key: u64| -> (u32, u16) {
        let off = names.len() as u32;
        match mutate {
            Some(salt) if mix(salt ^ key) % 100 < MUTATE_PERCENT => {
                push_mutated(names, name, mix(salt ^ key ^ 0x5bd1e995))
            }
            _ => names.extend_from_slice(name),
        }
        (off, (names.len() - off as usize) as u16)
    };

    let (sr, r) = (&src.records, &mut out.records);
    for (id, &kept) in keep.iter().enumerate() {
        if sr.flags[id] & FLAG_IN_USE == 0 || !kept {
            continue;
        }
        let t = base as usize + id;
        let root = id as u32 == ROOT_RECORD;
        (r.name_off[t], r.name_len[t]) = if root && part > 0 {
            let name = format!("copy{}", part);
            let off = out.names.len() as u32;
            out.names.extend_from_slice(name.as_bytes());
            (off, name.len() as u16)
        } else {
            add_name(&mut out.names, src.name(id as u32), id as u64)
        };
        r.parent[t] = if root {
            ROOT_RECORD
        } else {
            sr.parent[id] + base
        };
        // Set again below for the links that are kept
        r.flags[t] = sr.flags[id] & !FLAG_HAS_LINKS;
        r.size[t] = sr.size[id];
        r.created[t] = sr.created[id];
        r.modified[t] = sr.modified[id];
        r.sequence[t] = sr.sequence[id];
    }

    let sl = &src.links;
    for l in 0..sl.len() {
        let (record, parent) = (sl.record[l], sl.parent[l]);
        if record == NO_RECORD
            || !keep[record as usize]
            || !keep.get(parent as usize).copied().unwrap_or(false)
        {
            continue;
        }
        let name = src.name(l as u32 | LINK_BIT);
        let (off, len) = add_name(&mut out.names, name, (l as u64) << 32 | 1);
        out.links.push(record + base, parent + base, off, len);
        out.records.flags[(record + base) as usize] |= FLAG_HAS_LINKS;
    }
}

/// Inserts `_xxxx` before the extension, or appends it without one.
fn push_mutated(names: &mut Vec<u8>, name: &[u8], h: u64) {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let token = (0..4).map(|i| ALPHABET[(h >> (i * 8)) as usize % ALPHABET.len()]);
    let split = match name.iter().rposition(|&b| b == b'.') {
        Some(dot) if dot > 0 => dot,
        _ => name.len(),
    };
    names.extend_from_slice(&name[..split]);
    names.push(b'_');
    names.extend(token);
    names.extend_from_slice(&name[split..]);
}

/// Which records of `src` a part with `fraction` keeps. Subtrees of at most [`SUBTREE`] entries
/// are kept or dropped as a whole, larger directories are always kept.
fn keep_mask(src: &VolumeIndex, fraction: f64, salt: u64) -> Vec<bool> {
    let r = &src.records;
    let n = r.len();
    if fraction >= 1.0 {
        return vec![true; n];
    }
    let in_use = |id: u32| {
        r.flags
            .get(id as usize)
            .is_some_and(|f| f & FLAG_IN_USE != 0)
    };
    // Ancestors of an in-use record, from the parent up to (excluding) the root
    let ancestors = |id: u32| {
        let mut cur = r.parent[id as usize];
        let mut depth = 0;
        std::iter::from_fn(move || {
            if cur == ROOT_RECORD || depth >= MAX_DEPTH || !in_use(cur) {
                return None;
            }
            let dir = cur;
            cur = r.parent[dir as usize];
            depth += 1;
            Some(dir)
        })
    };

    // Entries in each subtree
    let mut count = vec![0u32; n];
    for id in 0..n as u32 {
        if in_use(id) && id != ROOT_RECORD {
            count[id as usize] += 1;
            for a in ancestors(id) {
                count[a as usize] += 1;
            }
        }
    }

    let threshold = (fraction * u64::MAX as f64) as u64;
    (0..n as u32)
        .into_par_iter()
        .map(|id| {
            if !in_use(id) || id == ROOT_RECORD || count[id as usize] > SUBTREE {
                return true;
            }
            // The topmost ancestor whose subtree is small enough decides
            let mut bucket = id;
            for a in ancestors(id) {
                if count[a as usize] > SUBTREE {
                    break;
                }
                bucket = a;
            }
            mix(salt ^ bucket as u64) < threshold
        })
        .collect()
}

/// splitmix64 finalizer
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}
