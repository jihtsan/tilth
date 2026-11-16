//! Imports as data, one per module a statement names, and their resolution
//! to local file paths (the MCP layer's related-file hint, deps, callees).

use std::path::{Path, PathBuf};

use streaming_iterator::StreamingIterator;

use crate::lang::detect_file_type_with_content;
use crate::lang::outline::outline_language;
use crate::lang::treesitter::compiled_query;
use crate::types::{FileType, Lang};

/// The local files a code file's imports resolve to, read through `cache`'s
/// parsed tree so an outlined read does not parse the file a second time.
/// A Rust file's `mod` declarations count as imports of their files.
/// Returns empty for non-code files, files with no imports, or when all
/// imports are external.
#[must_use]
pub fn resolve_related_files(file_path: &Path, cache: &crate::cache::OutlineCache) -> Vec<PathBuf> {
    // The path alone says whether a file is code, so a log or data file of
    // any size is never read just to find it names no imports.
    if !matches!(crate::lang::detect_file_type(file_path), FileType::Code(_)) {
        return Vec::new();
    }
    if let Some(parsed) = cache.get_or_parse(file_path) {
        let references = references_in_tree(parsed.tree.root_node(), &parsed.content, parsed.lang);
        return resolve_references(file_path, parsed.lang, &references);
    }
    // The cache holds no tree for this file (no grammar, or over its size
    // cap): read it and parse here.
    let Ok(Some(content)) = crate::source::read_source(file_path) else {
        return Vec::new();
    };
    resolve_related_files_with_content(file_path, &content)
}

/// Same as `resolve_related_files` but takes pre-read content to avoid a redundant file read.
#[must_use]
pub fn resolve_related_files_with_content(file_path: &Path, content: &str) -> Vec<PathBuf> {
    let FileType::Code(lang) = detect_file_type_with_content(file_path, content) else {
        return Vec::new();
    };
    resolve_references(file_path, lang, &references(content, lang))
}

/// Resolve the local imports of the file at `file_path` to existing files,
/// each once, in import order. For a caller that already holds the file's
/// imports.
#[must_use]
pub fn resolve_imports(file_path: &Path, lang: Lang, imports: &[Import]) -> Vec<PathBuf> {
    first_of_each(
        imports
            .iter()
            .flat_map(|import| resolve_import(file_path, lang, import)),
    )
}

/// The files `references` of the file at `file_path` name, each once, in
/// the order they are named.
fn resolve_references(file_path: &Path, lang: Lang, references: &[Reference]) -> Vec<PathBuf> {
    first_of_each(
        references
            .iter()
            .flat_map(|reference| reference.resolve(file_path, lang)),
    )
}

/// `paths` without repeats, each where it first appears.
fn first_of_each(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in paths {
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// The local files one import of the file at `file_path` names, empty when
/// it names none. A Python absolute import (`from pkg.mod import x`) is
/// local when `pkg/mod.py` sits where Python would look for it (see
/// `resolve_python_absolute`); `is_external` cannot tell that without the
/// disk. `from pkg import a, b` names the submodule `pkg/a.py` when there is
/// one, and `pkg` itself for a name that is no submodule. A Rust import
/// resolves by the module tree (see `resolve_rust_use`), read as a statement
/// at the file's top level: the inline modules around a statement are in
/// the syntax tree, which `resolve_related_files_with_content` reads.
#[must_use]
pub fn resolve_import(file_path: &Path, lang: Lang, import: &Import) -> Vec<PathBuf> {
    resolve_import_within(file_path, lang, import, &[])
}

/// [`resolve_import`] for a statement inside the Rust inline modules
/// `within`, outermost first.
fn resolve_import_within(
    file_path: &Path,
    lang: Lang,
    import: &Import,
    within: &[String],
) -> Vec<PathBuf> {
    let Some(dir) = file_path.parent() else {
        return Vec::new();
    };
    if is_external(&import.source, lang) && lang != Lang::Python {
        return Vec::new();
    }
    if lang == Lang::Rust {
        return resolve_rust_use(file_path, &import.source, within)
            .map(|p| normalize_path(&p))
            .into_iter()
            .collect();
    }
    if import.names.is_empty() {
        return resolve(dir, &import.source, lang).into_iter().collect();
    }
    let mut files = Vec::new();
    let mut names_the_module = false;
    for name in &import.names {
        // `from . import x` names `.x`; `from pkg import x` names `pkg.x`.
        let submodule = if import.source.ends_with('.') {
            format!("{}{name}", import.source)
        } else {
            format!("{}.{name}", import.source)
        };
        match resolve(dir, &submodule, lang) {
            Some(file) if !files.contains(&file) => files.push(file),
            Some(_) => {}
            None => names_the_module = true,
        }
    }
    if names_the_module {
        if let Some(file) = resolve(dir, &import.source, lang) {
            if !files.contains(&file) {
                files.push(file);
            }
        }
    }
    files
}

/// One module an import statement names.
///
/// A statement can name several modules: a Go `import ( … )` block, a Rust
/// `use a::{b, c}` group, Python's `import os, json`, a Scala
/// `import a.{b, c}`, an Elixir `alias A.{B, C}`. Each module is its own
/// `Import`, on the line that names it, and carries the lines of the
/// statement it sits in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The module as the file spells it, without an alias: `example.com/m/core`,
    /// `crate::seams::git`, `json`, `./core`, `"util.h"`. `is_external` reads it.
    pub source: String,
    /// The 1-based line on which the statement names this module.
    pub line: u32,
    /// The 1-based first line of the statement.
    pub start_line: u32,
    /// The 1-based last line of the statement, inclusive.
    pub end_line: u32,
    /// What a Python `from m import a, b` takes from `m`: `a` and `b`, each
    /// a name in `m` or a submodule of it. Empty for every other import, and
    /// for `from m import *`.
    pub names: Vec<String>,
}

/// A Rust `mod name;`: a module whose code is in a file of its own, which
/// the declaring file makes part of its crate. By Rust's rules the file is
/// `name.rs` or `name/mod.rs` in the declaring module's directory, or the
/// one a `#[path = "…"]` attribute names. An inline `mod name { … }` is no
/// declaration of this kind.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModDecl {
    /// The module's name as the file spells it: `engine`, `r#type`.
    pub name: String,
    /// The file a `#[path = "…"]` attribute on the declaration names, as
    /// written.
    pub path_attr: Option<String>,
    /// The inline modules (`mod a { … }`) the declaration sits in,
    /// outermost first. Empty at a file's top level.
    pub within: Vec<String>,
    /// The 1-based line on which the declaration names the module.
    pub line: u32,
}

/// Every `mod name;` declaration in the Rust source `content`, in source
/// order. [`extract_imports`] gives the `use` statements;
/// [`resolve_related_files_with_content`] resolves both kinds.
#[must_use]
pub fn rust_mod_declarations(content: &str) -> Vec<ModDecl> {
    references(content, Lang::Rust)
        .into_iter()
        .filter_map(|reference| match reference {
            Reference::Mod(decl) => Some(decl),
            Reference::Import { .. } => None,
        })
        .collect()
}

/// One module a file names, with what resolving it to a file needs.
pub(crate) enum Reference {
    /// An import, with the Rust inline modules its statement sits in,
    /// outermost first. Empty at a file's top level and in other languages.
    Import { import: Import, within: Vec<String> },
    /// A Rust `mod` declaration.
    Mod(ModDecl),
}

