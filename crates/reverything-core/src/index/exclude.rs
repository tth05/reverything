//! Excluding folders (and everything below them) from search results.

use std::cmp::Ordering;

use crate::index::{VolumeIndex, FLAG_DIRECTORY, FLAG_IN_USE, MAX_DEPTH};
use crate::ntfs::ROOT_RECORD;

const DIR_IN_USE: u32 = FLAG_IN_USE | FLAG_DIRECTORY;

/// ASCII case-insensitive comparison without the byte-wise tie break of
/// [`crate::index::sort::cmp_names`], so all case variants of a name compare equal. They are
/// next to each other in [`VolumeIndex::sorted`].
fn cmp_folded(a: &[u8], b: &[u8]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (x.to_ascii_lowercase(), y.to_ascii_lowercase());
        if x != y {
            return x.cmp(&y);
        }
    }
    a.len().cmp(&b.len())
}

impl VolumeIndex {
    /// Finds a directory by its full path, e.g. `C:\Users\me\AppData`. Case-insensitive for
    /// ASCII letters.
    pub fn find_directory(&self, path: &str) -> Option<u32> {
        let mut parts = path.split(['\\', '/']).filter(|p| !p.is_empty());
        let drive = parts.next()?;
        let letter = drive.strip_suffix(':')?;
        if !letter.eq_ignore_ascii_case(&self.volume.id.to_string()) {
            return None;
        }

        let mut current = ROOT_RECORD;
        for part in parts {
            let name = part.as_bytes();
            let start = self
                .sorted
                .partition_point(|&id| cmp_folded(self.name(id), name) == Ordering::Less);
            current = self.sorted[start..]
                .iter()
                .take_while(|&&id| cmp_folded(self.name(id), name) == Ordering::Equal)
                .copied()
                .find(|&id| {
                    self.parent(id) == current && self.flags(id) & DIR_IN_USE == DIR_IN_USE
                })?;
        }
        Some(current)
    }

    /// Bitset over record numbers of the given directories and every directory below them.
    pub fn excluded_dirs(&self, roots: &[u32]) -> Vec<u64> {
        const UNKNOWN: u8 = 0;
        const EXCLUDED: u8 = 1;
        const INCLUDED: u8 = 2;

        let n = self.records.len();
        let mut state = vec![UNKNOWN; n];
        for &root in roots {
            if let Some(s) = state.get_mut(root as usize) {
                *s = EXCLUDED;
            }
        }
        if let Some(s) = state.get_mut(ROOT_RECORD as usize) {
            if *s == UNKNOWN {
                *s = INCLUDED;
            }
        }

        let flags = &self.records.flags;
        let parent = &self.records.parent;
        let mut chain = Vec::new();
        for i in 0..n {
            if flags[i] & DIR_IN_USE != DIR_IN_USE || state[i] != UNKNOWN {
                continue;
            }
            // Walk up until a directory with a known state, then give the whole chain that state
            let mut cur = i;
            let result = loop {
                if state[cur] != UNKNOWN {
                    break state[cur];
                }
                if chain.len() > MAX_DEPTH {
                    break INCLUDED;
                }
                chain.push(cur);
                let p = parent[cur] as usize;
                if p >= n || p == cur || flags[p] & DIR_IN_USE != DIR_IN_USE {
                    break INCLUDED;
                }
                cur = p;
            };
            for c in chain.drain(..) {
                state[c] = result;
            }
        }

        let mut bits = vec![0u64; n.div_ceil(64)];
        for (i, &s) in state.iter().enumerate() {
            if s == EXCLUDED {
                bits[i / 64] |= 1 << (i % 64);
            }
        }
        bits
    }
}
