//! Small indices for tests.

use crate::index::{Records, VolumeIndex, FLAG_DIRECTORY, FLAG_IN_USE};
use crate::ntfs::volume::Volume;
use crate::ntfs::ROOT_RECORD;

/// A volume with the given names directly below the root, record numbers from 16 up.
pub fn volume(letter: char, names: &[String]) -> VolumeIndex {
    let mut index = VolumeIndex::empty(Volume { id: letter });
    index.records = Records::with_len(16 + names.len());
    let r = &mut index.records;
    r.flags[ROOT_RECORD as usize] = FLAG_IN_USE | FLAG_DIRECTORY;
    r.parent[ROOT_RECORD as usize] = ROOT_RECORD;
    for (i, name) in names.iter().enumerate() {
        let id = 16 + i;
        r.flags[id] = FLAG_IN_USE;
        r.parent[id] = ROOT_RECORD;
        r.name_off[id] = index.names.len() as u32;
        r.name_len[id] = name.len() as u16;
        index.names.extend_from_slice(name.as_bytes());
    }
    index.sort_and_compact();
    index
}

/// Names that share prefixes longer than the 8 byte sort key, differ in case, and repeat
pub fn names(seed: u64, n: usize) -> Vec<String> {
    const PARTS: &[&str] = &[
        "a",
        "A",
        "b",
        "Readme",
        "readme",
        "README.md",
        "longprefix",
        "x.",
        "ä",
        "Z",
    ];
    let mut x = seed;
    (0..n)
        .map(|_| {
            let mut name = String::new();
            for _ in 0..1 + x % 3 {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                name.push_str(PARTS[(x >> 33) as usize % PARTS.len()]);
            }
            name
        })
        .collect()
}

/// A volume with the given paths below the root, like `a\b\c.txt`. Folders on the way are
/// created, a trailing `\` makes the entry itself a folder.
pub fn tree(letter: char, paths: &[&str]) -> VolumeIndex {
    let mut entries: Vec<(String, u32, bool)> = Vec::new();
    for path in paths {
        let mut parent = ROOT_RECORD;
        let parts = path
            .split('\\')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>();
        for (i, part) in parts.iter().enumerate() {
            let directory = i + 1 < parts.len() || path.ends_with('\\');
            let existing = entries
                .iter()
                .position(|(name, p, _)| name == part && *p == parent);
            let id = match existing {
                Some(at) => 16 + at as u32,
                None => {
                    entries.push((part.to_string(), parent, directory));
                    16 + entries.len() as u32 - 1
                }
            };
            parent = id;
        }
    }
    let mut index = VolumeIndex::empty(Volume { id: letter });
    index.records = Records::with_len(16 + entries.len());
    let r = &mut index.records;
    r.flags[ROOT_RECORD as usize] = FLAG_IN_USE | FLAG_DIRECTORY;
    r.parent[ROOT_RECORD as usize] = ROOT_RECORD;
    for (i, (name, parent, directory)) in entries.iter().enumerate() {
        let id = 16 + i;
        r.flags[id] = FLAG_IN_USE | if *directory { FLAG_DIRECTORY } else { 0 };
        r.parent[id] = *parent;
        r.name_off[id] = index.names.len() as u32;
        r.name_len[id] = name.len() as u16;
        index.names.extend_from_slice(name.as_bytes());
    }
    index.sort_and_compact();
    index
}