impl Reference {
    /// The local files this reference of the file at `file_path` names.
    pub(crate) fn resolve(&self, file_path: &Path, lang: Lang) -> Vec<PathBuf> {
        match self {
            Self::Import { import, within } => {
                resolve_import_within(file_path, lang, import, within)
            }
            Self::Mod(decl) => resolve_rust_mod(file_path, decl)
                .map(|p| normalize_path(&p))
                .into_iter()
                .collect(),
        }
    }

    /// The 1-based first and last lines of the statement or declaration.
    pub(crate) fn lines(&self) -> (u32, u32) {
        match self {
            Self::Import { import, .. } => (import.start_line, import.end_line),
            Self::Mod(decl) => (decl.line, decl.line),
        }
    }
}

/// Every module `content` imports, one [`Import`] per module, in source order.
///
/// In Go, Rust, Python, JavaScript, TypeScript, Scala, Elixir, Java,
/// Kotlin, PHP, C# and Swift, statements come from the syntax tree, so an
/// import-shaped line inside a string is not one and an alias is not part
/// of a source. A JavaScript or TypeScript re-export (`export … from "m"`)
/// imports `m` too. A Rust `mod m;` is no statement; see
/// [`rust_mod_declarations`]. In C,
/// C++ and Bash, where an import is one line naming one file, each line
/// `is_import_line` accepts gives one import, its source read by
/// `extract_import_source`. A statement that does not spell its module
/// out, such as a shell `source "$dir/lib.sh"`, gives none. Languages without
/// imports tilth reads give none.
#[must_use]
pub fn extract_imports(content: &str, lang: Lang) -> Vec<Import> {
    only_imports(references(content, lang))
}

/// [`extract_imports`] over a tree the caller has already parsed from
/// `content` with `lang`'s grammar, so a caller that has one parses once.
#[must_use]
pub fn imports_in_tree(root: tree_sitter::Node, content: &str, lang: Lang) -> Vec<Import> {
    only_imports(references_in_tree(root, content, lang))
}

/// The imports among `references`.
fn only_imports(references: Vec<Reference>) -> Vec<Import> {
    references
        .into_iter()
        .filter_map(|reference| match reference {
            Reference::Import { import, .. } => Some(import),
            Reference::Mod(_) => None,
        })
        .collect()
}

/// Every module `content` names, in source order: its imports, as
/// [`extract_imports`] reads them, and in Rust its `mod` declarations.
pub(crate) fn references(content: &str, lang: Lang) -> Vec<Reference> {
    if !reads_statements_from_tree(lang) {
        return top_level(line_imports(content, lang));
    }
    let Some(language) = outline_language(lang) else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(content, None) else {
        return Vec::new();
    };
    references_in_tree(tree.root_node(), content, lang)
}

/// [`references`] over a tree already parsed from `content`.
fn references_in_tree(root: tree_sitter::Node, content: &str, lang: Lang) -> Vec<Reference> {
    if !reads_statements_from_tree(lang) {
        return top_level(line_imports(content, lang));
    }
    let Some(query) = statement_query(lang) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = tree_sitter::QueryCursor::new();
    // These grammars allow imports only at the top of a file, so the match
    // need not look below the root's children.
    if matches!(
        lang,
        Lang::Go | Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Java | Lang::Kotlin
    ) {
        cursor.set_max_start_depth(Some(1));
    }
    let mut matches = cursor.matches(query, root, content.as_bytes());
    while let Some(found) = matches.next() {
        for capture in found.captures() {
            let node = capture.node;
            if lang == Lang::Rust && node.kind() == "mod_item" {
                out.extend(rust_mod_declaration(node, content).map(Reference::Mod));
                continue;
            }
            let within = if lang == Lang::Rust {
                rust_inline_modules(node, content)
            } else {
                Vec::new()
            };
            for (source, line) in statement_modules(node, content, lang).into_iter().flatten() {
                if source.is_empty() {
                    continue;
                }
                let names = if lang == Lang::Python && node.kind() == "import_from_statement" {
                    python_from_names(node, content)
                } else {
                    Vec::new()
                };
                let import = Import {
                    source,
                    line,
                    start_line: line_of(node),
                    end_line: last_line_of(node),
                    names,
                };
                out.push(Reference::Import {
                    import,
                    within: within.clone(),
                });
            }
        }
    }
    out
}

/// `imports`, each at the top level of its file.
fn top_level(imports: Vec<Import>) -> Vec<Reference> {
    imports
        .into_iter()
        .map(|import| Reference::Import {
            import,
            within: Vec::new(),
        })
        .collect()
}

/// The query matching the node kinds that can be an import statement in
/// `lang`, compiled on first use and kept for the life of the process.
/// Matching runs inside tree-sitter, which is faster than stepping a cursor
/// over every node from Rust. `statement_modules` then reads each candidate,
/// and drops the Elixir calls that are not imports. Each language compiles
/// only when a file of it is read, since compiling costs a few milliseconds.
fn statement_query(lang: Lang) -> Option<&'static tree_sitter::Query> {
    let pattern = match lang {
        Lang::Go | Lang::Scala | Lang::Java | Lang::Swift => "(import_declaration) @statement",
        Lang::Rust => "[(use_declaration) (mod_item)] @statement",
        Lang::Python => {
            "[(import_statement) (import_from_statement) (future_import_statement)] @statement"
        }
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            "[(import_statement) (export_statement source: (_))] @statement"
        }
        Lang::Elixir => "(call target: (identifier)) @statement",
        Lang::Kotlin => "(import) @statement",
        Lang::Php => "(namespace_use_declaration) @statement",
        Lang::CSharp => "(using_directive) @statement",
        _ => return None,
    };
    compiled_query(&outline_language(lang)?, pattern)
}

/// Languages whose import statements are read from the syntax tree: one
/// statement can name several modules, span lines, or carry an alias or
/// modifier that is not part of the module.
fn reads_statements_from_tree(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::Go
            | Lang::Rust
            | Lang::Python
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::JavaScript
            | Lang::Scala
            | Lang::Elixir
            | Lang::Java
            | Lang::Kotlin
            | Lang::Php
            | Lang::CSharp
            | Lang::Swift
    )
}

/// One import per line `is_import_line` accepts.
fn line_imports(content: &str, lang: Lang) -> Vec<Import> {
    let mut out = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if !is_import_line(line, lang) {
            continue;
        }
        let source = crate::lang::outline::extract_import_source(line, Some(lang));
        if source.is_empty() {
            continue;
        }
        let line = index as u32 + 1;
        out.push(Import {
            source,
            line,
            start_line: line,
            end_line: line,
            names: Vec::new(),
        });
    }
    out
}

