//! Relevance of search results, the default order.
//!
//! Every hit gets a one byte score, compared in this order:
//!
//! 1. How the name matches: exactly, exactly without the extension, at the start, at the start
//!    of a word, anywhere
//! 2. Where it is: in a user folder (`C:\Users\<name>`), anywhere else, or in places full of
//!    files nobody looks for (Windows, Program Files, ProgramData, AppData, WinSxS,
//!    node_modules, target, the Recycle Bin and folders starting with a dot)
//! 3. Whether the name contains the terms with the same upper and lower case as typed
//! 4. How recently it was modified
//!
//! Hits with the same score keep their name order.

use std::sync::Arc;

use memchr::memmem;

use crate::index::search::{Matcher, Query};
use crate::index::{VolumeIndex, FLAG_DIRECTORY, FLAG_IN_USE, MAX_DEPTH};
use crate::ntfs::ROOT_RECORD;

const DIR_IN_USE: u32 = FLAG_IN_USE | FLAG_DIRECTORY;

/// Location of a directory and everything below it
const LOW: u8 = 0;
const NORMAL: u8 = 1;
const USER: u8 = 2;
/// `C:\Users` itself, normal, but the folders in it are user folders
const USERS_ROOT: u8 = 3;
const UNKNOWN: u8 = u8::MAX;

/// Folders anywhere on the volume whose content ranks lower
const LOW_ANYWHERE: [&str; 5] = [
    "AppData",
    "WinSxS",
    "node_modules",
    "target",
    "$Recycle.Bin",
];
/// Folders in the root of the volume whose content ranks lower
const LOW_AT_ROOT: [&str; 4] = [
    "Windows",
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
];

const DAY: u32 = 24 * 60 * 60;

/// An updated directory, with its parent and name before the update if it existed
pub(crate) type DirectoryChange = (u32, Option<(u32, Vec<u8>)>);

/// Location of a directory, from its name and the location of its parent.
fn dir_location(name: &[u8], parent: u8, parent_is_root: bool) -> u8 {
    let is = |names: &[&str]| {
        names
            .iter()
            .any(|n| name.eq_ignore_ascii_case(n.as_bytes()))
    };
    if parent == LOW
        || name.first() == Some(&b'.')
        || is(&LOW_ANYWHERE)
        || (parent_is_root && is(&LOW_AT_ROOT))
    {
        LOW
    } else if parent_is_root && name.eq_ignore_ascii_case(b"Users") {
        USERS_ROOT
    } else if parent == USER || parent == USERS_ROOT {
        USER
    } else {
        NORMAL
    }
}

impl VolumeIndex {
    /// Location of every directory, indexed by record number. Cached until directories change.
    pub fn locations(&self) -> Arc<Vec<u8>> {
        let mut cache = self.locations.lock().unwrap();
        cache
            .get_or_insert_with(|| Arc::new(self.compute_locations()))
            .clone()
    }

    pub(crate) fn is_directory(&self, record: u32) -> bool {
        self.records
            .flags
            .get(record as usize)
            .is_some_and(|f| f & DIR_IN_USE == DIR_IN_USE)
    }

    /// Keeps the cached locations up to date after `changes`, the updated directories (in
    /// parents first order) with their parent and name before the update. New directories get
    /// their location from their parent; a renamed or moved one can change everything below it,
    /// so the cache is dropped then.
    pub(crate) fn update_locations(&mut self, changes: &[DirectoryChange]) {
        let Some(mut cached) = self.locations.get_mut().unwrap().take() else {
            return;
        };
        // No search holds it while the index is being updated
        let Some(locations) = Arc::get_mut(&mut cached) else {
            return;
        };
        for (record, before) in changes {
            let r = *record as usize;
            if !self.is_directory(*record) {
                // Deleted, nothing refers to it anymore
                continue;
            }
            let parent = self.records.parent[r];
            match before {
                Some((old_parent, old_name)) => {
                    if *old_parent != parent || old_name.as_slice() != self.name(*record) {
                        return;
                    }
                }
                None => {
                    if locations.len() <= r {
                        locations.resize(self.records.len(), UNKNOWN);
                    }
                    let parent_location = match locations.get(parent as usize) {
                        Some(&l) if l != UNKNOWN => l,
                        _ => NORMAL,
                    };
                    locations[r] =
                        dir_location(self.name(*record), parent_location, parent == ROOT_RECORD);
                }
            }
        }
        *self.locations.get_mut().unwrap() = Some(cached);
    }

