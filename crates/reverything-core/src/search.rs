//! Searching all volumes and ordering the combined results.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::sync::RwLockReadGuard;

use rayon::prelude::*;
use tracing::info_span;

use crate::index::rank::{order_by_score, Ranker};
use crate::index::search::{FolderExclusion, Query};
use crate::index::sort::{cmp_names, sort_key};
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
    /// See [`crate::index::rank`]
    #[default]
    Relevance,
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
            column: SortColumn::Relevance,
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
///
/// Each phase runs in a `tracing` span named `search.*`, so benchmarks and traces can tell
/// where the time goes.
pub fn search_all(
    set: &IndexSet,
    query: &Query,
    sort: Sort,
    excluded: &[Option<Vec<u64>>],
) -> Vec<Hit> {
    let _span = info_span!("search").entered();
    let lock = info_span!("search.lock").entered();
    let indices = set
        .volumes
        .iter()
        .map(|v| v.index.read().unwrap())
        .collect::<Vec<_>>();
    drop(lock);

    let matching = info_span!("search.match").entered();
    let per_volume = indices
        .par_iter()
        .enumerate()
        .map(|(i, index)| {
            let excluded = excluded.get(i).and_then(|e| e.as_deref());
            if query.is_match_all() && excluded.is_none() {
                Cow::Borrowed(index.sorted.as_slice())
            } else {
                Cow::Owned(index.search(query, excluded))
            }
        })
        .collect::<Vec<_>>();
    drop(matching);

    let mut hits = info_span!("search.merge").in_scope(|| {
        let lists = per_volume.iter().map(|l| &**l).collect::<Vec<_>>();
        merge_by_name(&indices, &lists)
    });
    drop(per_volume);

    let sorting = info_span!("search.sort").entered();
    match sort.column {
        SortColumn::Relevance => {
            if let Some(ranker) = Ranker::new(query) {
                let locations = info_span!("search.locations").in_scope(|| {
                    indices
                        .par_iter()
                        .map(|i| i.locations())
                        .collect::<Vec<_>>()
                });
                let score = info_span!("search.score").entered();
                let scores = hits
                    .par_iter()
                    .with_min_len(4096)
                    .map(|&h| {
                        let (v, id) = hit_parts(h);
                        ranker.score(&indices[v], &locations[v], id)
                    })
                    .collect::<Vec<_>>();
                drop(score);
                info_span!("search.reorder").in_scope(|| order_by_score(&mut hits, &scores));
            }
        }
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
    drop(sorting);
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

/// Merges the per volume results, which are each in name order, into one list ordered by name,
/// then volume, then position in the volume's list. `lists` is indexed by volume slot.
///
/// The lists are cut into ranges of about equal size at common split keys, and each range is
/// merged on its own thread straight into its part of the output.
fn merge_by_name<I>(indices: &[I], lists: &[&[u32]]) -> Vec<Hit>
where
    I: Deref<Target = VolumeIndex> + Sync,
{
    let lists = lists
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.is_empty())
        .map(|(v, &l)| (v, l))
        .collect::<Vec<_>>();
    let total = lists.iter().map(|(_, l)| l.len()).sum::<usize>();
    match lists.as_slice() {
        [] => return Vec::new(),
        &[(v, list)] => return list.par_iter().map(|&id| hit(v, id)).collect(),
        _ => {}
    }

    // Split keys taken evenly from the largest list
    let ranges = (total / MIN_MERGE_RANGE).clamp(1, rayon::current_num_threads() * 8);
    let &(big_v, big) = lists.iter().max_by_key(|(_, l)| l.len()).unwrap();
    let mut cuts = vec![vec![0usize; lists.len()]];
    for r in 1..ranges {
        let split = (big_v, big[big.len() * r / ranges]);
        cuts.push(
            lists
                .iter()
                .map(|&(v, list)| {
                    list.partition_point(|&id| cmp_hits(indices, (v, id), split) == Ordering::Less)
                })
                .collect(),
        );
    }
    cuts.push(lists.iter().map(|(_, l)| l.len()).collect());

    let mut out = Vec::<Hit>::with_capacity(total);
    let mut rest = &mut out.spare_capacity_mut()[..total];
    let mut jobs = Vec::with_capacity(ranges);
    for w in cuts.windows(2) {
        let parts = lists
            .iter()
            .enumerate()
            .map(|(i, &(v, list))| (v, &list[w[0][i]..w[1][i]]))
            .collect::<Vec<_>>();
        let len = parts.iter().map(|(_, p)| p.len()).sum::<usize>();
        let (dst, tail) = std::mem::take(&mut rest).split_at_mut(len);
        rest = tail;
        jobs.push((parts, dst));
    }
    jobs.into_par_iter()
        .for_each(|(parts, dst)| merge_into(indices, &parts, dst));
    // SAFETY: every range wrote exactly as many hits as its parts hold, and the ranges cover
    // the whole output
    unsafe { out.set_len(total) };
    out
}

/// Ranges smaller than this are not worth a task of their own
const MIN_MERGE_RANGE: usize = 1 << 16;

/// Order of [`merge_by_name`] for entries of different lists.
#[inline]
fn cmp_hits<I: Deref<Target = VolumeIndex>>(
    indices: &[I],
    (va, a): (usize, u32),
    (vb, b): (usize, u32),
) -> Ordering {
    if va == vb {
        return indices[va].cmp_entries(a, b);
    }
    cmp_names(indices[va].name(a), indices[vb].name(b)).then(va.cmp(&vb))
}

/// The next entry of a list in [`merge_into`], with its name and sort key at hand.
struct Head<'a> {
    key: u64,
    name: &'a [u8],
    volume: usize,
    list: &'a [u32],
}

