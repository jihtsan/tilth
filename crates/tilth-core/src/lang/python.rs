//! Python language spec. Diverges on: stdlib first-segment rule (`.` separator).

use crate::lang::spec::{
    Hole, Interpolation, LangSpec, NameRule, StdlibRule, StripFamily, DEFAULT_DEFS,
    DEFAULT_DEF_KINDS,
};

const CALLEE_QUERY: &str = concat!(
    "(call function: (identifier) @callee)\n",
    "(call function: (attribute attribute: (identifier) @callee))\n",
);

const SIBLING_QUERY: &str = "(attribute object: (identifier) @obj attribute: (identifier) @ref)\n";

/// Common stdlib modules — not exhaustive, but covers the noisy ones.
const STDLIB_MODULES: &[&str] = &[
    "os",
    "sys",
    "re",
    "json",
    "math",
    "time",
    "datetime",
    "pathlib",
    "typing",
    "collections",
    "functools",
    "itertools",
    "abc",
    "io",
    "logging",
    "unittest",
    "dataclasses",
    "enum",
    "copy",
    "hashlib",
    "subprocess",
    "threading",
    "asyncio",
];

/// `{…}` in an f-string or a t-string.
const INTERPOLATION: Interpolation = Interpolation {
    quotes: b"\"'",
    marked: Some(ends_in_a_format_prefix),
    holes: &[Hole {
        opens: b"{",
        closes: b'}',
    }],
};

/// Whether `before` ends in the prefix of a string that holds code: `f`,
/// `t`, or either with `r` (`rf`, `Tr`). A plain, raw or bytes string holds
/// none, and neither does a string after another name (`if"{"`).
fn ends_in_a_format_prefix(before: &[u8]) -> bool {
    let start = before
        .iter()
        .rposition(|&b| !(b.is_ascii_alphanumeric() || b == b'_' || !b.is_ascii()))
        .map_or(0, |at| at + 1);
    let prefix = &before[start..];
    prefix.len() <= 2
        && prefix.iter().all(|b| b"rRfFtT".contains(b))
        && prefix.iter().any(|b| b"fFtT".contains(b))
}

pub(crate) const SPEC: LangSpec = LangSpec {
    display: "Python",
    extensions: &["py", "pyi"],
    filenames: &[],
    grammar: Some(tree_sitter_python::LANGUAGE),
    callee_query: Some(CALLEE_QUERY),
    sibling_query: Some(SIBLING_QUERY),
    stdlib: StdlibRule::PythonSegment(STDLIB_MODULES),
    manifests: &["pyproject.toml", "setup.py"],
    definition_kinds: DEFAULT_DEF_KINDS,
    has_lifetimes: false,
    triple_quoted_strings: true,
    hash_comments: true,
    names: NameRule::PLAIN,
    interpolation: Some(INTERPOLATION),
    strip_family: Some(StripFamily::Python),
    extract_receiver: None,
    callee_filter: None,
    test_site: None,
    definitions: DEFAULT_DEFS,
};
