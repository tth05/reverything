//! Case-insensitive substring search with path components and folder scopes.
//!
//! A query is a list of terms separated by spaces. An entry has to match every term:
//!
//! - `foo\bar\baz` matches entries whose name contains `baz`, whose parent's name contains `bar`
//!   and whose grandparent's name contains `foo`. A leading `C:` anchors the path at the root of
//!   that volume, a trailing `\` matches everything directly in the matching directories.
//! - `'a b'` keeps the spaces in a term. `"a b"` also does, and has to match the whole name.
//!   Quotes can cover single path segments (`"src"\main`) or the whole term.
//! - A segment with `*` (any characters) or `?` (one character) has to match as a whole, e.g.
//!   `*.mp3` or `report-??.pdf`. Without them it matches anywhere in the name.
//! - `**` as a folder stands for any number of folders, also none: `C:\**\AppData\` is every
//!   `AppData` folder on `C:`, `photos\**` everything below folders matching `photos`.
//! - `!term` leaves out entries matching `term`.
//! - `!folder\` leaves out everything below the directories matching `folder`, `+folder\` leaves
//!   out everything else. Several `+` folders add up, and the closest of the matching folders
//!   above an entry decides; on the same folder `!` wins. Both only look at the folders above an
//!   entry, not at the entry itself. `+term` without `\` is the same as `term`.
//! - `size:`, `dm:` and `dc:` filter by size, modification and creation date, see
//!   [`crate::index::filter`].

use fixedbitset::{Block, FixedBitSet};
use memchr::{memchr2_iter, memchr_iter};
use rayon::prelude::*;

use crate::index::filter::{Field, Filter};
use crate::index::scope::Scope;
use crate::index::{prefetch, prefetched, VolumeIndex, FLAG_DIRECTORY, FLAG_IN_USE};
use crate::ntfs::ROOT_RECORD;

const DIR_IN_USE: u32 = FLAG_IN_USE | FLAG_DIRECTORY;

#[derive(Debug, Default)]
pub struct Query {
    /// Terms an entry has to match
    pub include: Vec<Term>,
    /// Terms an entry must not match
    pub exclude: Vec<Term>,
    /// Folders whose contents are searched or left out
    pub folders: Vec<FolderScope>,
    /// Size and date conditions
    pub filters: Vec<Filter>,
    /// Leave out files
    pub skip_files: bool,
    /// Leave out directories
    pub skip_folders: bool,
}

/// A single term, see the module documentation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Term {
    pub volume: Option<char>,
    pub anchored: bool,
    pub dirs: Vec<Matcher>,
    pub name: Option<Matcher>,
    /// The name part as typed, for ranking
    pub name_text: Option<String>,
}

/// `+folder\` or `!folder\`: the directories matching `folder` (the name of the term, or the
/// root for a bare `C:\`) and whether their contents are searched or left out. Comparable, so
/// resolved scopes can be cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderScope {
    pub include: bool,
    pub folder: Term,
}

impl Query {
    pub fn parse(text: &str) -> Self {
        let mut query = Query::default();
        for token in tokenize(text) {
            let negate = token.prefix == Some('!');
            if !token.quoted {
                if let Some(filter) = Filter::parse(&token.raw, negate) {
                    query.filters.push(filter);
                    continue;
                }
            }
            let term = Term::from_segments(&token.segments, token.folder);
            if term.is_match_all() {
                continue;
            }
            match token.prefix {
                Some(prefix) if token.folder => query.folders.push(FolderScope {
                    include: prefix == '+',
                    folder: term.into_folder(),
                }),
                Some('!') => query.exclude.push(term),
                _ => query.include.push(term),
            }
        }
        query
    }

    pub fn is_match_all(&self) -> bool {
        self.include.iter().all(Term::is_match_all)
            && self.exclude.is_empty()
            && self.folders.is_empty()
            && self.filters.is_empty()
            && !self.skip_files
            && !self.skip_folders
    }
}

/// A term split into path segments.
#[derive(Debug, Default, PartialEq)]
struct Token {
    /// `!` or `+` in front, outside of quotes
    prefix: Option<char>,
    segments: Vec<Segment>,
    /// Ends with a separator
    folder: bool,
    /// Has quotes, so it is no filter
    quoted: bool,
    /// The text after the prefix, for filters
    raw: String,
}

