//! Case-insensitive substring search with path components and exclusions.
//!
//! A query is a list of terms separated by spaces (use quotes for spaces inside a term). An
//! entry has to match every term:
//!
//! - `foo\bar\baz` matches entries whose name contains `baz`, whose parent's name contains `bar`
//!   and whose grandparent's name contains `foo`. A leading `C:` anchors the path at the root of
//!   that volume, a trailing `\` matches everything directly in the matching directories.
//! - `!term` leaves out entries matching `term`.
//! - `!term\` leaves out the directories matching `term` and everything below them.
//! - `!C:\some\folder` leaves out that exact folder and everything below it.

use memchr::{memchr2_iter, memchr_iter};
use rayon::prelude::*;

use crate::index::{VolumeIndex, FLAG_DIRECTORY, FLAG_IN_USE};
use crate::ntfs::ROOT_RECORD;

const DIR_IN_USE: u32 = FLAG_IN_USE | FLAG_DIRECTORY;

#[derive(Debug, Default)]
pub struct Query {
    /// Terms an entry has to match
    pub include: Vec<Term>,
    /// Terms an entry must not match
    pub exclude: Vec<Term>,
    /// Folders whose whole subtree is left out
    pub folders: Vec<FolderExclusion>,
    /// Leave out files
    pub skip_files: bool,
    /// Leave out directories
    pub skip_folders: bool,
}

/// A single term, see the module documentation.
#[derive(Debug, Default)]
pub struct Term {
    pub volume: Option<char>,
    pub anchored: bool,
    pub dirs: Vec<Matcher>,
    pub name: Option<Matcher>,
    /// The name part as typed, for ranking
    pub name_text: Option<String>,
}

/// A folder subtree to leave out. Comparable, so resolved exclusions can be cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderExclusion {
    /// An exact path like `C:\Windows\WinSxS`
    Path(String),
    /// Every directory matching a term like `node_modules`
    Matching(String),
}

impl Query {
    pub fn parse(text: &str) -> Self {
        let mut query = Query::default();
        for token in tokenize(text) {
            let Some(negated) = token.strip_prefix('!') else {
                query.include.push(Term::parse(&token));
                continue;
            };
            if negated.is_empty() {
                continue;
            }
            if is_absolute_path(negated) {
                query
                    .folders
                    .push(FolderExclusion::Path(negated.to_string()));
            } else if negated.ends_with(['\\', '/']) {
                let term = negated.trim_end_matches(['\\', '/']);
                if !term.is_empty() {
                    query
                        .folders
                        .push(FolderExclusion::Matching(term.to_string()));
                }
            } else {
                query.exclude.push(Term::parse(negated));
            }
        }
        query
    }

    pub fn is_match_all(&self) -> bool {
        self.include.iter().all(Term::is_match_all)
            && self.exclude.is_empty()
            && self.folders.is_empty()
            && !self.skip_files
            && !self.skip_folders
    }
}

/// Splits at whitespace outside of double quotes and removes the quotes.
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// `C:` or `C:\...`
fn is_absolute_path(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || b[2] == b'\\' || b[2] == b'/')
}

impl Term {
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        let ends_with_separator = text.ends_with(['\\', '/']);
        let mut parts = text
            .split(['\\', '/'])
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>();

        let mut term = Term::default();
        if let Some(first) = parts.first() {
            let b = first.as_bytes();
            if b.len() == 2
                && b[1] == b':'
                && b[0].is_ascii_alphabetic()
                && (parts.len() > 1 || ends_with_separator)
            {
                term.volume = Some(b[0].to_ascii_uppercase() as char);
                term.anchored = true;
                parts.remove(0);
            }
        }

        if !ends_with_separator {
            if let Some(name) = parts.pop() {
                term.name = Some(Matcher::new(name));
                term.name_text = Some(name.to_string());
            }
        }
        term.dirs = parts.into_iter().map(Matcher::new).collect();
        term
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

/// A term prepared for one volume.
struct PreparedTerm<'a> {
    term: &'a Term,
    /// Directories the entry has to be in, if the term has path components
    parents: Option<Vec<u64>>,
}

impl PreparedTerm<'_> {
    #[inline]
    fn matches(&self, index: &VolumeIndex, id: u32) -> bool {
        self.parents
            .as_ref()
            .is_none_or(|set| bit(set, index.parent(id)))
            && self
                .term
                .name
                .as_ref()
                .is_none_or(|m| m.matches(index.name(id)))
    }
}

