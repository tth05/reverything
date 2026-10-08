//! Folder order for sorting by folder: a folder comes right before everything in it, and the
//! folders in a folder come in name order, like a tree view. `C:\a` and all that is in it come
//! before `C:\a b`.

use std::sync::Arc;

use rayon::prelude::*;

use crate::index::{is_link, VolumeIndex, FLAG_DIRECTORY, FLAG_IN_USE};
use crate::ntfs::ROOT_RECORD;

const DIR_IN_USE: u32 = FLAG_IN_USE | FLAG_DIRECTORY;

/// Rank of records that are not directories, or directories not reachable from the root
pub const NO_RANK: u32 = u32::MAX;

impl VolumeIndex {
    /// The position of every directory in folder order, indexed by record number. Directories
    /// whose parent chain does not reach the root come after the others; everything else gets
    /// [`NO_RANK`]. Cached until directories change.
    pub fn folder_ranks(&self) -> Arc<Vec<u32>> {
        let mut cache = self.folder_ranks.lock().unwrap();
        cache
            .get_or_insert_with(|| Arc::new(self.compute_folder_ranks()))
            .clone()
    }

    fn compute_folder_ranks(&self) -> Vec<u32> {
        let n = self.records.len();
        let flags = &self.records.flags;
        let parent = &self.records.parent;
        let is_dir = |r: u32| {
            flags
                .get(r as usize)
                .is_some_and(|f| f & DIR_IN_USE == DIR_IN_USE)
        };

        // Directories in name order, then the subdirectories of every directory, still in name
        // order, as one array with offsets
        let dirs = self
            .sorted
            .par_iter()
            .copied()
            .filter(|&id| !is_link(id) && is_dir(id))
            .collect::<Vec<_>>();
        let has_parent = |d: u32| {
            let p = parent[d as usize];
            p != d && is_dir(p)
        };
        // Counts, then where each directory's subdirectories end; filling them in from the back
        // moves that to where they start
        let mut start = vec![0u32; n + 1];
        for &d in &dirs {
            if has_parent(d) {
                start[parent[d as usize] as usize] += 1;
            }
        }
        let mut total = 0;
        for s in start.iter_mut() {
            total += *s;
            *s = total;
        }
        let mut children = vec![0u32; total as usize];
        for &d in dirs.iter().rev() {
            if has_parent(d) {
                let end = &mut start[parent[d as usize] as usize];
                *end -= 1;
                children[*end as usize] = d;
            }
        }

        // Depth first from the root, then from directories that were not reached
        let mut rank = vec![NO_RANK; n];
        let mut counter = 0;
        let mut stack = Vec::new();
        let roots = std::iter::once(ROOT_RECORD).chain(dirs.iter().copied());
        for top in roots.filter(|&r| is_dir(r)) {
            if rank[top as usize] != NO_RANK {
                continue;
            }
            stack.push(top);
            while let Some(d) = stack.pop() {
                if rank[d as usize] != NO_RANK {
                    continue;
                }
                rank[d as usize] = counter;
                counter += 1;
                let kids = &children[start[d as usize] as usize..start[d as usize + 1] as usize];
                stack.extend(kids.iter().rev());
            }
        }
        rank
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;
    use crate::index::sort::cmp_names;
    use crate::index::Records;
    use crate::ntfs::volume::Volume;

    /// A tree of random directories and files whose names share prefixes and differ in case, and
    /// a few directories with broken parent chains
    fn tree(n: usize) -> VolumeIndex {
        const PARTS: &[&str] = &["a", "A", "a b", "a.b", "b", "ab", "a-", "Z", "ä", "a b c"];
        let mut x = 0x9e3779b97f4a7c15_u64;
        let mut next = |max: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % max
        };
        let mut index = VolumeIndex::empty(Volume { id: 'T' });
        index.records = Records::with_len(n);
        let r = &mut index.records;
        r.flags[ROOT_RECORD as usize] = FLAG_IN_USE | FLAG_DIRECTORY;
        r.parent[ROOT_RECORD as usize] = ROOT_RECORD;
        let mut dirs = vec![ROOT_RECORD];
        for i in 16..n {
            let directory = next(3) == 0;
            r.flags[i] = FLAG_IN_USE | if directory { FLAG_DIRECTORY } else { 0 };
            r.parent[i] = if next(100) == 0 {
                // Possibly a file, a free record or a later directory
                next(n as u64) as u32
            } else {
                dirs[next(dirs.len() as u64) as usize]
            };
            let name = PARTS[next(PARTS.len() as u64) as usize];
            r.name_off[i] = index.names.len() as u32;
            r.name_len[i] = name.len() as u16;
            index.names.extend_from_slice(name.as_bytes());
            if directory {
                dirs.push(i as u32);
            }
        }
        index.sort_and_compact();
        index
    }

    /// Folder order the slow way: comparing the folder chains from the root down
    fn chain(index: &VolumeIndex, dir: u32) -> Option<Vec<u32>> {
        let mut chain = vec![dir];
        let mut cur = dir;
        while cur != ROOT_RECORD {
            cur = index.records.parent[cur as usize];
            let live = index.records.flags[cur as usize] & DIR_IN_USE == DIR_IN_USE;
            if !live || chain.contains(&cur) {
                return None;
            }
            chain.push(cur);
        }
        chain.reverse();
        Some(chain)
    }

    #[test]
    fn ranks_follow_the_tree() {
        let index = tree(5_000);
        let ranks = index.folder_ranks();
        let position = |id: u32| index.sorted.iter().position(|&s| s == id).unwrap_or(0);
        let reachable = (0..index.records.len() as u32)
            .filter(|&r| index.records.flags[r as usize] & DIR_IN_USE == DIR_IN_USE)
            .filter_map(|r| chain(&index, r).map(|c| (r, c)))
            .collect::<Vec<_>>();
        // Every reachable directory has a rank, before every unreachable one
        let worst = reachable.iter().map(|(r, _)| ranks[*r as usize]).max();
        assert_eq!(worst, Some(reachable.len() as u32 - 1));

        let cmp = |a: &Vec<u32>, b: &Vec<u32>| {
            for (&x, &y) in a.iter().zip(b) {
                if x != y {
                    // Siblings: by name, equal names in name order
                    return cmp_names(index.name(x), index.name(y))
                        .then(position(x).cmp(&position(y)));
                }
            }
            a.len().cmp(&b.len())
        };
        let mut expected = reachable.clone();
        expected.sort_by(|a, b| cmp(&a.1, &b.1));
        let mut got = reachable;
        got.sort_by_key(|(r, _)| ranks[*r as usize]);
        assert_eq!(
            got.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
            expected.iter().map(|(r, _)| *r).collect::<Vec<_>>()
        );
        assert_eq!(ranks[ROOT_RECORD as usize], 0);
        assert!(matches!(cmp(&vec![5, 1], &vec![5, 1, 2]), Ordering::Less));
    }
}
