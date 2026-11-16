//! Ruby language spec. Diverges on: callee filtering, because Ruby calls a
//! method without parentheses.

use tree_sitter::Node;

use crate::lang::spec::{LangSpec, NameRule, StdlibRule, DEFAULT_DEFS, DEFAULT_DEF_KINDS};

/// Every identifier: `callee_is_call` decides which ones call a method.
const CALLEE_QUERY: &str = "(identifier) @callee\n";

/// Nodes that start a scope with no locals from outside it.
const GATES: &[&str] = &[
    "program",
    "method",
    "singleton_method",
    "class",
    "singleton_class",
    "module",
];

/// Nodes that start a scope and also see the locals around them.
const BLOCKS: &[&str] = &["block", "do_block", "lambda"];

/// What a bare identifier does where it stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    /// The method of a `call`: `order.total`, `compute_tax subtotal`, `tax()`.
    Call,
    /// Binds a local: a parameter, an assignment's target, a pattern's variable.
    Binds,
    /// Names a method or a local without calling it: `def tax`, `alias`, `^x`.
    Names,
    /// Reads a local, or calls a method with no receiver and no arguments.
    Reads,
}

/// How the identifier `id` is used, read from its place in the tree.
fn use_of(id: Node) -> Use {
    let Some(parent) = id.parent() else {
        return Use::Reads;
    };
    let is = |field| parent.child_by_field_name(field) == Some(id);
    match parent.kind() {
        "call" if is("method") => Use::Call,
        "method" | "singleton_method" if is("name") => Use::Names,
        "setter" | "alias" | "undef" | "variable_reference_pattern" => Use::Names,
        "assignment" | "operator_assignment" if is("left") => Use::Binds,
        "keyword_parameter" | "optional_parameter" | "as_pattern" if is("name") => Use::Binds,
        "keyword_pattern" if is("value") => Use::Binds,
        "for" | "in_clause" | "match_pattern" | "test_pattern" if is("pattern") => Use::Binds,
        "method_parameters"
        | "lambda_parameters"
        | "block_parameters"
        | "destructured_parameter"
        | "splat_parameter"
        | "hash_splat_parameter"
        | "block_parameter"
        | "exception_variable"
        | "left_assignment_list"
        | "rest_assignment"
        | "destructured_left_assignment"
        | "array_pattern"
        | "find_pattern" => Use::Binds,
        _ => Use::Reads,
    }
}

/// Whether `node` binds a local: an identifier that [`use_of`] says binds, or
/// the key of a pattern that binds its own name (`in {total:}`).
fn binds(node: Node) -> bool {
    match node.kind() {
        "identifier" => use_of(node) == Use::Binds,
        "hash_key_symbol" => node.parent().is_some_and(|p| {
            p.kind() == "keyword_pattern" && p.child_by_field_name("value").is_none()
        }),
        _ => false,
    }
}

/// The nearest ancestor of `node` whose kind `starts_scope`, or the root.
fn scope_of<'t>(node: Node<'t>, starts_scope: impl Fn(&str) -> bool) -> Node<'t> {
    let mut scope = node;
    while let Some(parent) = scope.parent() {
        scope = parent;
        if starts_scope(scope.kind()) {
            break;
        }
    }
    scope
}

/// Whether a bare read is a local variable, by the rule Ruby's own parser
/// applies: a name is a local from the first binding of it in its scope to
/// the scope's end. A block or lambda sees the locals around it, and its own
/// bindings end with it; `def`, `class` and `module` start afresh.
fn is_local(read: Node, source: &[u8]) -> bool {
    let name = &source[read.byte_range()];
    let gate = scope_of(read, |kind| GATES.contains(&kind));
    let start = gate.start_byte();
    memchr::memmem::find_iter(&source[start..read.start_byte()], name).any(|at| {
        let range = start + at..start + at + name.len();
        let whole_word = !source[..range.start].last().is_some_and(|&b| name_byte(b))
            && !source.get(range.end).is_some_and(|&b| name_byte(b));
        whole_word
            && gate
                .named_descendant_for_byte_range(range.start, range.end)
                .is_some_and(|node| {
                    node.byte_range() == range && binds(node) && in_scope(node, read)
                })
    })
}

/// Whether Ruby's lexer reads `b` as part of a name: ASCII letters, digits
/// and `_`, and every byte of a non-ASCII character.
fn name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || !b.is_ascii()
}

/// Whether a read may name a block's implicit parameter: `_1` to `_9`, or
/// `it` since Ruby 3.4. Which Ruby a file targets is not in the file, so a
/// bare `it` in a block is not claimed as a call.
fn is_implicit_parameter(read: Node, source: &[u8]) -> bool {
    let name = &source[read.byte_range()];
    (name == b"it" || matches!(name, [b'_', b'1'..=b'9']))
        && std::iter::successors(read.parent(), Node::parent).any(|n| BLOCKS.contains(&n.kind()))
}

/// Whether `read` lies inside the scope `binding` binds its name in.
fn in_scope(binding: Node, read: Node) -> bool {
    let scope = scope_of(binding, |kind| {
        GATES.contains(&kind) || BLOCKS.contains(&kind)
    });
    std::iter::successors(Some(read), Node::parent).any(|n| n == scope)
}

/// Whether a captured identifier calls a method. tree-sitter-ruby reads a call
/// with no receiver, arguments or parentheses (`subtotal + tax`) as a bare
/// `identifier`, the same node as a local variable read, so the scope decides.
///
/// A local bound where the tree does not show it (a regex's named groups)
/// reads as a call; a key shorthand argument (`f(total:)`) is a call the
/// grammar keeps as a symbol, so it is not seen.
fn callee_is_call(node: &Node, source: &[u8]) -> bool {
    match use_of(*node) {
        Use::Call => true,
        Use::Reads => !is_local(*node, source) && !is_implicit_parameter(*node, source),
        Use::Binds | Use::Names => false,
    }
}

pub(crate) const SPEC: LangSpec = LangSpec {
    display: "Ruby",
    extensions: &["rb"],
    filenames: &["Vagrantfile", "Rakefile"],
    grammar: Some(tree_sitter_ruby::LANGUAGE),
    callee_query: Some(CALLEE_QUERY),
    sibling_query: None,
    stdlib: StdlibRule::None,
    manifests: &[],
    definition_kinds: DEFAULT_DEF_KINDS,
    has_lifetimes: false,
    triple_quoted_strings: false,
    hash_comments: true,
    names: NameRule {
        letters: &[],
        marks: &['?', '!'],
    },
    strip_family: None,
    extract_receiver: None,
    callee_filter: Some(callee_is_call),
    test_site: None,
    definitions: DEFAULT_DEFS,
};
