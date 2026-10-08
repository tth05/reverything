//! Searching all volumes and ordering the combined results.

use std::cmp::Ordering;
use std::mem::MaybeUninit;
use std::ops::Deref;
use std::sync::{Arc, RwLockReadGuard};

use rayon::prelude::*;
use tracing::info_span;

use crate::index::rank::{group_by_score, Ranker};
use crate::index::search::{FolderExclusion, Query};
use crate::index::sort::{cmp_names, sort_key};
use crate::index::{prefetched, VolumeIndex};
use crate::results::{List, Order, Results};
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

/// Read access to the indices of all volume slots, e.g. for [`Results::page`].
pub fn read_all(set: &IndexSet) -> Vec<RwLockReadGuard<'_, VolumeIndex>> {
    set.volumes
        .iter()
        .map(|v| v.index.read().unwrap())
        .collect()
}

/// Searches all volumes. `excluded` holds the resolved folder exclusions of the query per
/// volume, see [`exclusions`].
///
/// The hits of every volume are put in their final order within the volume; the combined order
/// is only worked out for the rows that are read, see [`Results`]. Each phase runs in a
/// `tracing` span named `search.*`, so benchmarks and traces can tell where the time goes.
pub fn search_all(
    set: &IndexSet,
    query: &Query,
    sort: Sort,
    excluded: &[Option<Vec<u64>>],
) -> Results {
    search_all_cancellable(set, query, sort, excluded, &|| false).expect("Not cancelled")
}