impl VolumeIndex {
    fn on_this_volume(&self, term: &Term) -> bool {
        term.volume
            .is_none_or(|v| v == self.volume.id.to_ascii_uppercase())
    }

    fn prepare<'a>(&self, term: &'a Term) -> PreparedTerm<'a> {
        PreparedTerm {
            term,
            parents: (term.anchored || !term.dirs.is_empty()).then(|| self.match_dirs(term)),
        }
    }

    /// Matching entries in name order. `excluded` is a bitset of directories (see
    /// [`VolumeIndex::exclusions`]) whose entries are left out.
    pub fn search(&self, query: &Query, excluded: Option<&[u64]>) -> Vec<u32> {
        if !query.include.iter().all(|t| self.on_this_volume(t))
            || (query.skip_files && query.skip_folders)
        {
            return Vec::new();
        }
        let include = query
            .include
            .iter()
            .filter(|t| !t.is_match_all())
            .map(|t| self.prepare(t))
            .collect::<Vec<_>>();
        let exclude = query
            .exclude
            .iter()
            .filter(|t| self.on_this_volume(t) && !t.is_match_all())
            .map(|t| self.prepare(t))
            .collect::<Vec<_>>();

        // Only one of them can be set here, both leave nothing
        let skip_kind = (query.skip_files || query.skip_folders).then_some(if query.skip_folders {
            FLAG_DIRECTORY
        } else {
            0
        });

        self.sorted
            .par_iter()
            .with_min_len(4096)
            .copied()
            .filter(|&id| {
                skip_kind.is_none_or(|skip| self.flags(id) & FLAG_DIRECTORY != skip)
                    && excluded.is_none_or(|set| !bit(set, self.parent(id)) && !bit(set, id))
                    && include.iter().all(|t| t.matches(self, id))
                    && !exclude.iter().any(|t| t.matches(self, id))
            })
            .collect()
    }

    /// The exclusion bitset for the folder exclusions of a query, `None` if none of them
    /// applies to this volume.
    pub fn exclusions(&self, folders: &[FolderExclusion]) -> Option<Vec<u64>> {
        let mut roots = Vec::new();
        for folder in folders {
            match folder {
                FolderExclusion::Path(path) => roots.extend(self.find_directory(path)),
                FolderExclusion::Matching(text) => {
                    let term = Term::parse(text);
                    if !self.on_this_volume(&term) {
                        continue;
                    }
                    let prepared = self.prepare(&term);
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
        (!roots.is_empty()).then(|| self.excluded_dirs(&roots))
    }

    /// Bitset of the directories the last path component of the term may be in.
    fn match_dirs(&self, term: &Term) -> Vec<u64> {
        let n = self.records.len();
        let words = n.div_ceil(64);

        let mut set = term.anchored.then(|| {
            let mut root = vec![0u64; words];
            if let Some(w) = root.get_mut(ROOT_RECORD as usize / 64) {
                *w |= 1 << (ROOT_RECORD % 64);
            }
            root
        });

        for matcher in &term.dirs {
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
                        if flags[id] & DIR_IN_USE == DIR_IN_USE
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
    fn parse_term() {
        let t = Term::parse(r"c:\windows\notepad");
        assert_eq!(t.volume, Some('C'));
        assert!(t.anchored);
        assert_eq!(t.dirs.len(), 1);
        assert!(t.name.is_some());

        let t = Term::parse(r"system32\");
        assert_eq!(t.dirs.len(), 1);
        assert!(t.name.is_none());

        let t = Term::parse("c:");
        assert!(t.volume.is_none());
        assert!(t.name.is_some());
    }

    #[test]
    fn parse_query() {
        assert!(Query::parse("  ").is_match_all());

        let q = Query::parse(r#"notepad "my file" !winsxs !node_modules\ !C:\Windows\Temp"#);
        assert_eq!(q.include.len(), 2);
        assert_eq!(q.exclude.len(), 1);
        assert_eq!(
            q.folders,
            vec![
                FolderExclusion::Matching("node_modules".into()),
                FolderExclusion::Path(r"C:\Windows\Temp".into()),
            ]
        );
        assert!(!q.is_match_all());

        // A lone `!` is ignored
        assert!(Query::parse("!").is_match_all());
    }
}
