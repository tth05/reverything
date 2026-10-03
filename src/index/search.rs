//! Case-insensitive substring search with optional path components.

use memchr::{memchr2_iter, memchr_iter};
use rayon::prelude::*;

use crate::index::{VolumeIndex, FLAG_DIRECTORY, FLAG_IN_USE};
use crate::ntfs::ROOT_RECORD;

/// A parsed search. `foo\bar\baz` finds entries whose name contains `baz`, whose parent's name
/// contains `bar` and whose grandparent's name contains `foo`. A leading `C:` anchors the path at
/// the root of that volume.
#[derive(Debug, Default)]
pub struct Query {
    pub volume: Option<char>,
    pub anchored: bool,
    pub dirs: Vec<Matcher>,
    pub name: Option<Matcher>,
}

impl Query {
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        let ends_with_separator = text.ends_with(['\\', '/']);
        let mut parts = text
            .split(['\\', '/'])
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>();

        let mut query = Query::default();
        if let Some(first) = parts.first() {
            let b = first.as_bytes();
            if b.len() == 2 && b[1] == b':' && b[0].is_ascii_alphabetic() && (parts.len() > 1 || ends_with_separator) {
                query.volume = Some(b[0].to_ascii_uppercase() as char);
                query.anchored = true;
                parts.remove(0);
            }
        }

        if !ends_with_separator {
            query.name = parts.pop().map(Matcher::new);
        }
        query.dirs = parts.into_iter().map(Matcher::new).collect();
        query
    }

    pub fn is_match_all(&self) -> bool {
        self.volume.is_none() && self.dirs.is_empty() && self.name.is_none()
    }
}

#[derive(Debug)]
pub enum Matcher {
    /// Lowercased ASCII needle, matched with ASCII case folding
    Ascii(Vec<u8>),
    /// Lowercased needle containing non-ASCII characters
    Unicode(String),
}

impl Matcher {
    pub fn new(needle: &str) -> Self {
        if needle.is_ascii() {
            Matcher::Ascii(needle.to_ascii_lowercase().into_bytes())
        } else {
            Matcher::Unicode(needle.to_lowercase())
        }
    }

    #[inline]
    pub fn matches(&self, hay: &[u8]) -> bool {
        match self {
            Matcher::Ascii(needle) => contains_ascii_ci(hay, needle),
            Matcher::Unicode(needle) => {
                let hay = String::from_utf8_lossy(hay).to_lowercase();
                hay.contains(needle.as_str())
            }
        }
    }
}

/// ASCII case-insensitive substring search. Candidates are found with SIMD (`memchr2`) on the
/// first needle byte in both cases.
#[inline]
fn contains_ascii_ci(hay: &[u8], needle: &[u8]) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    if hay.len() < n {
        return false;
    }

    let candidates = &hay[..=hay.len() - n];
    let rest = &needle[1..];
    let check = |i: usize| hay[i + 1..i + n].eq_ignore_ascii_case(rest);
    let first = needle[0];
    if first.is_ascii_alphabetic() {
        memchr2_iter(first, first.to_ascii_uppercase(), candidates).any(check)
    } else {
        memchr_iter(first, candidates).any(check)
    }
}

#[inline]
fn bit(set: &[u64], id: u32) -> bool {
    set.get(id as usize / 64)
        .is_some_and(|w| w & (1 << (id % 64)) != 0)
}

impl VolumeIndex {
    /// Matching entries in name order.
    pub fn search(&self, query: &Query) -> Vec<u32> {
        if query
            .volume
            .is_some_and(|v| v != self.volume.id.to_ascii_uppercase())
        {
            return Vec::new();
        }

        let parents = (query.anchored || !query.dirs.is_empty()).then(|| self.match_dirs(query));

        self.sorted
            .par_iter()
            .with_min_len(4096)
            .copied()
            .filter(|&id| {
                parents
                    .as_ref()
                    .is_none_or(|set| bit(set, self.parent(id)))
                    && query.name.as_ref().is_none_or(|m| m.matches(self.name(id)))
            })
            .collect()
    }

    /// Bitset of the directories the last path component of the query may be in.
    fn match_dirs(&self, query: &Query) -> Vec<u64> {
        let n = self.records.len();
        let words = n.div_ceil(64);

        let mut set = query.anchored.then(|| {
            let mut root = vec![0u64; words];
            if let Some(w) = root.get_mut(ROOT_RECORD as usize / 64) {
                *w |= 1 << (ROOT_RECORD % 64);
            }
            root
        });

        for matcher in &query.dirs {
            let prev = set.as_deref();
            let flags = &self.records.flags;
            let parent = &self.records.parent;
            let next = (0..words)
                .into_par_iter()
                .with_min_len(256)
                .map(|w| {
                    let mut bits = 0u64;
                    for b in 0..64 {
                        let id = w * 64 + b;
                        if id >= n {
                            break;
                        }
                        let f = flags[id];
                        if f & FLAG_IN_USE != 0
                            && f & FLAG_DIRECTORY != 0
                            && prev.is_none_or(|p| bit(p, parent[id]))
                            && matcher.matches(self.name(id as u32))
                        {
                            bits |= 1 << b;
                        }
                    }
                    bits
                })
                .collect::<Vec<_>>();
            set = Some(next);
        }

        set.unwrap_or_else(|| vec![u64::MAX; words])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_contains() {
        assert!(contains_ascii_ci(b"Notepad.exe", b"pad.e"));
        assert!(contains_ascii_ci(b"Notepad.exe", b"notepad"));
        assert!(!contains_ascii_ci(b"Notepad.exe", b"notes"));
        assert!(contains_ascii_ci(b"a", b"a"));
        assert!(!contains_ascii_ci(b"", b"a"));
        assert!(contains_ascii_ci(b"x_1", b"_1"));
    }

    #[test]
    fn parse_query() {
        let q = Query::parse(r"c:\windows\notepad");
        assert_eq!(q.volume, Some('C'));
        assert!(q.anchored);
        assert_eq!(q.dirs.len(), 1);
        assert!(q.name.is_some());

        let q = Query::parse(r"system32\");
        assert_eq!(q.dirs.len(), 1);
        assert!(q.name.is_none());

        let q = Query::parse("c:");
        assert!(q.volume.is_none());
        assert!(q.name.is_some());

        assert!(Query::parse("  ").is_match_all());
    }
}
