//! Search results, put in their combined order only for the rows that are read.
//!
//! A search keeps the hits of every volume in their final order within that volume. Across
//! volumes, hits are ordered by the sort key, then the name, then the volume. That order is only
//! worked out for the rows a client reads: finding where a page starts takes a few binary
//! searches per volume, so millions of hits never have to be merged into one list.
//!
//! Names and metadata are read from the index when a page is read, so they can change between
//! the search and reading its rows. Pages are still cut consistently (every position maps to
//! exactly one hit); only the order of the changed entries may be off.

use std::cmp::Ordering;
use std::ops::Deref;
use std::sync::Arc;

use tracing::info_span;

use crate::index::sort::cmp_names;
use crate::index::{is_link, link_index, VolumeIndex};
use crate::search::{hit, Hit};

/// What the hits of different volumes are compared by before their names.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum Order {
    Name,
    /// Relevance score from [`List::groups`], best first
    Score,
    Size,
    Modified,
    Created,
    Attributes,
    /// Folder order within a volume, see [`crate::index::folders`]; across volumes by volume
    Path,
}

/// The hits of one volume, in their final order.
pub(crate) struct List {
    pub volume: usize,
    pub ids: Arc<Vec<u32>>,
    /// For [`Order::Score`]: the end of every run of hits with the same score, and the score,
    /// best first
    pub groups: Vec<(usize, u8)>,
}

pub struct Results {
    order: Order,
    descending: bool,
    total: usize,
    lists: Vec<List>,
}

impl Default for Results {
    fn default() -> Self {
        Self {
            order: Order::Name,
            descending: false,
            total: 0,
            lists: Vec::new(),
        }
    }
}

impl Results {
    pub(crate) fn lazy(order: Order, descending: bool, mut lists: Vec<List>) -> Self {
        lists.retain(|l| !l.ids.is_empty());
        Self {
            order,
            descending,
            total: lists.iter().map(|l| l.ids.len()).sum(),
            lists,
        }
    }

    pub fn len(&self) -> usize {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// The hits at positions `start..start + count`, fewer at the end. `indices` are the
    /// current indices of all volume slots.
    pub fn page<I: Deref<Target = VolumeIndex>>(
        &self,
        indices: &[I],
        start: usize,
        count: usize,
    ) -> Vec<Hit> {
        let _span = info_span!("search.page").entered();
        let start = start.min(self.total);
        let end = start.saturating_add(count).min(self.total);
        if self.descending {
            let mut hits = self.ascending(indices, self.total - end, self.total - start);
            hits.reverse();
            hits
        } else {
            self.ascending(indices, start, end)
        }
    }

    /// Hits `start..end` in ascending order.
    fn ascending<I: Deref<Target = VolumeIndex>>(
        &self,
        indices: &[I],
        start: usize,
        end: usize,
    ) -> Vec<Hit> {
        if start == end {
            return Vec::new();
        }
        let mut next = self.select(indices, start);
        let mut out = Vec::with_capacity(end - start);
        for _ in start..end {
            let l = self.smallest(indices, &next, |l| self.lists[l].ids.len());
            out.push(hit(self.lists[l].volume, self.lists[l].ids[next[l]]));
            next[l] += 1;
        }
        out
    }

    /// The list whose next hit (at `next`) comes first, among those with hits left before `end`.
    fn smallest<I: Deref<Target = VolumeIndex>>(
        &self,
        indices: &[I],
        next: &[usize],
        end: impl Fn(usize) -> usize,
    ) -> usize {
        (0..self.lists.len())
            .filter(|&l| next[l] < end(l))
            .min_by(|&a, &b| self.cmp(indices, (a, next[a]), (b, next[b])))
            .expect("No hits left")
    }

    /// How many hits of each list come before position `p` of the combined order.
    ///
    /// Narrows a lower and an upper bound per list: the middle of the widest range is the pivot,
    /// and counting the hits before it in every list moves one of the bounds. The bounds only
    /// ever narrow and stay on both sides of `p`, so the result adds up to `p` even if names
    /// changed since the search and the comparisons are not consistent.
    fn select<I: Deref<Target = VolumeIndex>>(&self, indices: &[I], p: usize) -> Vec<usize> {
        let n = self.lists.len();
        let mut lo = vec![0; n];
        let mut hi = self.lists.iter().map(|l| l.ids.len()).collect::<Vec<_>>();
        loop {
            let below = lo.iter().sum::<usize>();
            if below == p {
                return lo;
            }
            if hi.iter().sum::<usize>() == p {
                return hi;
            }
            let (l, width) = (0..n)
                .map(|l| (l, hi[l] - lo[l]))
                .max_by_key(|&(_, w)| w)
                .unwrap();
            if width <= 4 {
                break;
            }
            let pivot = (l, (lo[l] + hi[l]) / 2);
            let less = (0..n)
                .map(|u| {
                    if u == l {
                        pivot.1
                    } else {
                        self.count_less(indices, u, lo[u], hi[u], pivot)
                    }
                })
                .collect::<Vec<_>>();
            if less.iter().sum::<usize>() <= p {
                lo = less;
            } else {
                hi = less;
            }
        }
        // The last few one by one
        let mut below = lo.iter().sum::<usize>();
        while below < p {
            let l = self.smallest(indices, &lo, |l| hi[l]);
            lo[l] += 1;
            below += 1;
        }
        lo
    }

    /// The position in list `l` within `lo..hi` before which all hits come before `pivot`.
    fn count_less<I: Deref<Target = VolumeIndex>>(
        &self,
        indices: &[I],
        l: usize,
        mut lo: usize,
        mut hi: usize,
        pivot: (usize, usize),
    ) -> usize {
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.cmp(indices, (l, mid), pivot) == Ordering::Less {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Compares the hits at a position of a list each.
    fn cmp<I: Deref<Target = VolumeIndex>>(
        &self,
        indices: &[I],
        (la, pa): (usize, usize),
        (lb, pb): (usize, usize),
    ) -> Ordering {
        if la == lb {
            return pa.cmp(&pb);
        }
        let (a, b) = (&self.lists[la], &self.lists[lb]);
        let (ia, ib) = (&*indices[a.volume], &*indices[b.volume]);
        let (ida, idb) = (a.ids[pa], b.ids[pb]);
        self.key(ia, a, pa, ida)
            .cmp(&self.key(ib, b, pb, idb))
            .then_with(|| cmp_names(ia.name_checked(ida), ib.name_checked(idb)))
            .then(a.volume.cmp(&b.volume))
    }

    /// The sort key of a hit, smaller first. Entries the index no longer has count as 0.
    fn key(&self, index: &VolumeIndex, list: &List, pos: usize, id: u32) -> u64 {
        let record = || {
            let r = if is_link(id) {
                *index.links.record.get(link_index(id))?
            } else {
                id
            };
            ((r as usize) < index.records.len()).then_some(r as usize)
        };
        let r = &index.records;
        match self.order {
            Order::Name => 0,
            Order::Score => {
                let group = list.groups.partition_point(|&(end, _)| end <= pos);
                255 - list.groups.get(group).map_or(0, |&(_, score)| score) as u64
            }
            Order::Size => record().map_or(0, |i| r.size[i]),
            Order::Modified => record().map_or(0, |i| r.modified[i] as u64),
            Order::Created => record().map_or(0, |i| r.created[i] as u64),
            // The lists are in folder order, only volumes need comparing
            Order::Path => list.volume as u64,
            Order::Attributes => {
                record().map_or(0, |i| (r.flags[i] & crate::index::ATTRIBUTE_MASK) as u64)
            }
        }
    }
}