/// The modules an import statement names, each with the line it is named on.
/// `None` when `node` is not an import statement.
fn statement_modules(node: tree_sitter::Node, src: &str, lang: Lang) -> Option<Vec<(String, u32)>> {
    let mut out = Vec::new();
    match (lang, node.kind()) {
        (Lang::Go, "import_declaration") => go_specs(node, src, &mut out),
        (Lang::Rust, "use_declaration") => {
            if let Some(tree) = node.child_by_field_name("argument") {
                rust_use_tree(tree, "", src, &mut out);
            }
        }
        (Lang::Python, "import_statement") => {
            let mut cursor = node.walk();
            for name in node.children_by_field_name("name", &mut cursor) {
                // `import a.b as c` names `a.b`.
                let module = name.child_by_field_name("name").unwrap_or(name);
                out.push((compact(module, src), line_of(module)));
            }
        }
        (Lang::Python, "import_from_statement") => {
            if let Some(module) = node.child_by_field_name("module_name") {
                out.push((compact(module, src), line_of(module)));
            }
        }
        (Lang::Python, "future_import_statement") => {
            out.push(("__future__".to_string(), line_of(node)));
        }
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, "export_statement") => {
            // Only a re-export names a module: `export { x } from "m"`,
            // `export * from "m"`, `export * as ns from "m"`.
            if let Some(source) = node.child_by_field_name("source") {
                out.push((unquote(source, src), line_of(source)));
            }
        }
        (Lang::TypeScript | Lang::Tsx | Lang::JavaScript, "import_statement") => {
            // `import x from "m"` and `import "m"` carry the source on the
            // statement; `import x = require("m")` on its require clause.
            let source = node.child_by_field_name("source").or_else(|| {
                let mut cursor = node.walk();
                let clause = node
                    .children(&mut cursor)
                    .find(|c| c.kind() == "import_require_clause");
                clause.and_then(|c| c.child_by_field_name("source"))
            });
            if let Some(source) = source {
                out.push((unquote(source, src), line_of(source)));
            }
        }
        (Lang::Scala, "import_declaration") => scala_paths(node, src, &mut out),
        (Lang::Java, "import_declaration") | (Lang::Kotlin, "import") => {
            dotted_import(node, src, &mut out);
        }
        (Lang::Elixir, "call") => elixir_directive(node, src, &mut out)?,
        (Lang::Php, "namespace_use_declaration") => php_use(node, src, &mut out),
        (Lang::CSharp, "using_directive") => csharp_using(node, src, &mut out),
        (Lang::Swift, "import_declaration") => {
            let mut cursor = node.walk();
            let path = node
                .named_children(&mut cursor)
                .find(|c| c.kind() == "identifier");
            if let Some(path) = path {
                out.push((compact(path, src), line_of(path)));
            }
        }
        _ => return None,
    }
    Some(out)
}

/// The names a Python `from m import a, b as c` takes from `m`: `a`, `b`.
fn python_from_names(node: tree_sitter::Node, src: &str) -> Vec<String> {
    let mut cursor = node.walk();
    node.children_by_field_name("name", &mut cursor)
        .map(|name| compact(name.child_by_field_name("name").unwrap_or(name), src))
        .collect()
}

/// Go: `import "m"` and `import ( "a"; x "b" )`, one module per spec.
fn go_specs(node: tree_sitter::Node, src: &str, out: &mut Vec<(String, u32)>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "import_spec" => {
                if let Some(path) = child.child_by_field_name("path") {
                    out.push((unquote(path, src), line_of(path)));
                }
            }
            "import_spec_list" => go_specs(child, src, out),
            _ => {}
        }
    }
}

/// Rust: expand a use tree into one path per leaf. `crate::{core, seams::git}`
/// gives `crate::core` and `crate::seams::git`; `foo::{self, bar as baz}`
/// gives `foo` and `foo::bar`; a glob keeps its `*`.
fn rust_use_tree(node: tree_sitter::Node, prefix: &str, src: &str, out: &mut Vec<(String, u32)>) {
    match node.kind() {
        "scoped_use_list" => {
            let path = node
                .child_by_field_name("path")
                .map(|p| compact(p, src))
                .unwrap_or_default();
            if let Some(list) = node.child_by_field_name("list") {
                rust_use_tree(list, &join_path(prefix, &path, "::"), src, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for member in node.named_children(&mut cursor) {
                rust_use_tree(member, prefix, src, out);
            }
        }
        "use_as_clause" => {
            if let Some(path) = node.child_by_field_name("path") {
                out.push((join_path(prefix, &compact(path, src), "::"), line_of(node)));
            }
        }
        "self" if !prefix.is_empty() => out.push((prefix.to_string(), line_of(node))),
        kind if kind.ends_with("comment") => {}
        _ => out.push((join_path(prefix, &compact(node, src), "::"), line_of(node))),
    }
}

/// Rust: the declaration a `mod_item` is when it has no body. `mod foo { … }`
/// holds its module inline and declares no file.
fn rust_mod_declaration(item: tree_sitter::Node, src: &str) -> Option<ModDecl> {
    if item.child_by_field_name("body").is_some() {
        return None;
    }
    let name = item.child_by_field_name("name")?;
    Some(ModDecl {
        name: text(name, src).to_string(),
        path_attr: rust_path_attribute(item, src),
        within: rust_inline_modules(item, src),
        line: line_of(name),
    })
}

/// Rust: the file a `#[path = "…"]` attribute above `item` names. The
/// attributes are the item's preceding siblings in tree-sitter-rust, and
/// doc comments may sit between them.
fn rust_path_attribute(item: tree_sitter::Node, src: &str) -> Option<String> {
    let mut sibling = item.prev_sibling();
    while let Some(node) = sibling {
        match node.kind() {
            "attribute_item" => {
                let value = node
                    .named_child(0)
                    .filter(|attribute| {
                        attribute
                            .named_child(0)
                            .is_some_and(|name| text(name, src) == "path")
                    })
                    .and_then(|attribute| attribute.child_by_field_name("value"))
                    .filter(|value| value.kind() == "string_literal");
                if let Some(value) = value {
                    return Some(unquote(value, src));
                }
            }
            kind if kind.ends_with("comment") => {}
            _ => return None,
        }
        sibling = node.prev_sibling();
    }
    None
}

/// Rust: the names of the inline modules `node` sits in, outermost first.
fn rust_inline_modules(node: tree_sitter::Node, src: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut ancestor = node.parent();
    while let Some(module) = ancestor {
        if module.kind() == "mod_item" {
            if let Some(name) = module.child_by_field_name("name") {
                names.push(text(name, src).to_string());
            }
        }
        ancestor = module.parent();
    }
    names.reverse();
    names
}

/// Scala: `import a.b, c.d` gives two paths; `import a.{b, c => d}` gives
/// `a.b` and `a.c`; a wildcard keeps its `_`.
fn scala_paths(node: tree_sitter::Node, src: &str, out: &mut Vec<(String, u32)>) {
    /// Close the path read so far; a path that ended in selectors or a
    /// wildcard has already given its imports.
    fn flush(path: &mut String, line: u32, selected: &mut bool, out: &mut Vec<(String, u32)>) {
        let stem = path.trim_end_matches('.');
        if !*selected && !stem.is_empty() {
            out.push((stem.to_string(), line));
        }
        path.clear();
        *selected = false;
    }

    let mut path = String::new();
    let mut path_line = line_of(node);
    let mut selected = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import" => {}
            "," => flush(&mut path, path_line, &mut selected, out),
            "namespace_selectors" => {
                let stem = path.trim_end_matches('.').to_string();
                let mut members = child.walk();
                for member in child.named_children(&mut members) {
                    if member.kind().ends_with("comment") {
                        continue;
                    }
                    let name = if member.kind() == "namespace_wildcard" {
                        "_".to_string()
                    } else {
                        compact(member.child_by_field_name("name").unwrap_or(member), src)
                    };
                    out.push((join_path(&stem, &name, "."), line_of(member)));
                }
                selected = true;
            }
            "namespace_wildcard" => {
                out.push((
                    join_path(path.trim_end_matches('.'), "_", "."),
                    line_of(child),
                ));
                selected = true;
            }
            kind if kind.ends_with("comment") => {}
            _ => {
                if path.is_empty() {
                    path_line = line_of(child);
                }
                path.push_str(&compact(child, src));
            }
        }
    }
    flush(&mut path, path_line, &mut selected, out);
}