#[derive(Debug, PartialEq)]
struct Segment {
    text: String,
    /// In double quotes: matches the whole name
    exact: bool,
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Splits at whitespace outside of quotes, and the terms at `\` and `/`.
///
/// Names can contain `'` but not `"`, so a `'` only opens a quote at the start of a segment and
/// only closes it before whitespace, a separator or the end: `bob's files` needs no quotes. An
/// unclosed quote covers the rest of the text.
fn tokenize(text: &str) -> Vec<Token> {
    let chars = text.chars().collect::<Vec<_>>();
    let is_separator = |c: char| c == '\\' || c == '/';
    let mut tokens = Vec::new();
    let mut token = Token::default();
    let mut segment = String::new();
    let mut exact = false;
    let mut quote = Quote::None;

    let end_segment = |token: &mut Token, segment: &mut String, exact: &mut bool, quote: Quote| {
        if !segment.is_empty() {
            token.segments.push(Segment {
                text: std::mem::take(segment),
                exact: *exact,
            });
        }
        *exact = quote == Quote::Double;
    };

    for (i, &c) in chars.iter().enumerate() {
        if quote == Quote::None && c.is_whitespace() {
            end_segment(&mut token, &mut segment, &mut exact, quote);
            if token.prefix.is_some() || !token.raw.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
            continue;
        }
        if quote == Quote::None
            && matches!(c, '!' | '+')
            && token.prefix.is_none()
            && token.raw.is_empty()
        {
            token.prefix = Some(c);
            continue;
        }
        token.raw.push(c);
        match (quote, c) {
            (_, c) if is_separator(c) => {
                end_segment(&mut token, &mut segment, &mut exact, quote);
                token.folder = true;
            }
            (Quote::None, '"') => {
                quote = Quote::Double;
                exact = true;
                token.quoted = true;
            }
            (Quote::Double, '"') => quote = Quote::None,
            // Names can not contain it
            (Quote::Single, '"') => {}
            (Quote::None, '\'') if segment.is_empty() && !exact => {
                quote = Quote::Single;
                token.quoted = true;
            }
            (Quote::Single, '\'')
                if chars
                    .get(i + 1)
                    .is_none_or(|&n| n.is_whitespace() || is_separator(n)) =>
            {
                quote = Quote::None
            }
            (_, c) => {
                segment.push(c);
                token.folder = false;
            }
        }
    }
    end_segment(&mut token, &mut segment, &mut exact, quote);
    if token.prefix.is_some() || !token.raw.is_empty() {
        tokens.push(token);
    }
    tokens
}

/// `C:`
fn is_drive(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

impl Term {
    /// A single term without quotes, e.g. `windows\system32\`.
    pub fn parse(text: &str) -> Self {
        let token = tokenize(text).into_iter().next().unwrap_or_default();
        Self::from_segments(&token.segments, token.folder)
    }

    fn from_segments(segments: &[Segment], folder: bool) -> Self {
        let matcher = |s: &Segment| {
            if s.exact {
                Matcher::exact(&s.text)
            } else {
                Matcher::new(&s.text)
            }
        };
        let mut term = Term::default();
        let mut segments = segments;
        if let Some(first) = segments.first() {
            if is_drive(&first.text) && (segments.len() > 1 || folder) {
                term.volume = Some(first.text.as_bytes()[0].to_ascii_uppercase() as char);
                term.anchored = true;
                segments = &segments[1..];
            }
        }
        let levels = |s: &Segment| !s.exact && s.text == "**";
        let name = match segments.split_last() {
            // `a\**` is everything below `a`, like `a\**\`
            Some((name, dirs)) if !folder && !levels(name) => {
                segments = dirs;
                Some(name)
            }
            _ => None,
        };
        term.dirs = segments
            .iter()
            .map(|s| {
                if levels(s) {
                    Matcher::Levels
                } else {
                    matcher(s)
                }
            })
            .collect();
        term.dirs
            .dedup_by(|a, b| *a == Matcher::Levels && *b == Matcher::Levels);
        // Any levels below any folder is no condition
        if !term.anchored && term.dirs.first() == Some(&Matcher::Levels) {
            term.dirs.remove(0);
        }
        // `*` alone matches every name, like no name part at all
        if let Some(name) = name.filter(|n| !n.text.chars().all(|c| c == '*')) {
            term.name = Some(matcher(name));
            term.name_text = Some(name.text.clone());
        }
        term
    }

    /// The term of `+term\` or `!term\`, whose name is the folder: `a\b\` becomes `a\b`.
    fn into_folder(mut self) -> Self {
        if self.name.is_none() {
            // Everything below a folder counts anyway
            if self.dirs.last() == Some(&Matcher::Levels) {
                self.dirs.pop();
            }
            self.name = self.dirs.pop();
        }
        self.name_text = None;
        self
    }

    pub fn is_match_all(&self) -> bool {
        self.volume.is_none() && self.dirs.is_empty() && self.name.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    /// Lowercased ASCII needle, matched with ASCII case folding
    Ascii(Vec<u8>),
    /// Lowercased needle containing non-ASCII characters
    Unicode(String),
    /// Lowercased ASCII text that has to be the whole name
    Exact(Vec<u8>),
    /// Lowercased pattern with `*` and `?` that has to match the whole name
    Glob(String),
    /// A pattern like `*.mp3`: the name ends with this lowercased ASCII text
    Suffix(Vec<u8>),
    /// `**` among the folders of a term: any number of folders. Matches every name.
    Levels,
}

impl Matcher {
    /// Matches the whole name, with wildcards if there are any.
    pub fn exact(text: &str) -> Self {
        if text.is_ascii() && !text.contains(['*', '?']) {
            Matcher::Exact(text.to_ascii_lowercase().into_bytes())
        } else {
            // Without wildcards a pattern is the whole name
            Matcher::Glob(text.to_lowercase())
        }
    }

    pub fn new(needle: &str) -> Self {
        if needle.contains(['*', '?']) {
            let pattern = needle.to_lowercase();
            match pattern.strip_prefix('*') {
                // The most common pattern, checked without the general matching
                Some(rest) if rest.is_ascii() && !rest.contains(['*', '?']) => {
                    Matcher::Suffix(rest.as_bytes().to_vec())
                }
                _ => Matcher::Glob(pattern),
            }
        } else if needle.is_ascii() {
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
            Matcher::Exact(text) => hay.eq_ignore_ascii_case(text),
            Matcher::Levels => true,
            Matcher::Glob(pattern) => glob_matches(pattern, hay),
            Matcher::Suffix(suffix) => {
                hay.len() >= suffix.len()
                    && hay[hay.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
            }
        }
    }
}

impl Matcher {
    /// Byte ranges of `name` that the matcher matched, for highlighting. Only for display, so it
    /// may be approximate for unusual names.
    pub fn find(&self, name: &str) -> Vec<std::ops::Range<usize>> {
        match self {
            Matcher::Ascii(needle) => find_ci(name, std::str::from_utf8(needle).unwrap_or(""), 0)
                .into_iter()
                .collect(),
            Matcher::Unicode(needle) => find_ci(name, needle, 0).into_iter().collect(),
            Matcher::Levels => Vec::new(),
            Matcher::Exact(text) => name
                .as_bytes()
                .eq_ignore_ascii_case(text)
                .then_some(0..name.len())
                .into_iter()
                .collect(),
            Matcher::Suffix(suffix) => {
                let start = name.len().saturating_sub(suffix.len());
                if name.is_char_boundary(start)
                    && name[start..].eq_ignore_ascii_case(std::str::from_utf8(suffix).unwrap_or(""))
                    && !suffix.is_empty()
                {
                    std::iter::once(start..name.len()).collect()
                } else {
                    Vec::new()
                }
            }
            // The literal parts between the wildcards, in order
            Matcher::Glob(pattern) => {
                let mut ranges = Vec::new();
                let mut from = 0;
                for part in pattern.split(['*', '?']).filter(|p| !p.is_empty()) {
                    match find_ci(name, part, from) {
                        Some(range) => {
                            from = range.end;
                            ranges.push(range);
                        }
                        None => break,
                    }
                }
                ranges
            }
        }
    }
}

/// First case-insensitive occurrence of `needle` (lowercase) in `hay` at or after byte `from`.
fn find_ci(hay: &str, needle: &str, from: usize) -> Option<std::ops::Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    let lower = |c: char| c.to_lowercase();
    hay.char_indices()
        .filter(|&(i, _)| i >= from)
        .find_map(|(start, _)| {
            let mut hay_chars = hay[start..].char_indices().flat_map(|(i, c)| {
                let end = start + i + c.len_utf8();
                lower(c).map(move |l| (l, end))
            });
            let mut end = start;
            for n in needle.chars() {
                match hay_chars.next() {
                    Some((l, e)) if l == n => end = e,
                    _ => return None,
                }
            }
            Some(start..end)
        })
}

/// Whether the whole `hay` matches `pattern` (lowercase) with `*` and `?`, ignoring case.
fn glob_matches(pattern: &str, hay: &[u8]) -> bool {
    if pattern.is_ascii() && hay.is_ascii() {
        // No allocation for the common case
        return glob(pattern.as_bytes(), hay, |p, t| p == t.to_ascii_lowercase());
    }
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = String::from_utf8_lossy(hay)
        .to_lowercase()
        .chars()
        .collect::<Vec<_>>();
    glob(&pattern, &text, |p, t| p == t)
}

/// Greedy wildcard matching that goes back to the last `*` on a mismatch.
fn glob<T: Copy + From<u8> + PartialEq>(
    pattern: &[T],
    text: &[T],
    eq: impl Fn(T, T) -> bool,
) -> bool {
    let (any, one) = (T::from(b'*'), T::from(b'?'));
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(&c) if c == any => {
                star = Some((p, t));
                p += 1;
            }
            Some(&c) if c == one || eq(c, text[t]) => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    p = sp + 1;
                    t = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == any)
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

/// A term prepared for one volume.
pub(crate) struct PreparedTerm<'a> {
    term: &'a Term,
    /// Directories the entry has to be in, if the term has path components
    parents: Option<FixedBitSet>,
}

impl PreparedTerm<'_> {
    #[inline]
    pub(crate) fn matches(&self, index: &VolumeIndex, id: u32) -> bool {
        self.parents
            .as_ref()
            .is_none_or(|set| set.contains(index.parent(id) as usize))
            && self
                .term
                .name
                .as_ref()
                .is_none_or(|m| m.matches(index.name(id)))
    }
}

impl VolumeIndex {
    pub(crate) fn on_this_volume(&self, term: &Term) -> bool {
        term.volume
            .is_none_or(|v| v == self.volume.id.to_ascii_uppercase())
    }

