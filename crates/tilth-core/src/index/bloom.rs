//! Per-file Bloom filters for fast "does file X contain symbol Y?" queries.
//!
//! Used to pre-filter candidate files before expensive tree-sitter parsing
//! in callee/caller resolution. A Bloom filter can definitively say "no"
//! (symbol is NOT in this file) but may produce false positives.
//!
//! Identifier extraction uses a simple byte-level state machine -- no
//! tree-sitter needed -- making it fast enough to run on every uncached file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};
use std::time::SystemTime;

use fastbloom::BloomFilter;

use crate::lang::detect_file_type;
use crate::lang::spec::{Interpolation, NameRule};
use crate::types::{FileType, Lang};

// ---------------------------------------------------------------------------
// BloomFilterCache
// ---------------------------------------------------------------------------

/// The most memory the cached filters may hold, by [`entry_bytes`]. A callers
/// walk over the cargo registry source (1,420 crates, 1.9 GB, 67,762 files
/// cached) fills 77 MB, so a repeated walk over a tree that size finds every
/// filter still cached, with room for a tree 1.7 times larger. Half this
/// budget would not hold it, and a repeated walk over it would rebuild most of
/// its filters.
const CACHE_BUDGET_BYTES: usize = 128 * 1024 * 1024;

/// Thread-safe cache of per-file Bloom filters, keyed by path and validated
/// by mtime. Stale entries are automatically rebuilt on access.
///
/// The filters stay within a memory budget. When a new filter would take
/// them over it, the cache drops them all and starts again: a dropped file
/// is checked again from its content, as on its first visit. A walk over a
/// tree that fits the budget never meets it; a walk over a larger one
/// rebuilds filters as it would with no cache.
pub struct BloomFilterCache {
    filters: RwLock<Filters>,
    budget: usize,
}

/// The cached filters and the bytes they hold, kept together under one lock
/// so the count is exact.
#[derive(Default)]
struct Filters {
    entries: HashMap<PathBuf, Entry>,
    bytes: usize,
}

struct Entry {
    filter: BloomFilter,
    mtime: SystemTime,
}

impl Default for BloomFilterCache {
    fn default() -> Self {
        Self::new()
    }
}

impl BloomFilterCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::with_budget(CACHE_BUDGET_BYTES)
    }

    fn with_budget(budget: usize) -> Self {
        Self {
            filters: RwLock::default(),
            budget,
        }
    }

    /// Check if `symbol` might appear in the file at `path`.
    ///
    /// - If a cached filter exists with matching `mtime`, queries it directly.
    /// - Otherwise, builds a new filter from `content`, caches it, then queries.
    ///
    /// Returns `true` if the symbol MIGHT be in the file (possible false positive),
    /// `false` if it is DEFINITELY absent.
    #[must_use]
    pub fn contains(&self, path: &Path, mtime: SystemTime, content: &str, symbol: &str) -> bool {
        // Fast path: check existing cached entry
        if let Some(entry) = self
            .filters
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .get(path)
            .filter(|entry| entry.mtime == mtime)
        {
            return entry.filter.contains(symbol);
        }

        // Cache miss or stale: build and cache a new filter
        let filter = build_filter(content, code_lang(path));
        let result = filter.contains(symbol);
        self.filters
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_path_buf(), Entry { filter, mtime }, self.budget);
        result
    }
}

impl Filters {
    /// Cache `entry` for `path` in place of any older one, keeping the
    /// filters within `budget`. A filter larger than the whole budget is not
    /// cached.
    fn insert(&mut self, path: PathBuf, entry: Entry, budget: usize) {
        if let Some(stale) = self.entries.remove(&path) {
            self.bytes -= entry_bytes(&path, &stale.filter);
        }
        let cost = entry_bytes(&path, &entry.filter);
        if cost > budget {
            return;
        }
        if self.bytes + cost > budget {
            self.entries.clear();
            self.bytes = 0;
        }
        self.bytes += cost;
        self.entries.insert(path, entry);
    }
}

/// The memory one cached filter holds: its bits, its path, and its slot in
/// the table. The table's spare slots are not counted.
fn entry_bytes(path: &Path, filter: &BloomFilter) -> usize {
    std::mem::size_of::<(PathBuf, Entry)>() + path.as_os_str().len() + filter.num_bits() / 8
}

/// The source language of `path`, or `None` when it is not a known code file.
fn code_lang(path: &Path) -> Option<Lang> {
    match detect_file_type(path) {
        FileType::Code(lang) => Some(lang),
        FileType::Markdown
        | FileType::StructuredData
        | FileType::Tabular
        | FileType::Log
        | FileType::Other => None,
    }
}

/// The seed of every filter. fastbloom draws a random seed per filter by
/// default, which makes false positives, and so which files a batch search
/// reads, differ between runs over the same tree. A fixed seed makes a
/// filter's answers a function of the content alone. The value spells
/// `tilth_bloom_seed` in ASCII.
const FILTER_SEED: u128 = 0x7469_6c74_685f_626c_6f6f_6d5f_7365_6564;