/// Java and Kotlin: one dotted path per statement, a wildcard kept as `.*`.
/// `static` and an `as` alias are not part of the module.
fn dotted_import(node: tree_sitter::Node, src: &str, out: &mut Vec<(String, u32)>) {
    let mut cursor = node.walk();
    let children: Vec<tree_sitter::Node> = node.children(&mut cursor).collect();
    let Some(path) = children.iter().find(|c| {
        matches!(
            c.kind(),
            "scoped_identifier" | "qualified_identifier" | "identifier"
        )
    }) else {
        return;
    };
    let mut source = compact(*path, src);
    if children
        .iter()
        .any(|c| matches!(c.kind(), "asterisk" | "*"))
    {
        source.push_str(".*");
    }
    out.push((source, line_of(*path)));
}

/// Elixir: `alias`, `import`, `use` and `require` name the module in their
/// first argument; `alias A.{B, C}` names `A.B` and `A.C`. `None` when the
/// call is none of the four.
fn elixir_directive(
    node: tree_sitter::Node,
    src: &str,
    out: &mut Vec<(String, u32)>,
) -> Option<()> {
    let target = node.child_by_field_name("target")?;
    if target.kind() != "identifier"
        || !matches!(text(target, src), "alias" | "import" | "use" | "require")
    {
        return None;
    }
    let mut cursor = node.walk();
    let arguments = node
        .children(&mut cursor)
        .find(|c| c.kind() == "arguments")?;
    let mut args = arguments.walk();
    let Some(first) = arguments
        .named_children(&mut args)
        .find(|c| !c.kind().ends_with("comment"))
    else {
        return Some(());
    };
    let group = (first.kind() == "dot")
        .then(|| first.child_by_field_name("right"))
        .flatten()
        .filter(|right| right.kind() == "tuple");
    match (group, first.child_by_field_name("left")) {
        (Some(tuple), Some(left)) => {
            let stem = compact(left, src);
            let mut members = tuple.walk();
            for member in tuple.named_children(&mut members) {
                if !member.kind().ends_with("comment") {
                    out.push((
                        join_path(&stem, &compact(member, src), "."),
                        line_of(member),
                    ));
                }
            }
        }
        _ => out.push((compact(first, src), line_of(first))),
    }
    Some(())
}

/// PHP: `use A\B;`, `use A\B as C, D\E;` and the group `use A\{B, C as D};`,
/// one module per clause, its alias left out. A group's members sit under
/// its prefix. `use function` and `use const` import a function or a
/// constant by its qualified name, which is read the same way.
fn php_use(node: tree_sitter::Node, src: &str, out: &mut Vec<(String, u32)>) {
    fn clause_name(clause: tree_sitter::Node) -> Option<tree_sitter::Node> {
        let mut cursor = clause.walk();
        let found = clause
            .named_children(&mut cursor)
            .find(|c| matches!(c.kind(), "qualified_name" | "name"));
        // `use A\B as C`: the alias is a `name` too, after the module.
        found.filter(|n| clause.child_by_field_name("alias") != Some(*n))
    }

    let prefix = {
        let mut cursor = node.walk();
        let found = node
            .named_children(&mut cursor)
            .find(|c| c.kind() == "namespace_name");
        found.map(|p| compact(p, src))
    };
    // A group's clauses sit in its body; plain clauses on the statement.
    let holder = node.child_by_field_name("body").unwrap_or(node);
    let mut cursor = holder.walk();
    let clauses: Vec<tree_sitter::Node> = holder.named_children(&mut cursor).collect();
    for clause in clauses {
        if clause.kind() != "namespace_use_clause" {
            continue;
        }
        let Some(name) = clause_name(clause) else {
            continue;
        };
        let name = compact(name, src);
        let name = name.trim_start_matches('\\');
        let source = prefix.as_deref().map_or_else(
            || name.to_string(),
            |prefix| join_path(prefix.trim_start_matches('\\'), name, "\\"),
        );
        out.push((source, line_of(clause)));
    }
}

/// C#: `using A.B;`, `using static A.B;` and the alias `using X = A.B;`
/// name the namespace or type `A.B`.
fn csharp_using(node: tree_sitter::Node, src: &str, out: &mut Vec<(String, u32)>) {
    let alias = node.child_by_field_name("name");
    let mut cursor = node.walk();
    let target = node
        .named_children(&mut cursor)
        .find(|c| Some(*c) != alias && !c.kind().ends_with("comment"));
    if let Some(target) = target {
        out.push((compact(target, src), line_of(target)));
    }
}

fn text<'a>(node: tree_sitter::Node, src: &'a str) -> &'a str {
    &src[node.byte_range()]
}

/// A path's text with the whitespace a formatter may put inside it removed.
fn compact(node: tree_sitter::Node, src: &str) -> String {
    text(node, src).split_whitespace().collect()
}

/// A string literal's content.
fn unquote(node: tree_sitter::Node, src: &str) -> String {
    text(node, src)
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

fn join_path(prefix: &str, rest: &str, separator: &str) -> String {
    if prefix.is_empty() {
        rest.to_string()
    } else if rest.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}{separator}{rest}")
    }
}

fn line_of(node: tree_sitter::Node) -> u32 {
    node.start_position().row as u32 + 1
}

/// The last line a node covers; a node that ends at the start of a line
/// (after its newline) ends on the line before.
fn last_line_of(node: tree_sitter::Node) -> u32 {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row {
        end.row as u32
    } else {
        end.row as u32 + 1
    }
}

#[must_use]
pub fn is_import_line(line: &str, lang: Lang) -> bool {
    let trimmed = line.trim_start();
    match lang {
        Lang::Rust => trimmed.starts_with("use "),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            trimmed.starts_with("import ") || trimmed.starts_with("import{")
        }
        Lang::Python => trimmed.starts_with("import ") || trimmed.starts_with("from "),
        Lang::Go | Lang::Java | Lang::Scala | Lang::Kotlin => trimmed.starts_with("import "),
        Lang::C | Lang::Cpp => trimmed.starts_with("#include"),
        Lang::Elixir => {
            trimmed.starts_with("alias ")
                || trimmed.starts_with("import ")
                || trimmed.starts_with("use ")
                || trimmed.starts_with("require ")
        }
        Lang::Bash => trimmed
            .strip_prefix("source")
            .or_else(|| trimmed.strip_prefix('.'))
            .is_some_and(|rest| rest.starts_with(char::is_whitespace)),
        _ => false,
    }
}

#[must_use]
pub fn is_external(source: &str, lang: Lang) -> bool {
    match lang {
        Lang::Rust => {
            !(source.starts_with("crate::")
                || source.starts_with("self::")
                || source.starts_with("super::"))
        }
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            !(source.starts_with('.') || source.starts_with("@/") || source.starts_with("~/"))
        }
        // Bash: dot-relative paths are local; anything else (bare name, /abs/path) is external.
        Lang::Python | Lang::Bash => !source.starts_with('.'),
        Lang::C | Lang::Cpp => !source.starts_with('"'),
        // Elixir, Go, Java, Scala, Kotlin — can't resolve without build system knowledge.
        _ => true,
    }
}

