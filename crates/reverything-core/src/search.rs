//! Searching all volumes and ordering the combined results.

use std::sync::{Arc, RwLockReadGuard};

use rayon::prelude::*;
use tracing::info_span;

use crate::index::folders::NO_RANK;
use crate::index::rank::{group_by_score, Ranker};
use crate::index::search::{FolderExclusion, Query};
use crate::index::{prefetch, prefetched, VolumeIndex};
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
    let list = |volume, ids| List {
        volume,
        ids,
        groups: Vec::new(),
    };
    let by_key = |per_volume: Vec<(usize, Arc<Vec<u32>>)>, order, key: KeyFn, ahead: PrefetchFn| {
        let lists = per_volume
            .into_iter()
            .map(|(v, ids)| {
                let index = &*indices[v];
                let sorted = sort_by_key(&ids, |id| key(index, id), |id| ahead(index, id));
                list(v, Arc::new(sorted))
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
        SortColumn::Size => by_key(
            per_volume,
            Order::Size,
            |i, id| i.size(id),
            |i, id| prefetch_at(&i.records.size, id),
        ),
        SortColumn::Modified => by_key(
            per_volume,
            Order::Modified,
            |i, id| i.modified(id) as u64,
            |i, id| prefetch_at(&i.records.modified, id),
        ),
        SortColumn::Created => by_key(
            per_volume,
            Order::Created,
            |i, id| i.created(id) as u64,
            |i, id| prefetch_at(&i.records.created, id),
        ),
        SortColumn::Attributes => by_key(
            per_volume,
            Order::Attributes,
            |i, id| (i.flags(id) & crate::index::ATTRIBUTE_MASK) as u64,
            |i, id| prefetch_at(&i.records.flags, id),
        ),
        SortColumn::Path => {
            // Cached until directories change, built for all volumes at once otherwise
            let all_ranks = info_span!("search.folders").in_scope(|| {
                per_volume
                    .par_iter()
                    .map(|(v, _)| indices[*v].folder_ranks())
                    .collect::<Vec<_>>()
            });
            let lists = per_volume
                .into_iter()
                .zip(all_ranks)
                .map(|((v, ids), ranks)| {
                    let index = &*indices[v];
                    let folder = |id: u32| {
                        let rank = ranks.get(index.parent(id) as usize).copied();
                        rank.unwrap_or(NO_RANK) as u64
                    };
                    let parent = |id| prefetch_at(&index.records.parent, id);
                    list(v, Arc::new(sort_by_key(&ids, folder, parent)))
                })
                .collect();
            Results::lazy(Order::Path, descending, lists)
        }
        SortColumn::Name => Results::lazy(
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

/// The sort key of an entry
type KeyFn = fn(&VolumeIndex, u32) -> u64;
/// Prefetches what [`KeyFn`] reads
type PrefetchFn = fn(&VolumeIndex, u32);

/// Prefetches the element of a record array for entry `id`. Links are rare and left out.
fn prefetch_at<T>(array: &[T], id: u32) {
    if let Some(value) = array.get(id as usize) {
        prefetch(value);
    }
}

/// `ids` ordered by `key`, keeping their order for equal keys. `prefetch` asks for the memory
/// `key` reads.
///
/// Every key is read once and packed into a `u64` with the position of its entry, which makes
/// the order stable, so plain integers are sorted instead of looking up keys in every
/// comparison. Keys that do not fit next to the position are cut to their high bits; entries
/// whose cut keys are equal are ordered by their full keys afterwards.
fn sort_by_key(
    ids: &[u32],
    key: impl Fn(u32) -> u64 + Sync,
    prefetch: impl Fn(u32) + Sync + Copy,
) -> Vec<u32> {
    let n = ids.len();
    let pos_bits = (usize::BITS - n.leading_zeros()).max(1);
    let pos_mask = (1u64 << pos_bits) - 1;
    let keys = info_span!("search.sort.keys").entered();
    let mut packed = ids
        .par_chunks(4096)
        .flat_map_iter(|chunk| {
            let mut keys = Vec::with_capacity(chunk.len());
            prefetched(chunk, prefetch, |_| {}, |id| keys.push(key(id)));
            keys.into_iter()
        })
        .collect::<Vec<_>>();
    let max = packed.par_iter().copied().max().unwrap_or(0);
    let shift = (u64::BITS - max.leading_zeros()).saturating_sub(u64::BITS - pos_bits);
    packed
        .par_iter_mut()
        .enumerate()
        .for_each(|(pos, k)| *k = (*k >> shift) << pos_bits | pos as u64);
    drop(keys);
    let sorting = info_span!("search.sort.integers").entered();
    packed.par_sort_unstable();
    drop(sorting);
    if shift > 0 {
        packed
            .par_chunk_by_mut(|a, b| a >> pos_bits == b >> pos_bits)
            .filter(|run| run.len() > 1)
            .for_each(|run| run.sort_by_key(|&p| key(ids[(p & pos_mask) as usize])));
    }
    let _ids = info_span!("search.sort.ids").entered();
    packed
        .par_iter()
        .map(|&p| ids[(p & pos_mask) as usize])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::sort::cmp_names;
    use crate::index::testing::{names, volume};

    /// Same as a stable sort, also with keys too large to pack whole and with many equal keys
    #[test]
    fn key_sort_is_stable() {
        let mut x = 0x2545f4914f6cdd1d_u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let ids = (0..100_000u32).map(|i| i * 7 + 3).collect::<Vec<_>>();
        let sets: [Vec<u64>; 4] = [
            (0..ids.len()).map(|_| next() % 50).collect(),
            (0..ids.len()).map(|_| next() % 1_000_000).collect(),
            // Full 64 bit keys, cut to fit the positions
            (0..ids.len()).map(|_| next()).collect(),
            // Cut keys that are equal but full keys that differ
            (0..ids.len()).map(|_| (1 << 60) + next() % 64).collect(),
        ];
        for keys in sets {
            let key = |id: u32| keys[((id - 3) / 7) as usize];
            let mut expected = ids.clone();
            expected.sort_by_key(|&id| key(id));
            assert_eq!(sort_by_key(&ids, key, |_| {}), expected);
        }
        assert!(sort_by_key(&[], |_| 0, |_| {}).is_empty());
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