/// Build a Bloom filter from file content by extracting all identifiers.
fn build_filter(content: &str, lang: Option<Lang>) -> BloomFilter {
    let idents: Vec<&str> = extract_identifiers(content, lang).collect();
    // Sized for total token count, not unique identifiers -- duplicates over-allocate
    // the filter, so the achieved FPR is well below the 0.01 target in practice.
    let expected = idents.len().max(1);

    let mut filter = BloomFilter::with_false_pos(0.01)
        .seed(&FILTER_SEED)
        .expected_items(expected);
    for ident in idents {
        filter.insert(ident);
    }
    filter
}

// ---------------------------------------------------------------------------
// Identifier extraction (byte-level state machine)
// ---------------------------------------------------------------------------

/// Extract identifier tokens from source code using a simple byte-level
/// state machine. Skips string literals and block/line comments, but reads
/// the code in a string's holes (`${…}` in a JavaScript template, `#{…}` in
/// Ruby) as code, by the language's [`Interpolation`].
///
/// An identifier is a name by the language's [`NameRule`], the rule its
/// grammar reads names by, so the filter holds every name a parse of the
/// file can find (`save!` in Ruby, `größe` in Python). A file in no known
/// language is read by [`NameRule::PLAIN`].
///
/// This is intentionally approximate -- it does not understand all language
/// syntaxes perfectly, but is fast and good enough for Bloom filter population.
///
/// `lang` gates language-specific lexing: the Rust lifetime heuristic only
/// applies when `lang` is `Some(Lang::Rust)`. For every other language a `'`
/// opens a single-quoted string, matching their actual syntax.
/// Triple-quoted strings and `#` line comments are read as such only in the
/// languages that have them; read quote by quote, a quote or an apostrophe
/// inside one would swallow the identifiers that follow it.
fn extract_identifiers(content: &str, lang: Option<Lang>) -> impl Iterator<Item = &str> {
    IdentifierIter::new(content, lang)
}

/// States for the identifier extraction state machine.
#[derive(Clone, Copy)]
enum ScanState {
    /// Normal code scanning, also inside a string's hole.
    Code,
    /// Inside a string literal.
    Str(Quoted),
    /// Inside a line comment (`// ...`, or `# ...` where the language has it).
    LineComment,
    /// Inside a block comment (/* ... */).
    BlockComment,
}

/// A string literal: double-quoted, single-quoted, backtick (a JavaScript
/// template, a Go raw string) or triple-quoted.
#[derive(Clone, Copy)]
struct Quoted {
    /// The quote that closes it.
    quote: u8,
    /// Whether three of `quote` close it.
    triple: bool,
    /// Whether its holes are code, by the language's [`Interpolation`].
    holds_code: bool,
}

/// A hole the scan is in: code inside a string, up to the byte that
/// closes the hole.
struct OpenHole {
    /// The state to resume when the hole closes: the string it is in, or
    /// code for a shell's backtick outside a string.
    resume: ScanState,
    /// The byte that closes the hole.
    closes: u8,
    /// The brackets of the closing kind opened inside the hole and not yet
    /// closed.
    depth: usize,
}

/// The opening bracket of `close`, when it is a closing bracket.
const fn opening_bracket(close: u8) -> Option<u8> {
    match close {
        b'}' => Some(b'{'),
        b')' => Some(b'('),
        _ => None,
    }
}

struct IdentifierIter<'a> {
    bytes: &'a [u8],
    src: &'a str,
    pos: usize,
    state: ScanState,
    lang: Option<Lang>,
    names: NameRule,
    triple_quotes: bool,
    hash_comments: bool,
    interpolation: Option<Interpolation>,
    /// Whether a backtick in code opens a hole, as a shell's command
    /// substitution does inside a string and out.
    code_backticks: bool,
    /// The holes the scan is in, innermost last.
    holes: Vec<OpenHole>,
    /// The name just returned without its trailing mark, returned next.
    pending: Option<&'a str>,
}

impl<'a> IdentifierIter<'a> {
    fn new(content: &'a str, lang: Option<Lang>) -> Self {
        let interpolation = lang.and_then(Lang::interpolation);
        Self {
            bytes: content.as_bytes(),
            src: content,
            pos: 0,
            state: ScanState::Code,
            lang,
            names: lang.map_or(NameRule::PLAIN, Lang::names),
            triple_quotes: lang.is_some_and(Lang::has_triple_quoted_strings),
            hash_comments: lang.is_some_and(Lang::has_hash_comments),
            interpolation,
            code_backticks: interpolation
                .is_some_and(|rule| rule.holes.iter().any(|hole| hole.opens == b"`")),
            holes: Vec::new(),
            pending: None,
        }
    }

