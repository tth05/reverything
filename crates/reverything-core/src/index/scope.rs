//! Which folders a query searches: `+folder\` and `!folder\`, see [`crate::index::search`].

use fixedbitset::FixedBitSet;
use rayon::prelude::*;
use tracing::info_span;

use crate::index::search::FolderScope;
use crate::index::{VolumeIndex, FLAG_DIRECTORY, FLAG_IN_USE, MAX_DEPTH};
use crate::ntfs::ROOT_RECORD;

const DIR_IN_USE: u32 = FLAG_IN_USE | FLAG_DIRECTORY;

/// The entries of a volume a query looks at, by the directory they are in.
#[derive(Debug, Clone, Default)]
pub enum Scope {
    #[default]
    All,
    Nothing,
    /// Everything except what is directly in these directories, by record number
    Excluding(FixedBitSet),
}

impl VolumeIndex {
    /// Resolves the folder scopes of a query on this volume. Walks every directory, so callers
    /// may cache the result.
    pub fn scope(&self, folders: &[FolderScope]) -> Scope {
        let mut included = Vec::new();
        let mut excluded = Vec::new();
        for scope in folders {
            let roots = if scope.include {
                &mut included
            } else {
                &mut excluded
            };
            let folder = &scope.folder;
            if !self.on_this_volume(folder) {
                continue;
            }
            match &folder.name {
                // `C:\`
                None => roots.push(ROOT_RECORD),
                Some(_) => {
                    let prepared = self.prepare(folder);
                    let flags = &self.records.flags;
                    roots.par_extend(
                        (0..flags.len() as u32)
                            .into_par_iter()
                            .filter(|&id| flags[id as usize] & DIR_IN_USE == DIR_IN_USE)
                            .filter(|&id| prepared.matches(self, id)),
                    );
                }
            }
        }
        let searches_some = folders.iter().any(|f| f.include);
        if searches_some && included.is_empty() {
            Scope::Nothing
        } else if included.is_empty() && excluded.is_empty() {
            Scope::All
        } else {
            let _span = info_span!("search.scope.walk").entered();
            let included = FixedBitSet::from_iter(included.into_iter().map(|d| d as usize));
            let excluded = FixedBitSet::from_iter(excluded.into_iter().map(|d| d as usize));
            Scope::Excluding(self.closest(
                |d| {
                    if excluded.contains(d) {
                        Some(true)
                    } else {
                        included.contains(d).then_some(false)
                    }
                },
                searches_some,
            ))
        }
    }

    /// The directories at or below the given ones, by record number.
    pub(crate) fn below(&self, dirs: &FixedBitSet) -> FixedBitSet {
        self.closest(|d| dirs.contains(d).then_some(true), false)
    }

    /// The directories for which the closest marked directory at or above them is marked
    /// `true`, or with none, `default` is, by record number.
    fn closest(&self, mark: impl Fn(usize) -> Option<bool>, default: bool) -> FixedBitSet {
        const UNKNOWN: u8 = 0;
        const NO: u8 = 1;
        const YES: u8 = 2;
        let as_state = |yes: bool| if yes { YES } else { NO };
        let default = as_state(default);

        let n = self.records.len();
        let flags = &self.records.flags;
        let parent = &self.records.parent;
        let mut state = vec![UNKNOWN; n];
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
                chain.push(cur);
                if let Some(yes) = mark(cur) {
                    break as_state(yes);
                }
                if chain.len() > MAX_DEPTH {
                    break default;
                }
                let p = parent[cur] as usize;
                if cur == ROOT_RECORD as usize
                    || p >= n
                    || p == cur
                    || flags[p] & DIR_IN_USE != DIR_IN_USE
                {
                    break default;
                }
                cur = p;
            };
            for c in chain.drain(..) {
                state[c] = result;
            }
        }

        let mut dirs = FixedBitSet::with_capacity(n);
        dirs.extend((0..n).filter(|&i| state[i] == YES));
        dirs
    }
}