impl<'a> Head<'a> {
    fn new(index: &'a VolumeIndex, volume: usize, list: &'a [u32]) -> Self {
        let name = index.name(list[0]);
        Self {
            key: sort_key(name),
            name,
            volume,
            list,
        }
    }

    /// Whether this entry comes before `other`'s, which is in another list.
    #[inline]
    fn before(&self, other: &Head) -> bool {
        self.key
            .cmp(&other.key)
            .then_with(|| cmp_names(self.name, other.name))
            .then(self.volume.cmp(&other.volume))
            == Ordering::Less
    }
}

/// k-way merge of `parts` into `dst`, which has room for exactly all of them.
fn merge_into<I: Deref<Target = VolumeIndex>>(
    indices: &[I],
    parts: &[(usize, &[u32])],
    dst: &mut [MaybeUninit<Hit>],
) {
    let mut heads = parts
        .iter()
        .filter(|(_, p)| !p.is_empty())
        .map(|&(v, p)| Head::new(&indices[v], v, p))
        .collect::<Vec<_>>();
    let mut out = 0;
    while heads.len() > 1 {
        // The smallest head, and the runner-up it is emitted against
        let (mut first, mut second) = if heads[1].before(&heads[0]) {
            (1, 0)
        } else {
            (0, 1)
        };
        for h in 2..heads.len() {
            if heads[h].before(&heads[first]) {
                (first, second) = (h, first);
            } else if heads[h].before(&heads[second]) {
                second = h;
            }
        }
        let (head, limit) = if first < second {
            let (a, b) = heads.split_at_mut(second);
            (&mut a[first], &b[0])
        } else {
            let (a, b) = heads.split_at_mut(first);
            (&mut b[0], &a[second])
        };
        // Emit from the smallest list while it stays ahead of the runner-up
        let index = &indices[head.volume];
        loop {
            dst[out].write(hit(head.volume, head.list[0]));
            out += 1;
            head.list = &head.list[1..];
            let Some(&id) = head.list.first() else {
                break;
            };
            head.name = index.name(id);
            head.key = sort_key(head.name);
            if !head.before(limit) {
                break;
            }
        }
        if head.list.is_empty() {
            heads.swap_remove(first);
        }
    }
    if let Some(last) = heads.first() {
        for (d, &id) in dst[out..].iter_mut().zip(last.list) {
            d.write(hit(last.volume, id));
        }
        out += last.list.len();
    }
    // set_len in merge_by_name relies on this
    assert_eq!(out, dst.len());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Records, FLAG_DIRECTORY, FLAG_IN_USE};
    use crate::ntfs::volume::Volume;
    use crate::ntfs::ROOT_RECORD;

    /// A volume with the given names directly below the root, record numbers from 16 up.
    fn volume(letter: char, names: &[String]) -> VolumeIndex {
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
    /// within and across volumes
    fn names(seed: u64, n: usize) -> Vec<String> {
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

    /// The order merge_by_name promises, by sorting everything
    fn reference(indices: &[&VolumeIndex], lists: &[&[u32]]) -> Vec<Hit> {
        let mut all = lists
            .iter()
            .enumerate()
            .flat_map(|(v, l)| l.iter().map(move |&id| (v, id)))
            .collect::<Vec<_>>();
        all.sort_by(|&a, &b| cmp_hits(indices, a, b));
        all.into_iter().map(|(v, id)| hit(v, id)).collect()
    }

    #[test]
    fn merge_matches_sorting() {
        let volumes = [
            volume('A', &names(1, 70_000)),
            volume('B', &names(2, 130_000)),
            volume('C', &[]),
            volume('D', &names(3, 900)),
            volume('E', &names(4, 1)),
        ];
        let indices = volumes.iter().collect::<Vec<_>>();
        let all = indices
            .iter()
            .map(|i| i.sorted.as_slice())
            .collect::<Vec<_>>();
        assert_eq!(merge_by_name(&indices, &all), reference(&indices, &all));

        // Subsets, as a search returns them
        let every = |k: usize| {
            indices
                .iter()
                .map(|i| i.sorted.iter().copied().step_by(k).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        };
        for k in [2, 7, 1000] {
            let owned = every(k);
            let lists = owned.iter().map(Vec::as_slice).collect::<Vec<_>>();
            assert_eq!(merge_by_name(&indices, &lists), reference(&indices, &lists));
        }

        // A single list and no lists
        let only = [&[][..], &indices[1].sorted[..]];
        assert_eq!(merge_by_name(&indices, &only), reference(&indices, &only));
        assert!(merge_by_name(&indices, &[&[][..]; 3]).is_empty());
    }
}