    fn compute_locations(&self) -> Vec<u8> {
        let n = self.records.len();
        let flags = &self.records.flags;
        let parent = &self.records.parent;
        let mut location = vec![UNKNOWN; n];
        if let Some(root) = location.get_mut(ROOT_RECORD as usize) {
            *root = NORMAL;
        }

        let mut chain = Vec::new();
        for i in 0..n {
            if flags[i] & DIR_IN_USE != DIR_IN_USE || location[i] != UNKNOWN {
                continue;
            }
            // Walk up to a directory with a known location, then go back down the chain
            let mut cur = i;
            let mut known = loop {
                if location[cur] != UNKNOWN {
                    break location[cur];
                }
                if chain.len() > MAX_DEPTH {
                    break NORMAL;
                }
                chain.push(cur);
                let p = parent[cur] as usize;
                if p >= n || p == cur || flags[p] & DIR_IN_USE != DIR_IN_USE {
                    break NORMAL;
                }
                cur = p;
            };
            while let Some(c) = chain.pop() {
                let parent_is_root = parent[c] == ROOT_RECORD;
                known = dir_location(self.name(c as u32), known, parent_is_root);
                location[c] = known;
            }
        }
        location
    }
}

/// How well a name matches a term, higher is better.
fn match_quality(name: &[u8], matcher: &Matcher) -> u8 {
    match matcher {
        Matcher::Ascii(needle) => quality_ascii(name, needle),
        Matcher::Unicode(needle) => {
            let name = String::from_utf8_lossy(name).to_lowercase();
            quality_ascii(name.as_bytes(), needle.as_bytes())
        }
        // Every hit matches the whole pattern, location and the rest decide
        Matcher::Glob(_) | Matcher::Suffix(_) => 2,
    }
}

/// `needle` is lowercase, `name` is compared ignoring ASCII case.
fn quality_ascii(name: &[u8], needle: &[u8]) -> u8 {
    if needle.is_empty() {
        return 0;
    }
    if name.eq_ignore_ascii_case(needle) {
        return 4;
    }
    // "notepad" for notepad.exe
    let stem = match name.iter().rposition(|&c| c == b'.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    };
    if stem.eq_ignore_ascii_case(needle) {
        return 3;
    }
    if name.len() >= needle.len() && name[..needle.len()].eq_ignore_ascii_case(needle) {
        return 2;
    }
    // Start of a word: after a separator or at a lower to upper case change (camelCase)
    let word_start = (1..=name.len().saturating_sub(needle.len())).any(|i| {
        let (before, here) = (name[i - 1], name[i]);
        (!before.is_ascii_alphanumeric()
            || (before.is_ascii_lowercase() && here.is_ascii_uppercase()))
            && name[i..i + needle.len()].eq_ignore_ascii_case(needle)
    });
    if word_start {
        1
    } else {
        0
    }
}

/// Computes the relevance of hits for one query.
pub struct Ranker {
    /// Lowercase matchers and the text as typed, of every term with a name part
    terms: Vec<(Matcher, Vec<u8>)>,
    now: u32,
}

impl Ranker {
    /// `None` if the query has nothing to rank by (no name terms), then the name order stays.
    pub fn new(query: &Query) -> Option<Self> {
        let terms = query
            .include
            .iter()
            .filter_map(|t| t.name_text.as_ref())
            .map(|text| (Matcher::new(text), text.as_bytes().to_vec()))
            .collect::<Vec<_>>();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32);
        (!terms.is_empty()).then_some(Self { terms, now })
    }

    /// Score of entry `id`, higher is better. `locations` comes from
    /// [`VolumeIndex::locations`].
    pub fn score(&self, index: &VolumeIndex, locations: &[u8], id: u32) -> u8 {
        let name = index.name(id);
        let quality = self
            .terms
            .iter()
            .map(|(matcher, _)| match_quality(name, matcher))
            .max()
            .unwrap_or(0);

        // A directory is ranked by its own location, a file by the one of its folder
        let dir = if index.flags(id) & FLAG_DIRECTORY != 0 {
            index.record_of(id)
        } else {
            index.parent(id)
        };
        let location = match locations.get(dir as usize).copied() {
            Some(LOW) => 0,
            Some(USER) => 2,
            _ => 1,
        };

        let same_case = self
            .terms
            .iter()
            .all(|(_, typed)| memmem::find(name, typed).is_some()) as u8;

        let age = self.now.saturating_sub(index.modified(id));
        let recency = match age {
            a if a < 7 * DAY => 3,
            a if a < 30 * DAY => 2,
            a if a < 365 * DAY => 1,
            _ => 0,
        };

        ((quality * 3 + location) * 2 + same_case) * 4 + recency
    }
}