fn resolve(dir: &Path, source: &str, lang: Lang) -> Option<PathBuf> {
    let raw = match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => resolve_js(dir, source),
        Lang::Python => resolve_python(dir, source),
        Lang::C | Lang::Cpp => resolve_c_include(dir, source),
        Lang::Bash => resolve_bash(dir, source),
        // Rust resolves by its module tree, in `resolve_rust_use` and
        // `resolve_rust_mod`. Elixir, Go, Java, etc. — module-to-file
        // mapping requires build system conventions.
        _ => None,
    };
    raw.map(|p| normalize_path(&p))
}

/// Whether `lang`'s imports resolve to local files, by `resolve` or, in
/// Rust, by the module tree. Elsewhere the mapping from a module to its
/// files lives in build-system conventions tilth does not read.
pub(crate) fn resolves_imports(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::Rust
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::JavaScript
            | Lang::Python
            | Lang::C
            | Lang::Cpp
            | Lang::Bash
    )
}

/// Whether an import that tilth resolves can name `file`, of `lang`. An
/// include names a C or C++ header, never a source file.
pub(crate) fn can_be_imported(file: &Path, lang: Lang) -> bool {
    resolves_imports(lang)
        && (!matches!(lang, Lang::C | Lang::Cpp)
            || file.extension().is_some_and(|e| e == "h" || e == "hpp"))
}

/// The package `content`, of `lang`, declares, read from its syntax tree:
/// Go's `package` clause, Java's `package` declaration, Kotlin's package
/// header, or Scala's package clauses, which chain (`package a` then
/// `package b` is `a.b`). `None` when the file declares none, when a Scala
/// file holds more than one package block, or for any other language.
pub(crate) fn declared_package(content: &str, lang: Lang) -> Option<String> {
    let declaration = match lang {
        Lang::Go | Lang::Scala => "package_clause",
        Lang::Java => "package_declaration",
        Lang::Kotlin => "package_header",
        _ => return None,
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&outline_language(lang)?).ok()?;
    let tree = parser.parse(content, None)?;
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut clauses = root
        .named_children(&mut cursor)
        .filter(|n| n.kind() == declaration);
    if lang != Lang::Scala {
        // The name is the declaration's one child that is not an annotation.
        let clause = clauses.next()?;
        let mut cursor = clause.walk();
        let name = clause
            .named_children(&mut cursor)
            .find(|n| !n.kind().ends_with("annotation"))?;
        return Some(compact(name, content));
    }
    let mut package = String::new();
    let mut closed = false;
    for clause in clauses {
        // A clause with a body holds its package; a clause after it
        // opens another one.
        if closed {
            return None;
        }
        let name = clause.child_by_field_name("name")?;
        package = join_path(&package, &compact(name, content), ".");
        closed = clause.child_by_field_name("body").is_some();
    }
    (!package.is_empty()).then_some(package)
}

/// Lexically collapse `.` and `..` components without touching the filesystem.
/// `dir.join("../foo")` returns a `PathBuf` containing literal `..`; without this,
/// distinct spellings of the same target file produce distinct `PathBuf`s and
/// downstream callers (dedup loops, `HashMap` keys) treat them as different files.
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                let pop_ok = matches!(
                    out.components().next_back(),
                    Some(Component::Normal(_) | Component::Prefix(_))
                );
                if pop_ok {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            _ => out.push(comp),
        }
    }
    out
}

// --- Rust ---

/// The file a Rust `use` path in `file` names, by Rust's module tree, for
/// a statement inside the inline modules `within`. `self` is the module the
/// statement sits in, whose child modules live in its `module_scope`, and
/// each `super` is one directory up. `crate::` paths start at the nearest
/// `src/`. Any other path names another crate.
fn resolve_rust_use(file: &Path, source: &str, within: &[String]) -> Option<PathBuf> {
    if let Some(rest) = source.strip_prefix("crate::") {
        return try_rust_path(find_src_ancestor(file.parent()?)?, rest);
    }
    if !(source.starts_with("self::") || source.starts_with("super::")) {
        return None;
    }
    let mut base = module_scope(file, within)?;
    let mut rest = source.strip_prefix("self::").unwrap_or(source);
    while let Some(after) = rest.strip_prefix("super::") {
        base = base.parent()?.to_path_buf();
        rest = after;
    }
    try_rust_path(&base, rest)
}

/// The file a `mod` declaration in `file` names: `name.rs` or
/// `name/mod.rs` in the declaring module's `module_scope`. A `#[path]`
/// names its file relative to the declaring file's directory, or, inside
/// inline modules, relative to theirs.
fn resolve_rust_mod(file: &Path, decl: &ModDecl) -> Option<PathBuf> {
    let scope = module_scope(file, &decl.within)?;
    match &decl.path_attr {
        Some(path) => {
            let base = if decl.within.is_empty() {
                file.parent()?
            } else {
                &scope
            };
            let file = base.join(path);
            file.is_file().then_some(file)
        }
        None => try_rust_module(&scope.join(decl.name.trim_start_matches("r#"))),
    }
}

/// The directory holding the files of the child modules of the module
/// that the inline modules `within` (outermost first) make inside `file`:
/// `file`'s `module_dir`, one level down for each inline module.
fn module_scope(file: &Path, within: &[String]) -> Option<PathBuf> {
    Some(
        within
            .iter()
            .fold(module_dir(file)?, |dir, module| dir.join(module)),
    )
}

/// The directory holding the files of the modules `file` declares: a
/// crate root's or a `mod.rs`'s own directory, and for any other `x.rs`
/// the directory `x/` beside it.
fn module_dir(file: &Path) -> Option<PathBuf> {
    let dir = file.parent()?;
    if file.file_name().is_some_and(|name| name == "mod.rs") || is_crate_root(file) {
        Some(dir.to_path_buf())
    } else {
        Some(dir.join(file.file_stem()?))
    }
}

/// Whether `file` is a crate root by Cargo's conventions: a `main.rs` or
/// `lib.rs`, a package's `build.rs`, or a file directly in a package's
/// `src/bin`, `tests`, `examples` or `benches`.
fn is_crate_root(file: &Path) -> bool {
    let (Some(name), Some(dir)) = (file.file_name(), file.parent()) else {
        return false;
    };
    if name == "main.rs" || name == "lib.rs" {
        return true;
    }
    let package = if name == "build.rs" {
        Some(dir)
    } else {
        match dir.file_name().and_then(|n| n.to_str()) {
            Some("tests" | "examples" | "benches") => dir.parent(),
            Some("bin") => dir
                .parent()
                .filter(|src| src.file_name().is_some_and(|n| n == "src"))
                .and_then(Path::parent),
            _ => None,
        }
    };
    package.is_some_and(|package| package.join("Cargo.toml").is_file())
}

/// Try progressively shorter paths until one resolves.
/// `cache::OutlineCache` → try cache/OutlineCache.rs (no) → cache.rs (yes).
/// `read::imports` → try read/imports.rs (yes) → stop.
fn try_rust_path(base: &Path, rest: &str) -> Option<PathBuf> {
    let segments: Vec<&str> = rest.split("::").collect();
    for len in (1..=segments.len()).rev() {
        let rel: PathBuf = segments[..len].iter().collect();
        if let Some(found) = try_rust_module(&base.join(&rel)) {
            return Some(found);
        }
    }
    None
}

