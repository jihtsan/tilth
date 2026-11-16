//! Scala language spec.

use crate::lang::spec::{
    Hole, Interpolation, LangSpec, NameRule, StdlibRule, StripFamily, DEFAULT_DEFS,
    DEFAULT_DEF_KINDS,
};

const CALLEE_QUERY: &str = concat!(
    "(call_expression function: (identifier) @callee)\n",
    "(call_expression function: (field_expression field: (identifier) @callee))\n",
    "(infix_expression operator: (identifier) @callee)\n",
);

const SIBLING_QUERY: &str = concat!(
    "(field_expression (identifier) @obj (identifier) @ref)\n",
    "(call_expression function: (field_expression (identifier) @obj (identifier) @ref))\n",
);

/// `${…}` in a processed string: `s"…"`, `f"…"`, `raw"…"`, or any other
/// interpolator's.
const INTERPOLATION: Interpolation = Interpolation {
    quotes: b"\"",
    marked: Some(follows_an_interpolator),
    holes: &[Hole {
        opens: b"${",
        closes: b'}',
    }],
};

/// Whether `before` ends in a name, the interpolator a processed string
/// opens with.
fn follows_an_interpolator(before: &[u8]) -> bool {
    before
        .last()
        .is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'_' || !b.is_ascii())
}

pub(crate) const SPEC: LangSpec = LangSpec {
    display: "Scala",
    extensions: &["scala", "sc"],
    filenames: &[],
    grammar: Some(tree_sitter_scala::LANGUAGE),
    callee_query: Some(CALLEE_QUERY),
    sibling_query: Some(SIBLING_QUERY),
    stdlib: StdlibRule::None,
    manifests: &["build.sbt"],
    definition_kinds: DEFAULT_DEF_KINDS,
    has_lifetimes: false,
    triple_quoted_strings: true,
    hash_comments: false,
    names: NameRule {
        letters: &['$'],
        marks: &[],
    },
    interpolation: Some(INTERPOLATION),
    strip_family: Some(StripFamily::JavaKotlinCSharp),
    extract_receiver: None,
    callee_filter: None,
    test_site: None,
    definitions: DEFAULT_DEFS,
};
