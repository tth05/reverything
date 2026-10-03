//! Folder sizes: the size of a directory is the total size of all files below it. Hard linked
//! files count in every directory they appear in, like Explorer does.

use crate::index::{VolumeIndex, FLAG_DIRECTORY, FLAG_IN_USE, MAX_DEPTH, NO_RECORD};
use crate::ntfs::ROOT_RECORD;

const DIR_IN_USE: u32 = FLAG_IN_USE | FLAG_DIRECTORY;

impl VolumeIndex {
    #[inline]
    fn is_live_dir(&self, record: u32) -> bool {
        self.records
            .flags
            .get(record as usize)
            .is_some_and(|f| f & DIR_IN_USE == DIR_IN_USE)
    }

    /// Computes all folder sizes from scratch in O(n).
    pub fn compute_folder_sizes(&mut self) {
        let n = self.records.len();
        let flags = &self.records.flags;
        let parent = &self.records.parent;
        let size = &mut self.records.size;

        for i in 0..n {
            if flags[i] & DIR_IN_USE == DIR_IN_USE {
                size[i] = 0;
            }
        }

        // Direct contributions of files and their hard links
        let is_dir = |r: u32| flags.get(r as usize).is_some_and(|f| f & DIR_IN_USE == DIR_IN_USE);
        for i in 0..n {
            if flags[i] & DIR_IN_USE == FLAG_IN_USE && is_dir(parent[i]) {
                size[parent[i] as usize] += size[i];
            }
        }
        for (l, &r) in self.links.record.iter().enumerate() {
            let p = self.links.parent[l];
            if r != NO_RECORD && flags[r as usize] & DIR_IN_USE == FLAG_IN_USE && is_dir(p) {
                size[p as usize] += size[r as usize];
            }
        }

        // Depth of every directory, then add each directory to its parent, deepest first
        let mut depth = vec![u16::MAX; n];
        let mut stack = Vec::new();
        for i in 0..n {
            if flags[i] & DIR_IN_USE != DIR_IN_USE || depth[i] != u16::MAX {
                continue;
            }
            let mut cur = i;
            let base = loop {
                if cur == ROOT_RECORD as usize || !is_dir(parent[cur]) || stack.len() > MAX_DEPTH {
                    break 0;
                }
                if depth[cur] != u16::MAX {
                    break depth[cur];
                }
                stack.push(cur);
                cur = parent[cur] as usize;
            };
            if depth[cur] == u16::MAX {
                depth[cur] = base;
            }
            let mut d = depth[cur];
            while let Some(s) = stack.pop() {
                d = d.saturating_add(1);
                depth[s] = d;
            }
        }

        let max_depth = depth.iter().filter(|&&d| d != u16::MAX).max().copied().unwrap_or(0);
        let mut by_depth = vec![Vec::new(); max_depth as usize + 1];
        for (i, &d) in depth.iter().enumerate() {
            if d != u16::MAX && d > 0 {
                by_depth[d as usize].push(i as u32);
            }
        }
        for dirs in by_depth.iter().rev() {
            for &d in dirs {
                let p = parent[d as usize];
                if p as usize != d as usize && is_dir(p) {
                    size[p as usize] += size[d as usize];
                }
            }
        }
    }

    /// Adds `delta` to the sizes of all directories above `record`, through each of its names.
    pub fn add_to_ancestors(&mut self, record: u32, delta: i64) {
        if delta == 0 {
            return;
        }
        for id in self.entries_of(record) {
            let mut cur = self.parent(id);
            for _ in 0..MAX_DEPTH {
                if !self.is_live_dir(cur) || cur == record {
                    break;
                }
                let s = &mut self.records.size[cur as usize];
                *s = s.saturating_add_signed(delta);
                if cur == ROOT_RECORD {
                    break;
                }
                cur = self.records.parent[cur as usize];
            }
        }
    }
}