    pub(crate) fn prepare<'a>(&self, term: &'a Term) -> PreparedTerm<'a> {
        PreparedTerm {
            term,
            parents: (term.anchored || !term.dirs.is_empty()).then(|| self.match_dirs(term)),
        }
    }

    /// Matching entries in name order, within the query's resolved folder scope (see
    /// [`VolumeIndex::scope`]).
    pub fn search(&self, query: &Query, scope: &Scope) -> Vec<u32> {
        self.search_cancellable(query, scope, &|| false)
    }

    /// [`VolumeIndex::search`] that skips the rest once `cancelled` returns true, which it asks
    /// once per chunk of entries. The result is incomplete then.
    pub fn search_cancellable(
        &self,
        query: &Query,
        scope: &Scope,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Vec<u32> {
        let excluded = match scope {
            Scope::All => None,
            Scope::Nothing => return Vec::new(),
            Scope::Excluding(dirs) => Some(dirs),
        };
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

        let keep = |id: u32| {
            skip_kind.is_none_or(|skip| self.flags(id) & FLAG_DIRECTORY != skip)
                && query.filters.iter().all(|f| {
                    f.matches(match f.field {
                        Field::Size => self.size(id),
                        Field::Modified => self.modified(id) as u64,
                        Field::Created => self.created(id) as u64,
                    })
                })
                && excluded.is_none_or(|set| !set.contains(self.parent(id) as usize))
                && include.iter().all(|t| t.matches(self, id))
                && !exclude.iter().any(|t| t.matches(self, id))
        };
        let needs_names = include
            .iter()
            .chain(&exclude)
            .any(|t| t.term.name.is_some());
        let needs_parent =
            excluded.is_some() || include.iter().chain(&exclude).any(|t| t.parents.is_some());
        let records = &self.records;
        let far = |id: u32| {
            let i = id as usize;
            if needs_names {
                self.prefetch_name_position(id);
            }
            if let Some(parent) = records.parent.get(i).filter(|_| needs_parent) {
                prefetch(parent);
            }
            if let Some(flags) = records.flags.get(i).filter(|_| skip_kind.is_some()) {
                prefetch(flags);
            }
        };
        let near = |id: u32| {
            if needs_names {
                self.prefetch_name(id);
            }
        };

        self.sorted
            .par_chunks(4096)
            .flat_map_iter(|chunk| {
                let mut hits = Vec::new();
                if !cancelled() {
                    prefetched(chunk, far, near, |id| {
                        if keep(id) {
                            hits.push(id);
                        }
                    });
                }
                hits.into_iter()
            })
            .collect()
    }

    /// The directories the last path component of the term may be in, by record number.
    fn match_dirs(&self, term: &Term) -> FixedBitSet {
        let n = self.records.len();
        let mut set = term.anchored.then(|| {
            let mut root = FixedBitSet::with_capacity(n);
            root.set(ROOT_RECORD as usize, (ROOT_RECORD as usize) < n);
            root
        });

        for matcher in &term.dirs {
            if *matcher == Matcher::Levels {
                set = set.map(|dirs| self.below(&dirs));
                continue;
            }
            let prev = set.as_ref();
            let flags = &self.records.flags;
            let parent = &self.records.parent;
            // A block per task, so the bits are set without synchronization
            let blocks = (0..n.div_ceil(Block::BITS as usize))
                .into_par_iter()
                .with_min_len(256)
                .map(|block| {
                    let first = block * Block::BITS as usize;
                    let mut bits: Block = 0;
                    for id in first..n.min(first + Block::BITS as usize) {
                        if flags[id] & DIR_IN_USE == DIR_IN_USE
                            && prev.is_none_or(|p| p.contains(parent[id] as usize))
                            && matcher.matches(self.name(id as u32))
                        {
                            bits |= 1 << (id - first);
                        }
                    }
                    bits
                })
                .collect::<Vec<_>>();
            set = Some(FixedBitSet::with_capacity_and_blocks(n, blocks));
        }

        set.unwrap_or_else(|| {
            let mut all = FixedBitSet::with_capacity(n);
            all.insert_range(..);
            all
        })
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
    fn globs() {
        let m = |pattern: &str, name: &str| Matcher::new(pattern).matches(name.as_bytes());
        assert!(m("*.mp3", "Song.MP3"));
        assert!(!m("*.mp3", "song.mp3.part"));
        assert!(m("report-??.pdf", "Report-07.pdf"));
        assert!(!m("report-??.pdf", "report-7.pdf"));
        assert!(m("*", ""));
        assert!(m("a*b*c", "aXXbYYc"));
        assert!(!m("a*b*c", "aXXbYY"));
        assert!(m("*ärger*", "Großer ÄRGER.txt"));
        assert!(m("*.MP3", "song.mp3"));
        assert!(!m("*.mp3", "mp3"));
        // Without wildcards, still a substring
        assert!(m("song", "my song.mp3"));
        assert!(Term::parse("*").is_match_all());
        assert!(Term::parse(r"system32\*").name.is_none());
    }

    #[test]
    fn find_ranges() {
        let f = |pattern: &str, name: &str| Matcher::new(pattern).find(name);
        assert_eq!(f("pad", "Notepad.exe"), vec![4..7]);
        assert_eq!(f("*.EXE", "Notepad.exe"), vec![7..11]);
        assert_eq!(f("note*.e?e", "Notepad.exe"), vec![0..4, 7..9, 10..11]);
        assert_eq!(f("ärger", "Großer ÄRGER"), vec![8..14]);
        assert!(f("xyz", "Notepad.exe").is_empty());
    }

    #[test]
    fn parse_term() {
        let t = Term::parse(r"c:\windows\notepad");
        assert_eq!(t.volume, Some('C'));
        assert!(t.anchored);
        assert_eq!(t.dirs, vec![Matcher::new("windows")]);
        assert_eq!(t.name, Some(Matcher::new("notepad")));

        let t = Term::parse(r"system32\");
        assert_eq!(t.dirs.len(), 1);
        assert!(t.name.is_none());

        let t = Term::parse("c:");
        assert!(t.volume.is_none());
        assert!(t.name.is_some());
    }

    #[test]
    fn tokens() {
        let segments = |text: &str| {
            tokenize(text)
                .into_iter()
                .map(|t| {
                    let parts = t
                        .segments
                        .iter()
                        .map(|s| {
                            if s.exact {
                                format!("={}", s.text)
                            } else {
                                s.text.clone()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("|");
                    let prefix = t.prefix.map(String::from).unwrap_or_default();
                    format!("{prefix}{parts}{}", if t.folder { "/" } else { "" })
                })
                .collect::<Vec<_>>()
        };
        // Apostrophes inside words need no quotes
        assert_eq!(segments("bob's files"), ["bob's", "files"]);
        assert_eq!(segments("'a b'  c"), ["a b", "c"]);
        assert_eq!(segments("'bob's file' x"), ["bob's file", "x"]);
        assert_eq!(segments(r#""my file""#), ["=my file"]);
        assert_eq!(segments(r"+'My Projects'\ .rs"), ["+My Projects/", ".rs"]);
        assert_eq!(
            segments(r#""C:\Program Files\foo.txt""#),
            ["=C:|=Program Files|=foo.txt"]
        );
        assert_eq!(segments(r#""src"\main"#), ["=src|main"]);
        assert_eq!(segments(r#"!C:\"Windows"\"#), [r"!C:|=Windows/"]);
        assert_eq!(segments("a/b/"), ["a|b/"]);
        // Unclosed quotes cover the rest, while typing
        assert_eq!(segments("'my fi"), ["my fi"]);
        assert_eq!(segments(r#"x "my fi"#), ["x", "=my fi"]);
        // Quotes keep a `+` or `!` that is part of the name
        assert_eq!(
            segments("'+notes' '!x'"),
            ["+notes", "!x"].map(|s| s.to_string())
        );
        assert_eq!(segments("!"), ["!"]);
        assert!(segments("  ").is_empty());
    }

    #[test]
    fn parse_filters() {
        let q = Query::parse("report size:>1mb !dm:today dm: dm:2024/05/01 'size:>1mb'");
        assert_eq!(q.filters.len(), 2);
        assert!(q.filters[1].negate);
        // Not valid filters, or quoted, so normal terms
        assert_eq!(q.include.len(), 4);
        assert!(!q.is_match_all());
    }

    #[test]
    fn parse_query() {
        assert!(Query::parse("  ").is_match_all());

        let q = Query::parse(r"notepad 'my file' !winsxs !node_modules\ +C:\code\ +x");
        assert_eq!(q.include.len(), 3);
        assert_eq!(q.exclude.len(), 1);
        assert_eq!(
            q.folders
                .iter()
                .map(|f| (f.include, f.folder.volume, f.folder.name.clone()))
                .collect::<Vec<_>>(),
            vec![
                (false, None, Some(Matcher::new("node_modules"))),
                (true, Some('C'), Some(Matcher::new("code"))),
            ]
        );
        assert!(!q.is_match_all());

        // A lone `!` or `+` is ignored
        assert!(Query::parse("! + \\").is_match_all());
    }

    #[test]
    fn exact() {
        let m = |text: &str, name: &str| Matcher::exact(text).matches(name.as_bytes());
        assert!(m("readme.md", "README.md"));
        assert!(!m("readme.md", "readme.md.bak"));
        assert!(m("größe", "GRÖSSE".replace("SS", "ß").as_str()));
        assert!(m("*.md", "a.md"));
        assert_eq!(Matcher::exact("Readme").find("README"), vec![0..6]);
        assert!(Matcher::exact("Readme").find("READMEs").is_empty());
    }

    #[test]
    fn folder_scopes() {
        let index = crate::index::testing::tree(
            'C',
            &[
                r"code\app\main.rs",
                r"code\app\target\out.rs",
                r"code\target\keep\a.rs",
                r"other\main.rs",
                r"other\target\x.rs",
                r"My Projects\main.rs",
                r"Windows\note.txt",
                r"Windows.old\note.txt",
                r"readme.md",
                r"readme.md.bak",
            ],
        );
        let find = |text: &str| {
            let query = Query::parse(text);
            let scope = index.scope(&query.folders);
            let mut paths = index
                .search(&query, &scope)
                .into_iter()
                .map(|id| index.full_path(id)[3..].to_string())
                .collect::<Vec<_>>();
            paths.sort();
            paths
        };

        assert_eq!(
            find(r".rs !target\"),
            [
                r"My Projects\main.rs",
                r"code\app\main.rs",
                r"other\main.rs"
            ]
        );
        // Only what is in the folders is left out, not the folders
        assert_eq!(
            find(r"target !target\"),
            [r"code\app\target", r"code\target", r"other\target"]
        );
        assert_eq!(
            find(r"+code\ .rs"),
            [
                r"code\app\main.rs",
                r"code\app\target\out.rs",
                r"code\target\keep\a.rs"
            ]
        );
        assert_eq!(
            find(r"+code\ +other\ main"),
            [r"code\app\main.rs", r"other\main.rs"]
        );
        assert_eq!(find(r"+code\ !target\ .rs"), [r"code\app\main.rs"]);
        // The closest folder decides
        assert_eq!(
            find(r"!code\ +code\app\ .rs"),
            [r"code\app\main.rs", r"code\app\target\out.rs"]
        );
        assert_eq!(find(r"+code\ !code\ .rs"), Vec::<String>::new());
        // Segments match anywhere unless in double quotes
        assert_eq!(
            find(r"+C:\Windows\ note"),
            [r"Windows.old\note.txt", r"Windows\note.txt"]
        );
        assert_eq!(find(r#"+C:\"Windows"\ note"#), [r"Windows\note.txt"]);
        assert_eq!(find(r#"+"C:\Windows"\ note"#), [r"Windows\note.txt"]);
        assert_eq!(find(r#""readme.md""#), ["readme.md"]);
        assert_eq!(find("readme.md"), ["readme.md", "readme.md.bak"]);
        assert_eq!(find(r"'my projects'\main"), [r"My Projects\main.rs"]);
        assert_eq!(find(r"+'my projects'\ main"), [r"My Projects\main.rs"]);
        // Without `\` a plain term
        assert_eq!(find("+main"), find("main"));
        // Other volumes have nothing in scope
        assert!(find(r"+D:\ main").is_empty());
        assert_eq!(
            find(r"+C:\ !C:\code\ main.rs"),
            [r"My Projects\main.rs", r"other\main.rs"]
        );
    }

    #[test]
    fn levels() {
        let index = crate::index::testing::tree(
            'C',
            &[
                r"AppData\top.txt",
                r"Users\me\AppData\Local\x.txt",
                r"Users\me\AppData\Roaming\y.txt",
                r"Users\me\notes.txt",
                r"other\deep\AppData\w.txt",
                r"other\deep\AppDataOld\v.txt",
            ],
        );
        let find = |text: &str| {
            let query = Query::parse(text);
            let scope = index.scope(&query.folders);
            let mut paths = index
                .search(&query, &scope)
                .into_iter()
                .map(|id| index.full_path(id)[3..].to_string())
                .collect::<Vec<_>>();
            paths.sort();
            paths
        };
        let txt = |paths: Vec<String>| {
            paths
                .into_iter()
                .filter(|p| p.ends_with(".txt"))
                .collect::<Vec<_>>()
        };

        // Any number of folders, also none
        assert_eq!(
            txt(find(r#"+C:\**\"AppData"\"#)),
            [
                r"AppData\top.txt",
                r"Users\me\AppData\Local\x.txt",
                r"Users\me\AppData\Roaming\y.txt",
                r"other\deep\AppData\w.txt",
            ]
        );
        assert_eq!(
            find(r#"C:\**\"AppData"\"#),
            [
                r"AppData\top.txt",
                r"Users\me\AppData\Local",
                r"Users\me\AppData\Roaming",
                r"other\deep\AppData\w.txt",
            ]
        );
        assert_eq!(
            txt(find(r"Users\**")),
            [
                r"Users\me\AppData\Local\x.txt",
                r"Users\me\AppData\Roaming\y.txt",
                r"Users\me\notes.txt",
            ]
        );
        assert_eq!(
            txt(find(r"users\**\local\*")),
            [r"Users\me\AppData\Local\x.txt"]
        );
        assert_eq!(txt(find(r"+C:\**\ .txt")).len(), 6);
        assert_eq!(find(r"**\**\top"), find("top"));
        assert_eq!(
            txt(find(r#"!**\"appdata"\ .txt"#)),
            [r"Users\me\notes.txt", r"other\deep\AppDataOld\v.txt"]
        );
        assert!(Query::parse(r"** !**").is_match_all());
    }
}
