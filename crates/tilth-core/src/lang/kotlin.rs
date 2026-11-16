//! Kotlin language spec.

use crate::lang::spec::{
    Hole, Interpolation, LangSpec, NameRule, StdlibRule, StripFamily, DEFAULT_DEFS,
    DEFAULT_DEF_KINDS,
};

const CALLEE_QUERY: &str = concat!(
    "(call_expression (identifier) @callee)\n",
    "(call_expression (navigation_expression (identifier) @callee .))\n",
);

/// `${…}` in a string template, raw or not.
const INTERPOLATION: Interpolation = Interpolation {
    quotes: b"\"",
    marked: None,
    holes: &[Hole {
        opens: b"${",
        closes: b'}',
    }],
};

pub(crate) const SPEC: LangSpec = LangSpec {
    display: "Kotlin",
    extensions: &["kt", "kts"],
    filenames: &[],
    grammar: Some(tree_sitter_kotlin_ng::LANGUAGE),
    callee_query: Some(CALLEE_QUERY),
    sibling_query: None,
    stdlib: StdlibRule::None,
    manifests: &[],
    definition_kinds: DEFAULT_DEF_KINDS,
    has_lifetimes: false,
    triple_quoted_strings: true,
    hash_comments: false,
    names: NameRule::PLAIN,
    interpolation: Some(INTERPOLATION),
    strip_family: Some(StripFamily::JavaKotlinCSharp),
    extract_receiver: None,
    callee_filter: None,
    test_site: None,
    definitions: DEFAULT_DEFS,
};