    /// Read the name that starts at `start`, leaving `pos` after it. A name
    /// that ends in a mark is returned with it, and the next call returns it
    /// without: the mark may be the first character of an operator
    /// (`count!=0`), which only a parser can tell.
    fn read_name(&mut self, start: usize) -> &'a str {
        let src = self.src;
        let names = self.names;
        let end = src[start..]
            .char_indices()
            .skip(1)
            .find(|&(_, c)| !names.continues(c))
            .map_or(src.len(), |(at, _)| start + at);
        self.pos = end;
        if let Some(mark) = src[end..].chars().next().filter(|&c| names.ends(c)) {
            self.pending = Some(&src[start..end]);
            self.pos += mark.len_utf8();
        }
        &src[start..self.pos]
    }

    /// Open the string whose opening quote, three of them when `triple`,
    /// starts at `at`, leaving `pos` after it.
    fn open_string(&mut self, at: usize, quote: u8, triple: bool) {
        let before = &self.bytes[..at];
        let holds_code = self.interpolation.is_some_and(|rule| {
            rule.quotes.contains(&quote) && rule.marked.is_none_or(|marked| marked(before))
        });
        self.state = ScanState::Str(Quoted {
            quote,
            triple,
            holds_code,
        });
        self.pos = at + if triple { 3 } else { 1 };
    }

    /// Step over the hole opener at `at` in the current string, if one
    /// starts there, and read what follows as code. A one-byte opener
    /// written twice is that byte as text (`{{` in a Python f-string), and
    /// is stepped over whole. Returns whether `pos` moved.
    fn enter_hole(&mut self, at: usize) -> bool {
        let rest = &self.bytes[at..];
        let Some(hole) = self
            .interpolation
            .and_then(|rule| rule.holes.iter().find(|hole| rest.starts_with(hole.opens)))
        else {
            return false;
        };
        if let [opener] = hole.opens {
            if rest.get(1) == Some(opener) {
                self.pos = at + 2;
                return true;
            }
        }
        self.holes.push(OpenHole {
            resume: self.state,
            closes: hole.closes,
            depth: 0,
        });
        self.state = ScanState::Code;
        self.pos = at + hole.opens.len();
        true
    }
}

impl<'a> Iterator for IdentifierIter<'a> {
    type Item = &'a str;

    #[expect(clippy::too_many_lines, reason = "split by responsibility in #315")]
    fn next(&mut self) -> Option<&'a str> {
        if let Some(bare) = self.pending.take() {
            return Some(bare);
        }
        let bytes = self.bytes;
        let len = bytes.len();