fn try_rust_module(base: &Path) -> Option<PathBuf> {
    let with_rs = base.with_extension("rs");
    if with_rs.exists() {
        return Some(with_rs);
    }
    let mod_rs = base.join("mod.rs");
    if mod_rs.exists() {
        return Some(mod_rs);
    }
    None
}

fn find_src_ancestor(start: &Path) -> Option<&Path> {
    let mut current = start;
    loop {
        if current.file_name().and_then(|n| n.to_str()) == Some("src") {
            return Some(current);
        }
        current = current.parent()?;
    }
}

// --- JS/TS ---

fn resolve_js(dir: &Path, source: &str) -> Option<PathBuf> {
    let base = dir.join(source);
    // Try with extensions
    for ext in &[".ts", ".tsx", ".js", ".jsx"] {
        let mut candidate = base.clone().into_os_string();
        candidate.push(ext);
        let candidate = PathBuf::from(candidate);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // Already has extension
    if base.exists() && base.is_file() {
        return Some(base);
    }
    // Index files
    for name in &["index.ts", "index.tsx", "index.js", "index.jsx"] {
        let candidate = base.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

// --- Python ---

fn resolve_python(dir: &Path, source: &str) -> Option<PathBuf> {
    let dots = source.bytes().take_while(|&b| b == b'.').count();
    if dots == 0 {
        return resolve_python_absolute(dir, source);
    }
    // Each dot beyond the first goes up one directory.
    let mut base = dir.to_path_buf();
    for _ in 1..dots {
        base = base.parent()?.to_path_buf();
    }
    let module_part = &source[dots..];
    if module_part.is_empty() {
        // Bare `from . import X`
        let init = base.join("__init__.py");
        return if init.exists() { Some(init) } else { None };
    }
    let rel = module_part.replace('.', "/");
    let as_file = base.join(format!("{rel}.py"));
    if as_file.exists() {
        return Some(as_file);
    }
    let as_pkg = base.join(&rel).join("__init__.py");
    if as_pkg.exists() {
        return Some(as_pkg);
    }
    None
}

/// `a.b.c` as Python finds it from a file in `dir`: `a/b/c.py` or
/// `a/b/c/__init__.py` under a directory on Python's path, or under that
/// directory's `src/` (the src layout). Python 3 puts a script's own
/// directory on its path, and for a module in a package the directory
/// above its top package; the directory of a package is never on it, so a
/// `json.py` inside a package does not hide the standard library's `json`.
/// When a project root is marked (`.git`, `pyproject.toml`, `setup.py` or
/// `setup.cfg`), every directory from `dir` up to it that is not a package
/// is searched, nearest first, as running a test or script from the root
/// puts them on the path. Without one, only the nearest such directory is.
fn resolve_python_absolute(dir: &Path, source: &str) -> Option<PathBuf> {
    const ROOT_MARKERS: [&str; 4] = [".git", "pyproject.toml", "setup.py", "setup.cfg"];
    let first = source.split('.').next().filter(|p| !p.is_empty())?;
    let rel: PathBuf = source.split('.').collect();
    let root = dir
        .ancestors()
        .find(|a| ROOT_MARKERS.iter().any(|m| a.join(m).exists()));
    // The directories on the path, nearest first.
    let mut on_path = Vec::new();
    for ancestor in dir.ancestors() {
        let at_root = Some(ancestor) == root;
        if at_root || !ancestor.join("__init__.py").is_file() {
            on_path.push(ancestor);
            if root.is_none() {
                break;
            }
        }
        if at_root {
            break;
        }
    }
    on_path
        .into_iter()
        .flat_map(|dir| [dir.to_path_buf(), dir.join("src")])
        .find_map(|base| {
            // One stat rules out most bases: the top-level package or module.
            let top = base.join(first);
            if !top.is_dir() && !top.with_extension("py").is_file() {
                return None;
            }
            let as_file = base.join(&rel).with_extension("py");
            if as_file.is_file() {
                return Some(as_file);
            }
            let as_pkg = base.join(&rel).join("__init__.py");
            as_pkg.is_file().then_some(as_pkg)
        })
}

// --- C/C++ ---

fn resolve_c_include(dir: &Path, source: &str) -> Option<PathBuf> {
    let clean = source.trim_matches('"');
    let candidate = dir.join(clean);
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

// --- Bash ---

fn resolve_bash(dir: &Path, source: &str) -> Option<PathBuf> {
    // Only resolve literal relative paths — no extension inference. A single
    // metadata() stat avoids the exists()+is_file() two-call TOCTOU; resolution
    // is best-effort, so a stale result only ever costs a related-file hint.
    let candidate = dir.join(source);
    std::fs::metadata(&candidate)
        .is_ok_and(|m| m.is_file())
        .then_some(candidate)
}

#[cfg(test)]
#[expect(
    clippy::disallowed_methods,
    reason = "by design: tests read the files they check directly"
)]
mod tests {
    use super::*;
    use std::fs;

    /// A pipe names no imports, and looking for them never blocks on one
    /// that has no writer, whether its name says code or not.
    #[cfg(unix)]
    #[test]
    fn a_pipe_never_blocks_the_imports_lookup() {
        use crate::test_readers::{make_pipe, unless_blocked_on};
        let dir = tempfile::tempdir().unwrap();
        for name in ["main.rs", "server.log"] {
            let pipe = dir.path().join(name);
            make_pipe(&pipe);
            let cache = crate::cache::OutlineCache::new();
            let related = unless_blocked_on(&pipe, || resolve_related_files(&pipe, &cache));
            assert_eq!(
                related.expect("the imports lookup blocked on a pipe"),
                Vec::<PathBuf>::new(),
                "{name}"
            );
        }
    }

    /// A Scala package block holds its package, and a file of two blocks
    /// is in no one package. A file that declares none is in none.
    #[test]
    fn a_file_declares_at_most_one_package() {
        let package = |content: &str, lang| declared_package(content, lang);
        assert_eq!(
            package("package a.b {\n  object X\n}\n", Lang::Scala).as_deref(),
            Some("a.b")
        );
        assert_eq!(
            package(
                "package a { object X }\npackage b { object Y }\n",
                Lang::Scala
            ),
            None
        );
        assert_eq!(package("class X {}\n", Lang::Java), None);
        assert_eq!(package("fun x() = 1\n", Lang::Kotlin), None);
        assert_eq!(package("pub mod a {}\n", Lang::Rust), None);
    }

    #[test]
    fn every_tree_read_language_has_a_compiled_statement_query() {
        for lang in [
            Lang::Go,
            Lang::Rust,
            Lang::Python,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::JavaScript,
            Lang::Scala,
            Lang::Elixir,
            Lang::Java,
            Lang::Kotlin,
            Lang::Php,
            Lang::CSharp,
            Lang::Swift,
        ] {
            assert!(reads_statements_from_tree(lang), "{lang:?}");
            assert!(statement_query(lang).is_some(), "{lang:?}");
        }
    }

    /// The files `rel` (a file under `root`) imports, relative to `root`.
    fn related(root: &Path, rel: &str) -> Vec<String> {
        let file = root.join(rel);
        let content = fs::read_to_string(&file).unwrap();
        resolve_related_files_with_content(&file, &content)
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect()
    }

    fn write_tree(root: &Path, files: &[(&str, &str)]) {
        for (rel, content) in files {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    /// `from pkg import mod` imports the submodule `pkg/mod.py` when there
    /// is one, and a name from `pkg/__init__.py` otherwise.
    #[test]
    fn python_from_import_resolves_submodules() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_tree(
            &root,
            &[
                ("pyproject.toml", ""),
                ("pkg/__init__.py", ""),
                ("pkg/other.py", ""),
                ("pkg/sub/__init__.py", "def helper():\n    pass\n"),
                ("pkg/sub/mod.py", ""),
                ("pkg/a.py", "from pkg.sub import mod\nfrom . import other\n"),
                (
                    "pkg/b.py",
                    "from pkg.sub import (\n    mod as m,\n    helper,\n)\n",
                ),
            ],
        );
        assert_eq!(
            related(&root, "pkg/a.py"),
            ["pkg/sub/mod.py", "pkg/other.py"]
        );
        assert_eq!(
            related(&root, "pkg/b.py"),
            ["pkg/sub/mod.py", "pkg/sub/__init__.py"]
        );
    }

    /// Python 3 puts a script's directory on the path, and for a module in
    /// a package the directory above its top package; a package's own
    /// directory is never on it. So `import json` in a package module is the
    /// standard library even beside a `json.py`, while a script imports its
    /// neighbour. Without a project root, the search stops there instead of
    /// climbing to `/`.
    #[test]
    fn python_absolute_imports_search_where_python_does() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_tree(
            &root,
            &[
                ("proj/pyproject.toml", ""),
                ("proj/pkg/__init__.py", ""),
                ("proj/pkg/json.py", ""),
                ("proj/pkg/util.py", ""),
                ("proj/pkg/a.py", "import json\nimport pkg.util\n"),
                ("proj/scripts/helper.py", ""),
                ("proj/scripts/run.py", "import helper\nimport json\n"),
                ("loose/util.py", ""),
                ("loose/inner/pkg2/__init__.py", ""),
                ("loose/inner/pkg2/a.py", "import util\nimport pkg2\n"),
            ],
        );
        assert_eq!(related(&root.join("proj"), "pkg/a.py"), ["pkg/util.py"]);
        assert_eq!(
            related(&root.join("proj"), "scripts/run.py"),
            ["scripts/helper.py"]
        );
        assert_eq!(
            related(&root.join("loose"), "inner/pkg2/a.py"),
            ["inner/pkg2/__init__.py"]
        );
    }

    fn sources(content: &str, lang: Lang) -> Vec<String> {
        extract_imports(content, lang)
            .into_iter()
            .map(|i| i.source)
            .collect()
    }

    #[test]
    fn import_shaped_text_in_a_string_is_not_an_import() {
        let python = "DOC = \"\"\"\nimport not_a_module\n\"\"\"\nimport real\n";
        assert_eq!(sources(python, Lang::Python), ["real"]);
    }

    #[test]
    fn rust_use_trees_expand_to_leaf_paths() {
        let rust = "use foo::{self, bar as baz, *};\nuse x::*;\nfn f() {\n    use inner::q;\n}\n";
        assert_eq!(
            sources(rust, Lang::Rust),
            ["foo", "foo::bar", "foo::*", "x::*", "inner::q"]
        );
    }

    #[test]
    fn line_languages_give_one_import_per_line() {
        let bash = "source ./lib.sh\nsource \"$DIR/x.sh\"\n";
        assert_eq!(sources(bash, Lang::Bash), ["./lib.sh"]);
        let c = "#include \"util.h\"\n#include <stdio.h>\n";
        assert_eq!(sources(c, Lang::C), ["\"util.h\"", "<stdio.h>"]);
    }

    /// Java's `static` and Kotlin's `as` are not part of the module. Read off
    /// the line, the source was `static a.b.D.e` or `a.b.D as E`.
    #[test]
    fn java_and_kotlin_imports_name_the_module_alone() {
        let java = "import a.b.C;\nimport static a.b.D.e;\nimport a.*;\nclass X {}\n";
        assert_eq!(sources(java, Lang::Java), ["a.b.C", "a.b.D.e", "a.*"]);
        let kotlin = "import a.b.C\nimport a.b.*\nimport a.b.D as E\n";
        assert_eq!(sources(kotlin, Lang::Kotlin), ["a.b.C", "a.b.*", "a.b.D"]);
    }

    /// A Rust `use` group names one module per member, and each one that is
    /// local resolves to its own file. Read off the line as one source, the
    /// group was `crate` and resolved to nothing.
    #[test]
    fn rust_use_group_resolves_every_member() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("seams")).unwrap();
        fs::write(src.join("core.rs"), "").unwrap();
        fs::write(src.join("seams/git.rs"), "").unwrap();
        let content = "use crate::{core, seams::git};\nuse std::fs;\n";
        let related = resolve_related_files_with_content(&src.join("main.rs"), content);
        assert_eq!(related, [src.join("core.rs"), src.join("seams/git.rs")]);
    }

    /// A bodiless `mod` declares a module whose code is in a file of its
    /// own, so it imports that file. A crate root (`main.rs`, `lib.rs`, a
    /// target Cargo finds by its place in a package) and a `mod.rs` hold
    /// their modules beside them; any other `x.rs` holds them under `x/`.
    /// `#[path]` names the file, and an inline module adds its name to the
    /// directory. Matching `use` alone, a crate that reaches its modules by
    /// `mod` and paths imported nothing.
    #[test]
    fn rust_mod_declarations_import_their_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_tree(
            &root,
            &[
                ("Cargo.toml", "[package]\nname = \"m\"\n"),
                (
                    "src/main.rs",
                    "mod foo;\npub mod bar;\n/// Doc.\n#[cfg(unix)]\n#[path = \"sys/unix.rs\"]\nmod sys;\nmod inline {\n    fn f() {}\n}\nmod outer {\n    pub(crate) mod nested;\n}\nmod missing;\nfn main() {}\n",
                ),
                ("src/foo.rs", "mod child;\n"),
                ("src/foo/child.rs", ""),
                ("src/child.rs", ""),
                ("src/bar/mod.rs", "mod baz;\n"),
                ("src/bar/baz.rs", ""),
                ("src/sys/unix.rs", ""),
                ("src/sys.rs", ""),
                ("src/inline.rs", ""),
                ("src/outer/nested.rs", ""),
                ("tests/it.rs", "mod common;\n"),
                ("tests/common/mod.rs", ""),
            ],
        );
        assert_eq!(
            related(&root, "src/main.rs"),
            [
                "src/foo.rs",
                "src/bar/mod.rs",
                "src/sys/unix.rs",
                "src/outer/nested.rs"
            ]
        );
        assert_eq!(related(&root, "src/foo.rs"), ["src/foo/child.rs"]);
        assert_eq!(related(&root, "src/bar/mod.rs"), ["src/bar/baz.rs"]);
        assert_eq!(related(&root, "tests/it.rs"), ["tests/common/mod.rs"]);
    }

    /// A bodiless `mod` declares its module's file, with the `#[path]` and
    /// the inline modules it sits in; an inline module declares none. Both
    /// stay out of the import statements.
    #[test]
    fn rust_mod_declarations_carry_their_path_and_scope() {
        let rust = "use a::b;\n#[path = \"x.rs\"]\nmod c;\nmod d { mod e; }\nmod f {}\n";
        let decls: Vec<(String, Option<String>, Vec<String>, u32)> = rust_mod_declarations(rust)
            .into_iter()
            .map(|d| (d.name, d.path_attr, d.within, d.line))
            .collect();
        assert_eq!(
            decls,
            [
                ("c".to_string(), Some("x.rs".to_string()), vec![], 3),
                ("e".to_string(), None, vec!["d".to_string()], 4),
            ]
        );
        let sources: Vec<String> = extract_imports(rust, Lang::Rust)
            .into_iter()
            .map(|i| i.source)
            .collect();
        assert_eq!(sources, ["a::b"]);
    }

    /// `self` and `super` are the module a statement sits in and its
    /// parent. A file `a.rs` is the module `a`, whose children are under
    /// `a/`; read from the file's own directory, `self::b` named `src/b.rs`
    /// and `super::c` looked above `src/`.
    #[test]
    fn rust_self_and_super_follow_the_module_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write_tree(
            &root,
            &[
                (
                    "src/a.rs",
                    "mod b;\nuse self::b::X;\nuse super::c::Y;\nmod tests {\n    use super::b::Z;\n    use super::super::d::W;\n}\n",
                ),
                ("src/a/b.rs", ""),
                ("src/b.rs", ""),
                ("src/c.rs", ""),
                ("src/d.rs", ""),
                ("src/lib.rs", "mod a;\nmod tests {\n    use super::c::Y;\n}\n"),
                ("src/x/mod.rs", "use super::d::W;\nuse self::e::V;\n"),
                ("src/x/e.rs", ""),
            ],
        );
        assert_eq!(
            related(&root, "src/a.rs"),
            ["src/a/b.rs", "src/c.rs", "src/d.rs"]
        );
        assert_eq!(related(&root, "src/lib.rs"), ["src/a.rs", "src/c.rs"]);
        assert_eq!(related(&root, "src/x/mod.rs"), ["src/d.rs", "src/x/e.rs"]);
    }

    /// An import written across several lines names its module on the last
    /// one. Read off the first line alone, the source was `{`.
    #[test]
    fn multi_line_js_import_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("util.ts"), "").unwrap();
        let content = "import {\n  first,\n  second,\n} from \"./util\";\n";
        let related = resolve_related_files_with_content(&tmp.path().join("main.ts"), content);
        assert_eq!(related, [tmp.path().join("util.ts")]);
    }

    /// PHP, C# and Swift statements come from the tree: a PHP group names
    /// each member under its prefix, and no alias is part of a source. Read
    /// off the line, the group was `App\\Models\\` and the alias stayed on.
    #[test]
    fn php_csharp_and_swift_imports_name_each_module_without_aliases() {
        let php = "<?php\nuse App\\Models\\{User, Post as P};\nuse Foo\\Bar as Baz, Qux;\nuse function Foo\\helper;\n$s = 'use Not\\Real;';\nclass A {}\n";
        assert_eq!(
            sources(php, Lang::Php),
            [
                "App\\Models\\User",
                "App\\Models\\Post",
                "Foo\\Bar",
                "Qux",
                "Foo\\helper"
            ]
        );
        let csharp = "using System;\nusing Json = Newtonsoft.Json;\nusing static System.Math;\nnamespace N {\n  using Inner.Thing;\n  class A {}\n}\n";
        assert_eq!(
            sources(csharp, Lang::CSharp),
            ["System", "Newtonsoft.Json", "System.Math", "Inner.Thing"]
        );
        let swift =
            "import Foundation\nimport struct Foo.Bar\n@testable import MyApp\nclass A {}\n";
        assert_eq!(
            sources(swift, Lang::Swift),
            ["Foundation", "Foo.Bar", "MyApp"]
        );
    }

    /// A re-export imports the module it names. Matching only `import`, a
    /// barrel that re-exports had no imports at all.
    #[test]
    fn js_re_exports_are_imports() {
        let ts = "export { y } from \"./y\";\nexport * from './all';\nexport * as ns from \"./ns\";\nimport { z } from \"./z\";\nexport { local };\nexport const k = 1;\n";
        for lang in [Lang::TypeScript, Lang::Tsx, Lang::JavaScript] {
            assert_eq!(
                sources(ts, lang),
                ["./y", "./all", "./ns", "./z"],
                "{lang:?}"
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("y.ts"), "").unwrap();
        fs::write(tmp.path().join("z.ts"), "").unwrap();
        let related = resolve_related_files_with_content(
            &tmp.path().join("index.ts"),
            "export { y } from \"./y\";\nimport { z } from \"./z\";\n",
        );
        assert_eq!(related, [tmp.path().join("y.ts"), tmp.path().join("z.ts")]);
    }

    #[test]
    fn normalize_collapses_dot_and_parent_components() {
        let p = Path::new("temporal/workflows/../utils/activityProxies.ts");
        assert_eq!(
            normalize_path(p),
            PathBuf::from("temporal/utils/activityProxies.ts")
        );
        let p = Path::new("app/db/./db.ts");
        assert_eq!(normalize_path(p), PathBuf::from("app/db/db.ts"));
    }

    #[test]
    fn normalize_preserves_leading_parent_when_unresolvable() {
        // No prior Normal component to pop, so ".." is kept.
        let p = Path::new("../outside.ts");
        assert_eq!(normalize_path(p), PathBuf::from("../outside.ts"));
    }

    #[test]
    fn js_resolve_returns_normalized_path_for_parent_import() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("temporal/workflows")).unwrap();
        fs::create_dir_all(root.join("temporal/utils")).unwrap();
        fs::write(root.join("temporal/utils/activityProxies.ts"), "").unwrap();

        let resolved = resolve_js(&root.join("temporal/workflows"), "../utils/activityProxies")
            .expect("should resolve");
        let normalized = normalize_path(&resolved);
        assert_eq!(normalized, root.join("temporal/utils/activityProxies.ts"));
        // No "../" component should survive normalization.
        assert!(
            !normalized
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "normalized path still contains '..': {normalized:?}"
        );
    }

    #[test]
    fn js_resolve_dedups_different_spellings_of_same_file() {
        // Two importers of the same file via different relative paths must
        // produce the same PathBuf so that hot-file counting aggregates them.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("a/c")).unwrap();
        fs::write(root.join("a/b/target.ts"), "").unwrap();

        let from_sibling =
            resolve(&root.join("a/b"), "./target", Lang::TypeScript).expect("sibling");
        let from_cousin =
            resolve(&root.join("a/c"), "../b/target", Lang::TypeScript).expect("cousin");

        assert_eq!(
            from_sibling, from_cousin,
            "different spellings should normalize to the same PathBuf"
        );
    }

    #[test]
    fn bash_is_import_line_tab_separated() {
        // Tab between `source` and path is valid bash and must be detected.
        assert!(
            is_import_line("source\t./lib.sh", Lang::Bash),
            "source<TAB>./lib.sh should be detected as an import line"
        );
        // False positives: `sourcefile=1` looks like it starts with `source` but
        // has no whitespace separator.
        assert!(
            !is_import_line("sourcefile=1", Lang::Bash),
            "sourcefile=1 must not be detected as an import line"
        );
        // `./script.sh` is a script execution, not a source directive.
        assert!(
            !is_import_line("./script.sh", Lang::Bash),
            "./script.sh must not be detected as an import line"
        );
        // `.bashrc` — dot followed by non-whitespace, not a source directive.
        assert!(
            !is_import_line(".bashrc", Lang::Bash),
            ".bashrc must not be detected as an import line"
        );
    }
}