/// Orders `hits` by descending score, keeping the current order for equal scores. Linear time,
/// so ranking millions of hits costs about as much as finding them.
pub fn order_by_score<T: Copy + Default>(hits: &mut Vec<T>, scores: &[u8]) {
    let mut counts = [0usize; 256];
    for &s in scores {
        counts[s as usize] += 1;
    }
    // Start of every score's run, highest score first
    let mut start = [0usize; 256];
    let mut next = 0;
    for s in (0..256).rev() {
        start[s] = next;
        next += counts[s];
    }
    let mut ordered = vec![T::default(); hits.len()];
    for (&hit, &s) in hits.iter().zip(scores) {
        ordered[start[s as usize]] = hit;
        start[s as usize] += 1;
    }
    *hits = ordered;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_levels() {
        let q = |name: &str, needle: &str| quality_ascii(name.as_bytes(), needle.as_bytes());
        assert_eq!(q("Notepad", "notepad"), 4);
        assert_eq!(q("notepad.exe", "notepad"), 3);
        assert_eq!(q("notepad++.exe", "notepad"), 2);
        assert_eq!(q("my-notepad.txt", "notepad"), 1);
        assert_eq!(q("MyNotepad.txt", "notepad"), 1);
        assert_eq!(q("mynotepad.txt", "notepad"), 0);
        assert_eq!(q(".gitignore", "gitignore"), 1);
        assert_eq!(q(".gitignore", ".gitignore"), 4);
    }

    #[test]
    fn locations() {
        assert_eq!(dir_location(b"Windows", NORMAL, true), LOW);
        assert_eq!(dir_location(b"Windows", NORMAL, false), NORMAL);
        assert_eq!(dir_location(b"users", NORMAL, true), USERS_ROOT);
        assert_eq!(dir_location(b"Tim", USERS_ROOT, false), USER);
        assert_eq!(dir_location(b"Documents", USER, false), USER);
        assert_eq!(dir_location(b"AppData", USER, false), LOW);
        assert_eq!(dir_location(b".cargo", USER, false), LOW);
        assert_eq!(dir_location(b"src", LOW, false), LOW);
        assert_eq!(dir_location(b"node_modules", NORMAL, false), LOW);
    }

    #[test]
    fn stable_descending_order() {
        let mut hits = vec![10, 11, 12, 13, 14];
        order_by_score(&mut hits, &[1, 5, 1, 5, 0]);
        assert_eq!(hits, vec![11, 13, 10, 12, 14]);
    }
}
