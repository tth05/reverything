//! Searching all volumes and ordering the combined results.

use std::cmp::Ordering;
use std::sync::RwLockReadGuard;

use rayon::prelude::*;

use crate::index::search::{FolderExclusion, Query};
use crate::index::sort::cmp_names;
use crate::index::VolumeIndex;
use crate::service::IndexSet;

/// A search result: volume slot in the high 32 bits, record number in the low 32 bits.
pub type Hit = u64;

#[inline]
pub fn hit(volume: usize, id: u32) -> Hit {
    (volume as u64) << 32 | id as u64
}

#[inline]
pub fn hit_parts(hit: Hit) -> (usize, u32) {
    ((hit >> 32) as usize, hit as u32)
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum SortColumn {
    #[default]
    Name,
    Path,
    Size,
    Modified,
    Created,
    Attributes,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Sort {
    pub column: SortColumn,
    pub ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            column: SortColumn::Name,
            ascending: true,
        }
    }
}

/// Building full paths for millions of results is too slow and memory hungry, so sorting by
/// path is limited to result sets below this size.
pub const MAX_PATH_SORT: usize = 1_000_000;

/// Per volume bitsets of the directories a query's folder exclusions leave out, for
/// [`search_all`]. Resolving them walks every directory, so callers may cache the result.
pub fn exclusions(set: &IndexSet, folders: &[FolderExclusion]) -> Vec<Option<Vec<u64>>> {
    set.volumes
        .par_iter()
        .map(|v| v.index.read().unwrap().exclusions(folders))
        .collect()
}

/// Searches all volumes. `excluded` holds the resolved folder exclusions of the query per
/// volume, see [`exclusions`].
pub fn search_all(
    set: &IndexSet,
    query: &Query,
    sort: Sort,
    excluded: &[Option<Vec<u64>>],
) -> Vec<Hit> {
    let indices = set
        .volumes
        .iter()
        .map(|v| v.index.read().unwrap())
        .collect::<Vec<_>>();

    let per_volume = indices
        .par_iter()
        .enumerate()
        .map(|(i, index)| {
            let excluded = excluded.get(i).and_then(|e| e.as_deref());
            if query.is_match_all() && excluded.is_none() {
                index.sorted.clone()
            } else {
                index.search(query, excluded)
            }
        })
        .collect::<Vec<_>>();

    let mut hits = merge_by_name(&indices, per_volume);

    match sort.column {
        SortColumn::Name => {}
        SortColumn::Size => hits.par_sort_by_key(|&h| entry(&indices, h, |i, id| i.size(id))),
        SortColumn::Modified => {
            hits.par_sort_by_key(|&h| entry(&indices, h, |i, id| i.modified(id)))
        }
        SortColumn::Created => hits.par_sort_by_key(|&h| entry(&indices, h, |i, id| i.created(id))),
        SortColumn::Attributes => hits.par_sort_by_key(|&h| {
            entry(&indices, h, |i, id| {
                i.flags(id) & crate::index::ATTRIBUTE_MASK
            })
        }),
        SortColumn::Path if hits.len() <= MAX_PATH_SORT => hits.par_sort_by_cached_key(|&h| {
            entry(&indices, h, |i, id| i.folder_path(id).to_lowercase())
        }),
        SortColumn::Path => {}
    }
    if !sort.ascending {
        hits.reverse();
    }
    hits
}

#[inline]
fn entry<T>(
    indices: &[RwLockReadGuard<VolumeIndex>],
    hit: Hit,
    f: impl Fn(&VolumeIndex, u32) -> T,
) -> T {
    let (v, id) = hit_parts(hit);
    f(&indices[v], id)
}

/// Merges the per volume results, which are each in name order, into one list in name order.
fn merge_by_name(indices: &[RwLockReadGuard<VolumeIndex>], per_volume: Vec<Vec<u32>>) -> Vec<Hit> {
    let mut lists = per_volume
        .into_iter()
        .enumerate()
        .filter(|(_, l)| !l.is_empty())
        .collect::<Vec<_>>();
    // Insert the smaller lists into the largest one
    lists.sort_by_key(|(_, l)| std::cmp::Reverse(l.len()));

    let mut iter = lists.into_iter();
    let Some((v0, first)) = iter.next() else {
        return Vec::new();
    };
    let mut merged = first
        .into_par_iter()
        .map(|id| hit(v0, id))
        .collect::<Vec<_>>();

    for (v, list) in iter {
        let name_of = |h: Hit| {
            let (v, id) = hit_parts(h);
            indices[v].name(id)
        };
        let mut out = Vec::with_capacity(merged.len() + list.len());
        let mut start = 0;
        for id in list {
            let name = indices[v].name(id);
            let pos = start
                + merged[start..]
                    .partition_point(|&h| cmp_names(name_of(h), name) != Ordering::Greater);
            out.extend_from_slice(&merged[start..pos]);
            out.push(hit(v, id));
            start = pos;
        }
        out.extend_from_slice(&merged[start..]);
        merged = out;
    }

    merged
}
