//! C# language spec.

use crate::lang::spec::{
    Hole, Interpolation, LangSpec, NameRule, StdlibRule, StripFamily, DEFAULT_DEFS,
    DEFAULT_DEF_KINDS,
};

const CALLEE_QUERY: &str = concat!(
    "(invocation_expression function: (identifier) @callee)\n",
    "(invocation_expression function: (member_access_expression name: (identifier) @callee))\n",
);

const SIBLING_QUERY: &str = concat!(
    "(member_access_expression expression: (this_expression) name: (identifier) @ref)\n",
    "(invocation_expression function: (member_access_expression expression: (this_expression) name: (identifier) @ref))\n",
);

/// `{…}` in an interpolated string: `$"…"`, `$@"…"`, `@$"…"`.
const INTERPOLATION: Interpolation = Interpolation {
    quotes: b"\"",
    marked: Some(follows_a_dollar),
    holes: &[Hole {
        opens: b"{",
        closes: b'}',
    }],
};

/// Whether the `$` and `@` that end `before` hold a `$`.
fn follows_a_dollar(before: &[u8]) -> bool {
    before
        .iter()
        .rev()
        .take_while(|&&b| matches!(b, b'$' | b'@'))
        .any(|&b| b == b'$')
}

pub(crate) const SPEC: LangSpec = LangSpec {
    display: "C#",
    extensions: &["cs"],
    filenames: &[],
    grammar: Some(tree_sitter_c_sharp::LANGUAGE),
    callee_query: Some(CALLEE_QUERY),
    sibling_query: Some(SIBLING_QUERY),
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