/// [`search_all`] that gives up once `cancelled` returns true, `None` then. Matching and
/// scoring ask it once per chunk of entries, so it should be cheap, like an atomic load.
pub fn search_all_cancellable(
    set: &IndexSet,
    query: &Query,
    sort: Sort,
    excluded: &[Option<Vec<u64>>],
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Option<Results> {
    let _span = info_span!("search").entered();
    let indices = info_span!("search.lock").in_scope(|| read_all(set));

    let matching = info_span!("search.match").entered();
    let per_volume = indices
        .par_iter()
        .enumerate()
        .map(|(i, index)| {
            let excluded = excluded.get(i).and_then(|e| e.as_deref());
            if query.is_match_all() && excluded.is_none() {
                index.sorted.clone()
            } else {
                Arc::new(index.search_cancellable(query, excluded, cancelled))
            }
        })
        .enumerate()
        .filter(|(_, ids)| !ids.is_empty())
        .collect::<Vec<_>>();
    drop(matching);
    if cancelled() {
        return None;
    }

    let descending = !sort.ascending;
    let total = per_volume.iter().map(|(_, ids)| ids.len()).sum::<usize>();
    let list = |volume, ids| List {
        volume,
        ids,
        groups: Vec::new(),
    };
    let by_key =
        |per_volume: Vec<(usize, Arc<Vec<u32>>)>, order, key: fn(&VolumeIndex, u32) -> u64| {
            let lists = per_volume
                .into_iter()
                .map(|(v, ids)| {
                    let mut ids = Arc::unwrap_or_clone(ids);
                    // Stable, so equal keys stay in name order
                    ids.par_sort_by_key(|&id| key(&indices[v], id));
                    list(v, Arc::new(ids))
                })
                .collect();
            Results::lazy(order, descending, lists)
        };
    let _sorting = info_span!("search.sort").entered();
    let results = match sort.column {
        SortColumn::Relevance => match Ranker::new(query) {
            Some(ranker) => {
                let lists = per_volume
                    .into_iter()
                    .map(|(v, ids)| {
                        let index = &*indices[v];
                        let locations =
                            info_span!("search.locations").in_scope(|| index.locations());
                        let scores = info_span!("search.score").in_scope(|| {
                            ids.par_chunks(4096)
                                .flat_map_iter(|chunk| {
                                    let mut scores = Vec::with_capacity(chunk.len());
                                    if cancelled() {
                                        return scores.into_iter();
                                    }
                                    prefetched(
                                        chunk,
                                        |id| ranker.prefetch_record(index, id),
                                        |id| ranker.prefetch_name(index, &locations, id),
                                        |id| scores.push(ranker.score(index, &locations, id)),
                                    );
                                    scores.into_iter()
                                })
                                .collect::<Vec<_>>()
                        });
                        // Grouping millions of hits takes milliseconds
                        if cancelled() {
                            return List {
                                volume: v,
                                ids: Arc::default(),
                                groups: Vec::new(),
                            };
                        }
                        let (ids, groups) =
                            info_span!("search.group").in_scope(|| group_by_score(&ids, &scores));
                        List {
                            volume: v,
                            ids: Arc::new(ids),
                            groups,
                        }
                    })
                    .collect();
                Results::lazy(Order::Score, descending, lists)
            }
            None => Results::lazy(
                Order::Name,
                descending,
                per_volume
                    .into_iter()
                    .map(|(v, ids)| list(v, ids))
                    .collect(),
            ),
        },
        SortColumn::Size => by_key(per_volume, Order::Size, |i, id| i.size(id)),
        SortColumn::Modified => by_key(per_volume, Order::Modified, |i, id| i.modified(id) as u64),
        SortColumn::Created => by_key(per_volume, Order::Created, |i, id| i.created(id) as u64),
        SortColumn::Attributes => by_key(per_volume, Order::Attributes, |i, id| {
            (i.flags(id) & crate::index::ATTRIBUTE_MASK) as u64
        }),
        SortColumn::Path if total <= MAX_PATH_SORT => {
            let mut lists = vec![&[][..]; indices.len()];
            for (v, ids) in &per_volume {
                lists[*v] = ids.as_slice();
            }
            let mut hits = info_span!("search.merge").in_scope(|| merge_by_name(&indices, &lists));
            hits.par_sort_by_cached_key(|&h| {
                let (v, id) = hit_parts(h);
                indices[v].folder_path(id).to_lowercase()
            });
            Results::flat(hits, descending)
        }
        SortColumn::Name | SortColumn::Path => Results::lazy(
            Order::Name,
            descending,
            per_volume
                .into_iter()
                .map(|(v, ids)| list(v, ids))
                .collect(),
        ),
    };
    (!cancelled()).then_some(results)
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
    use crate::index::testing::{names, volume};

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

    /// Checks pages at many positions, in both directions, against sorting everything by
    /// `key`, then name, then volume, then position in the volume's list.
    fn check_pages(
        indices: &[&VolumeIndex],
        lists: Vec<List>,
        order: Order,
        key: impl Fn(usize, usize, u32) -> u64,
    ) {
        let mut all = lists
            .iter()
            .enumerate()
            .flat_map(|(l, list)| {
                list.ids
                    .iter()
                    .enumerate()
                    .map(move |(pos, &id)| (l, list.volume, pos, id))
            })
            .collect::<Vec<_>>();
        all.sort_by(|&(la, va, pa, a), &(lb, vb, pb, b)| {
            key(la, pa, a)
                .cmp(&key(lb, pb, b))
                .then_with(|| cmp_names(indices[va].name(a), indices[vb].name(b)))
                .then(va.cmp(&vb))
                .then(pa.cmp(&pb))
        });
        let expected = all
            .iter()
            .map(|&(_, v, _, id)| hit(v, id))
            .collect::<Vec<_>>();
        let reversed = expected.iter().rev().copied().collect::<Vec<_>>();
        let total = expected.len();

        let shared = lists
            .iter()
            .map(|l| List {
                volume: l.volume,
                ids: l.ids.clone(),
                groups: l.groups.clone(),
            })
            .collect();
        let ascending = Results::lazy(order, false, shared);
        let descending = Results::lazy(order, true, lists);
        assert_eq!(ascending.len(), total);
        let mut starts = vec![
            0,
            1,
            255,
            256,
            4095,
            total / 3,
            total / 2,
            total.saturating_sub(1),
            total,
            total + 5,
        ];
        starts.extend((1..40).map(|i| total * i / 41 + i));
        for start in starts {
            for count in [1, 256, 2048] {
                let end = (start + count).min(total);
                let range = start.min(total)..end;
                assert_eq!(
                    ascending.page(indices, start, count),
                    expected[range.clone()],
                    "{:?} at {}",
                    order,
                    start
                );
                assert_eq!(
                    descending.page(indices, start, count),
                    reversed[range],
                    "{:?} at {} descending",
                    order,
                    start
                );
            }
        }
    }

    #[test]
    fn pages_in_combined_order() {
        let mut volumes = [
            volume('A', &names(5, 60_000)),
            volume('B', &names(6, 25_000)),
            volume('C', &names(7, 3)),
        ];
        for (v, index) in volumes.iter_mut().enumerate() {
            for (i, size) in index.records.size.iter_mut().enumerate() {
                *size = (i as u64 * 31 + v as u64) % 50;
            }
        }
        let indices = volumes.iter().collect::<Vec<_>>();
        let sorted = |v: usize, step: usize| -> Vec<u32> {
            indices[v].sorted.iter().copied().step_by(step).collect()
        };

        // Name order, with a volume without hits in between
        let name_lists = || {
            vec![
                List {
                    volume: 0,
                    ids: Arc::new(sorted(0, 1)),
                    groups: Vec::new(),
                },
                List {
                    volume: 2,
                    ids: Arc::new(sorted(2, 1)),
                    groups: Vec::new(),
                },
                List {
                    volume: 1,
                    ids: Arc::new(sorted(1, 3)),
                    groups: Vec::new(),
                },
            ]
        };
        check_pages(&indices, name_lists(), Order::Name, |_, _, _| 0);

        // Grouped by score
        let score = |v: usize, id: u32| ((id as usize * 7 + v) % 5) as u8;
        let mut lists = Vec::new();
        for (v, step) in [(0, 2), (1, 1), (2, 1)] {
            let ids = sorted(v, step);
            let scores = ids.iter().map(|&id| score(v, id)).collect::<Vec<_>>();
            let (ids, groups) = group_by_score(&ids, &scores);
            lists.push(List {
                volume: v,
                ids: Arc::new(ids),
                groups,
            });
        }
        let volume_of = lists.iter().map(|l| l.volume).collect::<Vec<_>>();
        check_pages(&indices, lists, Order::Score, |l, _, id| {
            255 - score(volume_of[l], id) as u64
        });

        // By size, stable within a volume
        let mut lists = name_lists();
        for list in &mut lists {
            let index = indices[list.volume];
            Arc::make_mut(&mut list.ids).sort_by_key(|&id| index.size(id));
        }
        let volume_of = lists.iter().map(|l| l.volume).collect::<Vec<_>>();
        check_pages(&indices, lists, Order::Size, |l, _, id| {
            indices[volume_of[l]].size(id)
        });

        assert!(Results::default().page(&indices, 0, 256).is_empty());
    }
}
