//! Bash language spec.

use crate::lang::spec::{
    Hole, Interpolation, LangSpec, NameRule, StdlibRule, StripFamily, DEFAULT_DEFS,
    DEFAULT_DEF_KINDS,
};

const CALLEE_QUERY: &str = "(command name: (command_name) @callee)\n";

/// The command substitutions, `$(…)` and `` `…` ``, in a double-quoted
/// string. A backtick is one outside a string too. A parameter expansion's
/// `${…}` is no hole: its operators (`${path%/*}`, `${name##*/}`) are not
/// code, and a call in it stands in a command substitution of its own.
const INTERPOLATION: Interpolation = Interpolation {
    quotes: b"\"",
    marked: None,
    holes: &[
        Hole {
            opens: b"$(",
            closes: b')',
        },
        Hole {
            opens: b"`",
            closes: b'`',
        },
    ],
};

pub(crate) const SPEC: LangSpec = LangSpec {
    display: "Bash",
    extensions: &["sh", "bash", "bats"],
    filenames: &[".bashrc", ".bash_profile", ".bash_aliases", ".profile"],
    grammar: Some(tree_sitter_bash::LANGUAGE),
    callee_query: Some(CALLEE_QUERY),
    sibling_query: None,
    stdlib: StdlibRule::None,
    manifests: &[],
    definition_kinds: DEFAULT_DEF_KINDS,
    has_lifetimes: false,
    triple_quoted_strings: false,
    hash_comments: true,
    names: NameRule {
        letters: &['-'],
        marks: &[],
    },
    interpolation: Some(INTERPOLATION),
    strip_family: Some(StripFamily::Bash),
    extract_receiver: None,
    callee_filter: None,
    test_site: None,
    definitions: DEFAULT_DEFS,
};