        while self.pos < len {
            let i = self.pos;
            let b = bytes[i];

            match self.state {
                ScanState::Code => {
                    // In a hole, its closing byte ends it, unless a bracket
                    // of the same kind opened inside it is still open.
                    if let Some(hole) = self.holes.last_mut() {
                        if b == hole.closes {
                            if hole.depth == 0 {
                                self.state = hole.resume;
                                self.holes.pop();
                                self.pos += 1;
                                continue;
                            }
                            hole.depth -= 1;
                        } else if Some(b) == opening_bracket(hole.closes) {
                            hole.depth += 1;
                        }
                    }

                    // A triple-quoted string first: read quote by quote, `"""` is
                    // an empty string and then an open one, and the first quote
                    // inside flips code and string for the rest of the file.
                    if self.triple_quotes
                        && (b == b'"' || b == b'\'')
                        && bytes[i..].starts_with(&[b, b, b])
                    {
                        self.open_string(i, b, true);
                        continue;
                    }

                    // A `#` opens a line comment where the language says so,
                    // also straight after code (`call()# it's`). It is code
                    // inside a word (`a#b`), in a shell expansion (`${#name}`,
                    // `$#`), as a character literal (`?#`), escaped (`\#`) and
                    // before `[` (an attribute). Misreading code as a comment
                    // loses one line; misreading a comment as code can lose
                    // the rest of the file, so the rule leans to comment. A hole
                    // holds no comment, for the reason given at `/` below.
                    if b == b'#'
                        && self.hash_comments
                        && self.holes.is_empty()
                        && (i == 0
                            || !(bytes[i - 1].is_ascii_alphanumeric()
                                || matches!(bytes[i - 1], b'_' | b'$' | b'{' | b'?' | b'\\')))
                        && !(i + 1 < len && bytes[i + 1] == b'[')
                    {
                        self.state = ScanState::LineComment;
                        self.pos += 1;
                        continue;
                    }

                    // Check for start of string literals
                    if b == b'"' {
                        self.open_string(i, b, false);
                        continue;
                    }
                    if b == b'\'' {
                        // Distinguish a Rust lifetime (`'a`, `'static`) from a char
                        // literal (`'a'`, `'\n'`). A char literal has a closing quote
                        // right after a single char/escape; a lifetime is a tick
                        // followed by an identifier with no closing quote. Treating a
                        // lifetime as a string opener would swallow every following
                        // identifier up to the next tick, dropping them from the filter
                        // and producing a false negative (the one thing Bloom forbids).
                        // Lifetimes are Rust-only; in other languages a `'` opens a
                        // single-quoted string, so the heuristic is gated on
                        // `has_lifetimes` to avoid swallowing identifiers after a
                        // `'foo'` string there.
                        let tick = self.src.get(i + 1..).and_then(|rest| rest.chars().next());
                        let is_lifetime = self.lang.is_some_and(Lang::has_lifetimes)
                            && tick.is_some_and(|c| {
                                self.names.starts(c)
                                    && bytes.get(i + 1 + c.len_utf8()) != Some(&b'\'')
                            });
                        if is_lifetime {
                            self.pos += 1;
                            continue;
                        }
                        self.open_string(i, b, false);
                        continue;
                    }
                    if b == b'`' {
                        if self.code_backticks {
                            self.holes.push(OpenHole {
                                resume: ScanState::Code,
                                closes: b,
                                depth: 0,
                            });
                            self.pos += 1;
                        } else {
                            self.open_string(i, b, false);
                        }
                        continue;
                    }

                    // Check for comments. A hole is one expression and holds
                    // none: read as a comment, a `//` (Python's floor division)
                    // or a shell's `/*` would swallow the hole's closing
                    // bracket, and the scan would lose its place in the file.
                    if b == b'/' && i + 1 < len && self.holes.is_empty() {
                        if bytes[i + 1] == b'/' {
                            self.state = ScanState::LineComment;
                            self.pos += 2;
                            continue;
                        }
                        if bytes[i + 1] == b'*' {
                            self.state = ScanState::BlockComment;
                            self.pos += 2;
                            continue;
                        }
                    }

                    // A name, or a character to step over. In code the scan
                    // stops only on a character boundary; stepping one byte
                    // keeps a position inside a character total.
                    let at = if b.is_ascii() {
                        Some(char::from(b))
                    } else {
                        self.src.get(i..).and_then(|rest| rest.chars().next())
                    };
                    let Some(c) = at else {
                        self.pos += 1;
                        continue;
                    };
                    if self.names.starts(c) {
                        return Some(self.read_name(i));
                    }
                    self.pos += c.len_utf8();
                }

                ScanState::Str(string) => {
                    if string.holds_code && self.enter_hole(i) {
                        continue;
                    }
                    if b == b'\\' && i + 1 < len {
                        self.pos += 2; // skip escaped character
                    } else if b == string.quote && !string.triple {
                        self.state = ScanState::Code;
                        self.pos += 1;
                    } else if b == string.quote && bytes[i..].starts_with(&[b, b, b]) {
                        self.state = ScanState::Code;
                        self.pos += 3;
                    } else {
                        self.pos += 1;
                    }
                }

                ScanState::LineComment => {
                    if b == b'\n' {
                        self.state = ScanState::Code;
                    }
                    self.pos += 1;
                }

                ScanState::BlockComment => {
                    if b == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                        self.state = ScanState::Code;
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                    }
                }
            }
        }

        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    impl BloomFilterCache {
        /// How many filters the cache holds, and the bytes they take.
        ///
        /// The bytes are summed from the filters themselves, and must match the
        /// count the cache keeps.
        fn held(&self) -> (usize, usize) {
            let filters = self.filters.read().unwrap_or_else(PoisonError::into_inner);
            let bytes = filters
                .entries
                .iter()
                .map(|(path, entry)| entry_bytes(path, &entry.filter))
                .sum();
            assert_eq!(filters.bytes, bytes, "the cache miscounts its bytes");
            (filters.entries.len(), bytes)
        }

        fn holds(&self, path: &Path) -> bool {
            self.filters
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .entries
                .contains_key(path)
        }
    }

    /// The path, content and one defined symbol of the `i`th file of a tree.
    fn nth_file(i: usize) -> (PathBuf, String, String) {
        let symbol = format!("present_{i}");
        let content = format!("fn {symbol}() {{}}\n");
        (PathBuf::from(format!("/tree/file_{i}.rs")), content, symbol)
    }

    /// A cache with room for about `files` filters like those of [`nth_file`].
    fn cache_with_room_for(files: usize) -> BloomFilterCache {
        let (path, content, _) = nth_file(0);
        let one = entry_bytes(&path, &build_filter(&content, Some(Lang::Rust)));
        BloomFilterCache::with_budget(one * files)
    }

    #[test]
    fn cache_stays_within_its_budget() {
        let cache = cache_with_room_for(10);
        for i in 0..100 {
            let (path, content, symbol) = nth_file(i);
            assert!(cache.contains(&path, SystemTime::UNIX_EPOCH, &content, &symbol));
            let (_, bytes) = cache.held();
            assert!(
                bytes <= cache.budget,
                "after {} files the cache holds {bytes} bytes, over its budget of {}",
                i + 1,
                cache.budget
            );
        }
        let (filters, _) = cache.held();
        assert!(filters <= 10, "the cache holds {filters} filters");
    }

    #[test]
    fn evicted_file_is_checked_again_from_its_content() {
        let cache = cache_with_room_for(10);
        let mtime = SystemTime::UNIX_EPOCH;
        for i in 0..100 {
            let (path, content, symbol) = nth_file(i);
            assert!(cache.contains(&path, mtime, &content, &symbol));
        }
        let (first, content, symbol) = nth_file(0);
        assert!(!cache.holds(&first), "the first file was not evicted");
        assert!(cache.contains(&first, mtime, &content, &symbol));
        assert!(!cache.contains(&first, mtime, &content, "present_1"));
    }

    #[test]
    fn test_basic_membership() {
        let mut bf = BloomFilter::with_false_pos(0.01).expected_items(100);
        bf.insert("foo");
        bf.insert("bar");
        bf.insert("baz");

        assert!(bf.contains("foo"));
        assert!(bf.contains("bar"));
        assert!(bf.contains("baz"));
    }

    #[test]
    fn extracts_identifiers_across_rust_lifetimes() {
        let src = "fn longest<'a>(x: &'a str, y: &'a str) -> &'a str { x }";
        let idents: Vec<&str> = extract_identifiers(src, Some(Lang::Rust)).collect();
        for want in ["fn", "longest", "x", "y", "str"] {
            assert!(
                idents.contains(&want),
                "lifetime tick swallowed identifier {want:?}; got {idents:?}"
            );
        }
    }

    #[test]
    fn char_literal_is_still_skipped() {
        let src = "let c = 'a'; let d = '\\n'; fn target() {}";
        let idents: Vec<&str> = extract_identifiers(src, Some(Lang::Rust)).collect();
        assert!(idents.contains(&"target"), "got {idents:?}");
        assert!(
            !idents.contains(&"a"),
            "char-literal body leaked: {idents:?}"
        );
    }

    #[test]
    fn non_rust_single_quote_string_does_not_swallow_following_idents() {
        // In JS/Python/Ruby/PHP a `'...'` is a string, not a Rust lifetime. The
        // lifetime heuristic must stay off for them: if it fired, the closing
        // quote of `'foo'` would open a spurious string that swallows every
        // identifier up to the next quote -- a Bloom false negative.
        let src = "let x = 'foo'; bar();";
        let idents: Vec<&str> = extract_identifiers(src, Some(Lang::JavaScript)).collect();
        assert!(
            idents.contains(&"bar"),
            "closing quote opened a swallowing string: {idents:?}"
        );
        assert!(idents.contains(&"let"), "got {idents:?}");
        assert!(idents.contains(&"x"), "got {idents:?}");
    }

    #[test]
    fn test_definitely_not_present() {
        let mut bf = BloomFilter::with_false_pos(0.01).expected_items(10);
        bf.insert("alpha");
        bf.insert("beta");
        bf.insert("gamma");

        // With only 3 items in a filter sized for 10 at 1% FPR,
        // these should almost certainly return false.
        let mut false_positives = 0;
        let test_items = [
            "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa", "lambda", "mu", "nu",
            "xi", "omicron", "pi", "rho", "sigma", "tau", "upsilon", "phi", "chi", "psi", "omega",
        ];
        for item in &test_items {
            if bf.contains(item) {
                false_positives += 1;
            }
        }
        // At most 1 false positive out of 21 items is generous
        assert!(
            false_positives <= 1,
            "too many false positives: {false_positives}/{}",
            test_items.len()
        );
    }

    #[test]
    fn test_false_positive_rate() {
        let n = 500;
        let mut bf = BloomFilter::with_false_pos(0.01).expected_items(n);

        // Insert N items
        for i in 0..n {
            bf.insert(&format!("item_{i}"));
        }

        // Verify all inserted items are found
        for i in 0..n {
            assert!(bf.contains(&format!("item_{i}")), "missing item_{i}");
        }

        // Test M random items that were NOT inserted
        let m = 10_000;
        let mut false_positives = 0;
        for i in 0..m {
            if bf.contains(&format!("notinserted_{i}")) {
                false_positives += 1;
            }
        }

        let fpr = f64::from(false_positives) / f64::from(m);
        // Target is 1%, allow up to 5% for statistical variance
        assert!(
            fpr < 0.05,
            "false positive rate too high: {fpr:.4} ({false_positives}/{m})"
        );
    }

    #[test]
    fn test_identifier_extraction() {
        let code = "fn foo(bar: Baz) { qux() }";
        let idents: Vec<&str> = extract_identifiers(code, Some(Lang::Rust)).collect();
        assert_eq!(idents, vec!["fn", "foo", "bar", "Baz", "qux"]);
    }

    #[test]
    fn test_identifier_extraction_skips_strings() {
        let code = r#"let x = "hello world"; let y = 42;"#;
        let idents: Vec<&str> = extract_identifiers(code, Some(Lang::Rust)).collect();
        assert!(idents.contains(&"let"));
        assert!(idents.contains(&"x"));
        assert!(idents.contains(&"y"));
        // "hello" and "world" are inside a string -- should be skipped
        assert!(!idents.contains(&"hello"));
        assert!(!idents.contains(&"world"));
    }

    #[test]
    fn test_identifier_extraction_skips_comments() {
        let code = "fn real() // fn fake()\n/* fn also_fake() */\nfn another()";
        let idents: Vec<&str> = extract_identifiers(code, Some(Lang::Rust)).collect();
        assert!(idents.contains(&"real"));
        assert!(idents.contains(&"another"));
        assert!(!idents.contains(&"fake"));
        assert!(!idents.contains(&"also_fake"));
    }

    #[test]
    fn test_identifier_extraction_underscores_and_numbers() {
        let code = "_private __dunder var_123 _0 a1b2c3";
        let idents: Vec<&str> = extract_identifiers(code, Some(Lang::Rust)).collect();
        assert_eq!(
            idents,
            vec!["_private", "__dunder", "var_123", "_0", "a1b2c3"]
        );
    }

    #[test]
    fn test_identifier_extraction_empty() {
        let idents: Vec<&str> = extract_identifiers("", Some(Lang::Rust)).collect();
        assert_eq!(idents, Vec::<&str>::new());
    }

    #[test]
    fn test_identifier_extraction_no_identifiers() {
        let idents: Vec<&str> = extract_identifiers("123 + 456 = 789", Some(Lang::Rust)).collect();
        assert_eq!(idents, Vec::<&str>::new());
    }

    #[test]
    fn test_cache_mtime_invalidation() {
        let cache = BloomFilterCache::new();
        let path = Path::new("/tmp/test_bloom.rs");

        let old_content = "fn old_function() {}";
        let new_content = "fn new_function() {}";

        let mtime_old = SystemTime::UNIX_EPOCH;
        let mtime_new = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);

        // Cache with old content
        assert!(cache.contains(path, mtime_old, old_content, "old_function"));
        assert!(!cache.contains(path, mtime_old, old_content, "new_function"));

        // Same mtime: should use cached filter (old content), even though
        // we pass new content -- the cache trusts the mtime.
        assert!(cache.contains(path, mtime_old, new_content, "old_function"));

        // Different mtime: should rebuild from new content
        assert!(cache.contains(path, mtime_new, new_content, "new_function"));
        assert!(!cache.contains(path, mtime_new, new_content, "old_function"));
        let (filters, _) = cache.held();
        assert_eq!(filters, 1, "the stale filter was kept");
    }

    #[test]
    fn test_identifier_extraction_escaped_strings() {
        let code = r#"let s = "escaped \"quote\" inside"; let t = 1;"#;
        let idents: Vec<&str> = extract_identifiers(code, Some(Lang::Rust)).collect();
        assert!(idents.contains(&"s"));
        assert!(idents.contains(&"t"));
        // "quote" and "inside" are inside the string -- should be skipped
        assert!(!idents.contains(&"quote"));
        assert!(!idents.contains(&"inside"));
    }

    #[test]
    fn test_identifier_extraction_single_quotes() {
        let code = "let c = 'a'; let d = 'b';";
        let idents: Vec<&str> = extract_identifiers(code, Some(Lang::Rust)).collect();
        assert!(idents.contains(&"let"));
        assert!(idents.contains(&"c"));
        assert!(idents.contains(&"d"));
    }

    /// A source of `n` distinct identifiers, enough to fill a filter to its target rate.
    fn many_identifiers(n: usize) -> String {
        use std::fmt::Write as _;
        let mut source = String::new();
        for i in 0..n {
            writeln!(source, "fn present_{i}() {{}}").expect("writing to a String cannot fail");
        }
        source
    }

    #[test]
    fn filter_answers_are_a_function_of_the_content() {
        // Two filters built from the same content must answer every question alike,
        // false positives included. With a random seed per filter they do not, and a
        // caller search over the same tree then returns different call sites on
        // different runs.
        let content = many_identifiers(2_000);
        let first = build_filter(&content, Some(Lang::Rust));
        let second = build_filter(&content, Some(Lang::Rust));
        let answers = |filter: &BloomFilter| -> Vec<bool> {
            (0..20_000)
                .map(|i| filter.contains(format!("absent_{i}").as_str()))
                .collect()
        };
        assert_eq!(
            answers(&first),
            answers(&second),
            "two filters over the same content disagree: the seed is not fixed"
        );
    }

    #[test]
    fn python_triple_quoted_string_does_not_swallow_following_idents() {
        // A quote inside a triple-quoted string must not flip the scanner, or the
        // apostrophe after it opens a string that swallows the rest of the file: a
        // Bloom false negative (from a real test file).
        let src = concat!(
            "def before():\n",
            "    pass\n",
            "\n",
            "FIXTURE = \"\"\"\n",
            "{\"prompt\": \"count ripgrep's lines\"}\n",
            "\"\"\"\n",
            "\n",
            "def after_the_apostrophe():\n",
            "    pass\n",
            "\n",
            "after_the_apostrophe()\n",
        );
        let idents: Vec<&str> = extract_identifiers(src, Some(Lang::Python)).collect();
        assert!(idents.contains(&"before"), "got {idents:?}");
        assert!(
            idents.contains(&"after_the_apostrophe"),
            "the triple-quoted string swallowed the code after it: {idents:?}"
        );
        assert!(
            !idents.contains(&"prompt"),
            "the body of the triple-quoted string leaked: {idents:?}"
        );
    }

    #[test]
    fn python_single_triple_quoted_string_is_one_string() {
        let src = "DOC = '''it's \"quoted\" here'''\ndef after():\n    pass\n";
        let idents: Vec<&str> = extract_identifiers(src, Some(Lang::Python)).collect();
        assert!(idents.contains(&"after"), "got {idents:?}");
        assert!(!idents.contains(&"quoted"), "got {idents:?}");
    }

    #[test]
    fn hash_comment_apostrophe_does_not_swallow_following_idents() {
        // `#` opens a line comment in Python, Ruby, shell and others. Read as code,
        // the apostrophe in "don't" opens a string that swallows what follows.
        let src = "# don't call this twice\ndef after_the_comment():\n    pass\n";
        for lang in [Lang::Python, Lang::Ruby, Lang::Bash, Lang::Elixir] {
            let idents: Vec<&str> = extract_identifiers(src, Some(lang)).collect();
            assert!(
                idents.contains(&"after_the_comment"),
                "{lang:?}: the comment's apostrophe swallowed the code after it: {idents:?}"
            );
            assert!(
                !idents.contains(&"twice"),
                "{lang:?}: the comment body leaked: {idents:?}"
            );
        }
    }

    #[test]
    fn hash_inside_a_word_is_not_a_comment() {
        // Shell `${#name}` and `$#` are code. Only a `#` that starts a word opens a
        // comment, so nothing after such a `#` is lost.
        let src = "count=${#items}; n=$#; url=a#b next_step\n";
        let idents: Vec<&str> = extract_identifiers(src, Some(Lang::Bash)).collect();
        assert!(idents.contains(&"next_step"), "got {idents:?}");
    }

    #[test]
    fn hash_comment_straight_after_code_is_a_comment() {
        // No space before the `#`: still a comment in Python, so its apostrophe
        // must not open a string that swallows the next definition.
        let src = "setup()# don't call this twice\ndef after_the_comment():\n    pass\n";
        let idents: Vec<&str> = extract_identifiers(src, Some(Lang::Python)).collect();
        assert!(idents.contains(&"after_the_comment"), "got {idents:?}");
        assert!(!idents.contains(&"twice"), "got {idents:?}");
    }

    #[test]
    fn rust_attribute_hash_is_not_a_comment() {
        let src = "#[derive(Debug)]\nstruct After;\n";
        let idents: Vec<&str> = extract_identifiers(src, Some(Lang::Rust)).collect();
        assert!(idents.contains(&"derive"), "got {idents:?}");
        assert!(idents.contains(&"After"), "got {idents:?}");
    }

    #[test]
    fn php_attribute_hash_is_not_a_comment() {
        let src = "<?php\n#[Route(path)]\nfunction after() {}\n";
        let idents: Vec<&str> = extract_identifiers(src, Some(Lang::Php)).collect();
        assert!(idents.contains(&"Route"), "got {idents:?}");
        assert!(idents.contains(&"after"), "got {idents:?}");
    }

    /// Each name is one token, spelled as its language spells it (#375).
    #[test]
    fn a_name_is_read_by_its_languages_rule() {
        for (lang, src, name) in [
            (Lang::Ruby, "order.persist!\n", "persist!"),
            (Lang::Ruby, "raise unless ok?\n", "ok?"),
            (Lang::Elixir, "order = fetch!(id)\n", "fetch!"),
            (Lang::Python, "return größe()\n", "größe"),
            (Lang::Rust, "let n = größe();\n", "größe"),
            (Lang::JavaScript, "return $http();\n", "$http"),
            (Lang::Java, "run$();\n", "run$"),
            (Lang::Bash, "main() {\n  my-helper\n}\n", "my-helper"),
        ] {
            let idents: Vec<&str> = extract_identifiers(src, Some(lang)).collect();
            assert!(
                idents.contains(&name),
                "{lang:?}: {name:?} not in {idents:?}"
            );
        }
    }

    /// A trailing mark may be an operator's first character (`count!=0`),
    /// which only a parser can tell, so the name is held both with and
    /// without it. A mark is never a name of its own.
    #[test]
    fn a_name_with_a_trailing_mark_is_also_held_without_it() {
        let idents: Vec<&str> = extract_identifiers("x = count!=0\n", Some(Lang::Ruby)).collect();
        assert_eq!(idents, ["x", "count!", "count"]);
    }

    /// The marks and extra characters are one language's: `$` is a sigil in
    /// PHP, and `!` is a Rust macro's, not part of its name.
    #[test]
    fn another_languages_name_characters_split_a_name() {
        let php: Vec<&str> = extract_identifiers("$order->save();\n", Some(Lang::Php)).collect();
        assert_eq!(php, ["order", "save"]);
        let rust: Vec<&str> = extract_identifiers("validate!(x);\n", Some(Lang::Rust)).collect();
        assert_eq!(rust, ["validate", "x"]);
    }

    /// A string's text is skipped, but the code in its holes is read as code,
    /// by each language's rule (#398). `text` stands in the string's text and
    /// `code` in a hole.
    #[test]
    fn interpolated_code_is_read_as_code() {
        for (lang, src) in [
            (Lang::JavaScript, "s = `text ${code(x)} text`;\n"),
            (Lang::TypeScript, "s = `text ${code(x)} text`;\n"),
            (Lang::Tsx, "s = `text ${code(x)} text`;\n"),
            (Lang::Ruby, "s = \"text #{code(x)} text\"\n"),
            (Lang::Ruby, "s = `text #{code(x)} text`\n"),
            (Lang::Elixir, "s = \"text #{code(x)} text\"\n"),
            (Lang::Elixir, "s = \"\"\"\ntext #{code(x)} text\n\"\"\"\n"),
            (Lang::Elixir, "s = 'text #{code(x)} text'\n"),
            (Lang::Kotlin, "val s = \"text ${code(x)} text\"\n"),
            (Lang::Kotlin, "val s = \"\"\"text ${code(x)} text\"\"\"\n"),
            (Lang::Scala, "val s = s\"text ${code(x)} text\"\n"),
            (Lang::Scala, "val s = raw\"\"\"text ${code(x)} text\"\"\"\n"),
            (Lang::Swift, "let s = \"text \\(code(x)) text\"\n"),
            (
                Lang::Swift,
                "let s = \"\"\"\ntext \\(code(x)) text\n\"\"\"\n",
            ),
            (Lang::Python, "s = f\"text {code(x)} text\"\n"),
            (Lang::Python, "s = rf'text {code(x)} text'\n"),
            (Lang::Python, "s = F\"\"\"text {code(x)} text\"\"\"\n"),
            (Lang::CSharp, "var s = $\"text {code(x)} text\";\n"),
            (Lang::CSharp, "var s = $@\"text {code(x)} text\";\n"),
            (Lang::Bash, "s=\"text $(code x) text\"\n"),
            (Lang::Bash, "s=\"text `code x` text\"\n"),
            (Lang::Bash, "s=`code x`\n"),
            (Lang::Php, "$s = \"text {$code->x()} text\";\n"),
        ] {
            let idents: Vec<&str> = extract_identifiers(src, Some(lang)).collect();
            assert!(
                idents.contains(&"code"),
                "{lang:?}: the hole in {src:?} was skipped: {idents:?}"
            );
            assert!(
                !idents.contains(&"text"),
                "{lang:?}: the text of {src:?} leaked: {idents:?}"
            );
        }
    }

    /// A hole is code to its own closing bracket: a bracket or a string
    /// opened inside it, even a string with holes of its own, closes inside
    /// it, and the string resumes after it.
    #[test]
    fn a_hole_closes_at_its_own_bracket() {
        for (lang, src) in [
            (
                Lang::JavaScript,
                "s = `${ {k: inner}.k } text ${`text ${deep()}`} text`; after();\n",
            ),
            (
                Lang::Ruby,
                "s = \"#{ {k: inner}[:k] } text #{\"text #{deep}\"} text\"; after\n",
            ),
            (
                Lang::Python,
                "s = f\"{ {'k': inner}['k'] } text {f'text {deep()}'} text\"; after()\n",
            ),
            (
                Lang::Swift,
                "let s = \"\\((inner)) text \\(\"text \\(deep())\") text\"; after()\n",
            ),
            (
                Lang::Bash,
                "s=\"$( (inner) ) text $(echo \"text $(deep)\") text\"; after\n",
            ),
        ] {
            let idents: Vec<&str> = extract_identifiers(src, Some(lang)).collect();
            for name in ["inner", "deep", "after"] {
                assert!(
                    idents.contains(&name),
                    "{lang:?}: {name:?} not in {idents:?} from {src:?}"
                );
            }
            assert!(
                !idents.contains(&"text"),
                "{lang:?}: the text of {src:?} leaked: {idents:?}"
            );
        }
    }

    /// A hole is one expression and holds no comment. Read as comments,
    /// Python's floor division `//` or a shell's `#` and `/*` would swallow
    /// the hole's closing bracket, and the scan would lose its place for the
    /// rest of the file.
    #[test]
    fn a_hole_holds_no_comment() {
        for (lang, src) in [
            (Lang::Python, "s = f\"{a // inner} text\"; after()\n"),
            (
                Lang::Bash,
                "s=\"$(inner ${p##*/}) $(dirname ${p%/*}) text\"; after\n",
            ),
        ] {
            let idents: Vec<&str> = extract_identifiers(src, Some(lang)).collect();
            for name in ["inner", "after"] {
                assert!(
                    idents.contains(&name),
                    "{lang:?}: {name:?} not in {idents:?} from {src:?}"
                );
            }
            assert!(
                !idents.contains(&"text"),
                "{lang:?}: the text of {src:?} leaked: {idents:?}"
            );
        }
    }

    /// Text that only looks like a hole stays text: an escaped opener, a
    /// doubled brace in a format string, and a string the language does not
    /// interpolate. Read as a hole, a lone `{` in such a string would leave
    /// the scan in code at its closing quote, and the next quote would open a
    /// string that swallows the rest of the file.
    #[test]
    fn text_that_only_looks_like_a_hole_stays_text() {
        for (lang, src) in [
            (Lang::JavaScript, "s = `\\${text}`; after();\n"),
            (Lang::Ruby, "s = \"\\#{text}\" + 'it#{text}'; after\n"),
            (Lang::Kotlin, "val s = \"\\${text}\"; after()\n"),
            (Lang::Swift, "let s = \"\\\\(text)\"; after()\n"),
            (
                Lang::Python,
                "s = f\"{{text}}\" + \"{\" + r'{' + '{'; after()\n",
            ),
            (Lang::Python, "if\"{\" in s: after()\n"),
            (
                Lang::CSharp,
                "var s = $\"{{text}}\" + \"{\" + @\"{\"; after();\n",
            ),
            (Lang::Scala, "val s = \"${\" + \"$\"; after()\n"),
            (Lang::Go, "s := `${text}`; after()\n"),
            (Lang::Rust, "let s = format!(\"{text}\"); after();\n"),
            (Lang::Bash, "s='$(text)'; after\n"),
            (Lang::Bash, "s=\"${text##*/} ${text%/*}\"; after\n"),
            (Lang::Php, "$s = \"${text}\"; after();\n"),
            (Lang::Php, "$s = '{$text}'; after();\n"),
        ] {
            let idents: Vec<&str> = extract_identifiers(src, Some(lang)).collect();
            assert!(
                idents.contains(&"after"),
                "{lang:?}: {src:?} swallowed the code after it: {idents:?}"
            );
            assert!(
                !idents.contains(&"text"),
                "{lang:?}: the text of {src:?} leaked: {idents:?}"
            );
        }
    }

    #[test]
    fn test_build_filter_integration() {
        let content = "pub fn search(query: &str) -> Vec<Match> { find(query) }";
        let filter = build_filter(content, Some(Lang::Rust));

        assert!(filter.contains("search"));
        assert!(filter.contains("query"));
        assert!(filter.contains("Vec"));
        assert!(filter.contains("Match"));
        assert!(filter.contains("find"));
        assert!(!filter.contains("nonexistent_symbol_xyz"));
    }
}
