//! `tilth_deps` output: renders the `DepsResult` that `tilth_core` computes,
//! with budget truncation.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::format::{and_more, noun, rel};
use tilth_core::search::deps::{Dependent, DepsResult, LocalDep, MAX_EXPORTED_SYMBOLS};

/// Dependents an import or the target's package confirms listed before the
/// rest are counted, unless the caller asks for the full list.
const MAX_DEPENDENTS: usize = 15;

/// Dependents found by name only listed before the rest are counted, unless
/// the caller asks for the full list or no import can name the target.
const MAX_BY_NAME: usize = 5;

/// How a dependent is known to reach the target.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    /// One of its imports names the target.
    Imports,
    /// It is in the target's package, which needs no import.
    SamePackage,
    /// It calls a name the target defines, which another definition may own.
    ByName,
}

impl Link {
    /// In the order the lines counting unlisted dependents come.
    const ALL: [Self; 3] = [Self::Imports, Self::SamePackage, Self::ByName];

    fn of(dependent: &Dependent) -> Self {
        if !dependent.import_lines.is_empty() {
            Self::Imports
        } else if dependent.same_package {
            Self::SamePackage
        } else {
            Self::ByName
        }
    }

    /// The mark on the dependent's first row.
    const fn mark(self) -> &'static str {
        match self {
            Self::Imports => "imports it",
            Self::SamePackage => "same package",
            Self::ByName => "by name",
        }
    }

    /// The dependents of this kind, in the line that counts unlisted ones.
    const fn unlisted(self) -> &'static str {
        match self {
            Self::Imports => "importing it",
            Self::SamePackage => "in its package",
            Self::ByName => "by name only",
        }
    }
}

/// Format a `DepsResult` as a compact, readable string.
///
/// Each "Used by" section lists the dependents an import or the target's
/// package confirms first, then those found by name only, and each
/// dependent's first row says which it is. `full` lists every dependent;
/// otherwise the first `MAX_DEPENDENTS` confirmed ones and the first
/// `MAX_BY_NAME` found by name are listed and the rest counted. Where no
/// import can name the target, every dependent found by name is listed:
/// nothing tells them from real ones.
///
/// Budget truncation priority (when `budget` tokens is too tight):
/// 1. Truncate "Used by" entries (keep header count)
/// 2. Truncate "Uses (external)" to count only
/// 3. Truncate "Uses (local)" symbol lists to file paths only
/// 4. Never truncate the header, nor the by-name note that explains its count
///
/// Whenever dependents are left out, a line says how many.
pub fn format_deps(result: &DepsResult, scope: &Path, budget: Option<usize>, full: bool) -> String {
    // The result's paths are canonical; shown relative to the same scope.
    let scope = &scope.canonicalize().unwrap_or_else(|_| scope.to_path_buf());
    let dep_count = result.used_by.len();
    let (confirmed, by_name): (Vec<&Dependent>, Vec<&Dependent>) = result
        .used_by
        .iter()
        .partition(|d| Link::of(d) != Link::ByName);
    let by_name_cap = if result.importable {
        MAX_BY_NAME
    } else {
        usize::MAX
    };
    let cap = |deps: &[&Dependent], max: usize| {
        if full {
            deps.len()
        } else {
            deps.len().min(max)
        }
    };
    let listed: Vec<&Dependent> = confirmed[..cap(&confirmed, MAX_DEPENDENTS)]
        .iter()
        .chain(&by_name[..cap(&by_name, by_name_cap)])
        .copied()
        .collect();
    let section = |is_test: bool| -> Vec<&Dependent> {
        listed
            .iter()
            .filter(|d| d.is_test == is_test)
            .copied()
            .collect()
    };
    let prod_deps = section(false);
    let test_deps = section(true);

    // ── Build sections (full fidelity first) ─────────────────────────────────

    // Header, with the note that explains its by-name count.
    let rel_target = rel(&result.target, scope);
    let mut header = format!(
        "# Deps: {} — {} local, {} external, {} {}",
        rel_target,
        result.uses_local.len(),
        result.uses_external.len(),
        dep_count,
        noun(dep_count, "dependent", "dependents"),
    );
    if !by_name.is_empty() {
        let _ = write!(
            header,
            ", {} of them by name only\n> By name only: the file calls a name the target \
             defines, and no import of the target was found, so the call may reach another \
             definition of the name.",
            by_name.len()
        );
    }

    let uses_local_section = format_uses_local(&result.uses_local, scope, true);
    let uses_external_section = format_uses_external(&result.uses_external);
    let used_by_section = format_used_by(&prod_deps, scope, "## Used by");
    let used_by_tests_section = format_used_by(&test_deps, scope, "## Used by (tests)");
    let unsearched = unsearched_note(result);

    // Full output
    let full_output = assemble(&[
        &header,
        &uses_local_section,
        &uses_external_section,
        &used_by_section,
        &used_by_tests_section,
        &unlisted_dependents(&result.used_by, &listed),
        &unsearched,
    ]);
    let full_tokens = crate::types::estimate_tokens(full_output.len() as u64) as usize;

    let output = match budget {
        None => full_output,
        Some(b) if full_tokens <= b => full_output,
        Some(b) => {
            // Apply truncation in priority order
            apply_budget_truncation(
                &header,
                &uses_local_section,
                &uses_external_section,
                &used_by_section,
                &unlisted_dependents(&result.used_by, &prod_deps),
                dep_count,
                &unsearched,
                b,
            )
        }
    };

    // Kept under any budget: a skipped file may hold dependents nobody checked.
    let held = format!("a name from `{rel_target}`");
    let output = match crate::search::skipped_files_note(&result.skipped_large, &held, scope) {
        Some(sentence) => format!("{output}\n\n> {sentence}"),
        None => output,
    };

    let token_est = crate::types::estimate_tokens(output.len() as u64);
    format!("{output}\n\n[~{token_est} tokens]")
}

/// The note that names what the call-site search left out, or nothing when
/// it searched every name. Importers are found whatever the cap.
fn unsearched_note(result: &DepsResult) -> String {
    /// Left-out names spelled out before the rest are counted.
    const NAMED: usize = 10;
    let left_out = &result.unsearched;
    if left_out.is_empty() {
        return String::new();
    }
    let mut named = left_out
        .iter()
        .take(NAMED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if let Some(more) = and_more(left_out.len(), NAMED) {
        let _ = write!(named, " {more}");
    }
    format!(
        "> Callers searched for {MAX_EXPORTED_SYMBOLS} of {} names: first the ones other \
         files import by name, then the longest. Not searched: {named}.",
        result.exported_count
    )
}

/// The lines that count the dependents of `all` left out of `listed`, one
/// per kind of `Link` with any left out. Empty when all are listed.
fn unlisted_dependents(all: &[Dependent], listed: &[&Dependent]) -> String {
    Link::ALL
        .into_iter()
        .filter_map(|link| {
            let total = all.iter().filter(|d| Link::of(d) == link).count();
            let shown = listed.iter().filter(|d| Link::of(d) == link).count();
            and_more(total, shown).map(|more| format!("... {more} {}", link.unlisted()))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Format the "Uses (local)" section.
fn format_uses_local(deps: &[LocalDep], scope: &Path, with_symbols: bool) -> String {
    if deps.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Uses (local)");
    for dep in deps {
        let rel = rel(&dep.path, scope);
        if with_symbols && !dep.symbols.is_empty() {
            let _ = write!(out, "\n{:<30} {}", rel, dep.symbols.join(", "));
        } else {
            let _ = write!(out, "\n{rel}");
        }
    }
    out
}

/// Format the "Uses (external)" section.
fn format_uses_external(externals: &[String]) -> String {
    if externals.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Uses (external)");
    for ext in externals {
        let _ = write!(out, "\n{ext}");
    }
    out
}

/// Format a "Used by" section from a slice of dependents.
fn format_used_by(deps: &[&Dependent], scope: &Path, heading: &str) -> String {
    if deps.is_empty() {
        return String::new();
    }
    let mut out = String::from(heading);
    for dep in deps {
        let file = rel(&dep.path, scope);
        // Group by (caller, line) for readability — keep the earliest line per caller
        let mut by_caller: HashMap<&str, (u32, Vec<&str>)> = HashMap::new();
        for (caller, symbol, line) in &dep.symbols {
            let entry = by_caller
                .entry(caller.as_str())
                .or_insert((*line, Vec::new()));
            entry.0 = entry.0.min(*line);
            entry.1.push(symbol.as_str());
        }
        let mut callers: Vec<(&str, u32, Vec<&str>)> = by_caller
            .into_iter()
            .map(|(caller, (line, syms))| (caller, line, syms))
            .collect();
        callers.sort_unstable_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
        let how = Link::of(dep).mark();
        // The file's first row says how it was found, and its other rows follow it.
        for (i, (caller, line, syms)) in callers.iter().enumerate() {
            let loc = format!("{file}:{line}");
            let joined = syms.join(", ");
            let _ = write!(out, "\n{loc:<30} {caller:<20} \u{2192} {joined}");
            if i == 0 {
                let _ = write!(out, " ({how})");
            }
        }
        // A file that imports the target without calling it.
        if callers.is_empty() {
            if let Some(line) = dep.import_lines.first() {
                let loc = format!("{file}:{line}");
                match &dep.via {
                    Some(via) => {
                        let via = rel(via, scope);
                        let _ = write!(out, "\n{loc:<30} imports it through {via}");
                    }
                    None => {
                        let _ = write!(out, "\n{loc:<30} imports it");
                    }
                }
            }
        }
    }
    out
}

/// Apply progressive budget truncation and reassemble the output.
#[expect(
    clippy::too_many_arguments,
    reason = "renderers become functions of a result model in #311"
)]
fn apply_budget_truncation(
    header: &str,
    uses_local_full: &str,
    uses_external_full: &str,
    prod_listed: &str,
    prod_unlisted: &str,
    total_dependents: usize,
    unsearched: &str,
    budget: usize,
) -> String {
    // Local deps as paths only, without their symbol lists. Each line is
    // "path_padded  symbols": keep the path.
    let local_paths_only = {
        let local_lines: Vec<&str> = uses_local_full
            .lines()
            .skip(1) // skip heading
            .map(|l| l.split_whitespace().next().unwrap_or(l))
            .collect();
        if local_lines.is_empty() {
            String::new()
        } else {
            format!("## Uses (local)\n{}", local_lines.join("\n"))
        }
    };
    let none_listed = match total_dependents {
        0 => String::new(),
        n => format!("... {n} {} not listed", noun(n, "dependent", "dependents")),
    };

    // Progressively degraded versions, richest first.
    let candidates: [Vec<&str>; 5] = [
        // Level 0: no tests
        vec![
            header,
            uses_local_full,
            uses_external_full,
            prod_listed,
            prod_unlisted,
            unsearched,
        ],
        // Level 1: no used-by entries at all
        vec![
            header,
            uses_local_full,
            uses_external_full,
            &none_listed,
            unsearched,
        ],
        // Level 2: external as count only
        vec![header, uses_local_full, &none_listed, unsearched],
        // Level 3: local as paths only (no symbols)
        vec![header, &local_paths_only, &none_listed],
        // Level 4: header only
        vec![header],
    ];

    for parts in &candidates {
        let candidate = assemble(parts);
        let tokens = crate::types::estimate_tokens(candidate.len() as u64) as usize;
        if tokens <= budget {
            return candidate;
        }
    }

    // Absolute fallback: just the header
    header.to_string()
}

/// Join non-empty parts with double newlines.
fn assemble(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|s| !s.trim().is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 20 dependents, the last 5 of them tests, as `analyze_deps` orders them.
    fn twenty_dependents() -> DepsResult {
        let used_by = (0..20)
            .map(|i| Dependent {
                path: PathBuf::from(format!("/r/user{i:02}.ts")),
                symbols: vec![(format!("user{i}"), "libFn".to_string(), 2)],
                is_test: i >= 15,
                import_lines: vec![1],
                via: None,
                same_package: false,
            })
            .collect();
        DepsResult {
            target: PathBuf::from("/r/lib.ts"),
            uses_local: Vec::new(),
            uses_external: Vec::new(),
            used_by,
            skipped_large: Vec::new(),
            exported_count: 1,
            unsearched: Vec::new(),
            importable: true,
        }
    }

    /// A dependent that imports the target without calling it had no line,
    /// and the old note claimed a barrel file for any file with many names.
    #[test]
    fn importers_and_unsearched_names_are_shown() {
        let dependent = |name: &str, via: Option<&str>| Dependent {
            path: PathBuf::from(format!("/r/{name}")),
            symbols: Vec::new(),
            is_test: false,
            import_lines: vec![3],
            via: via.map(|v| PathBuf::from(format!("/r/{v}"))),
            same_package: false,
        };
        let result = DepsResult {
            target: PathBuf::from("/r/lib.py"),
            uses_local: Vec::new(),
            uses_external: Vec::new(),
            used_by: vec![
                dependent("a.py", None),
                dependent("b.py", Some("pkg/__init__.py")),
            ],
            skipped_large: Vec::new(),
            exported_count: 27,
            unsearched: vec!["get_dependant".to_string(), "x_y".to_string()],
            importable: true,
        };
        let out = format_deps(&result, Path::new("/r"), None, false);
        assert!(
            out.contains("\na.py:3                         imports it\n"),
            "{out}"
        );
        assert!(
            out.contains("\nb.py:3                         imports it through pkg/__init__.py"),
            "{out}"
        );
        assert!(
            out.contains("> Callers searched for 25 of 27 names: first the ones other files import by name, then the longest. Not searched: get_dependant, x_y."),
            "{out}"
        );
        assert!(!out.contains("barrel file detected"), "{out}");
    }

    /// A dependent found only because it calls a name the target defines,
    /// with no import of the target, may call another definition of that
    /// name: on a short name such as `new`, most of them do. The count says
    /// how many are of that kind, so it does not read as exact.
    #[test]
    fn dependents_matched_by_name_only_are_counted_apart() {
        let mut result = twenty_dependents();
        for d in &mut result.used_by[..12] {
            d.import_lines.clear();
        }
        let out = format_deps(&result, Path::new("/r"), None, false);
        assert!(
            out.contains("20 dependents, 12 of them by name only"),
            "{out}"
        );
        assert!(out.contains("> By name only:"), "{out}");
        let all_imported = format_deps(&twenty_dependents(), Path::new("/r"), None, false);
        assert!(!all_imported.contains("by name only"), "{all_imported}");
        assert!(!all_imported.contains("By name only"), "{all_imported}");
    }

    /// #379: `main.rs` reaches `foo.rs` through `mod foo;` and `bar.rs`
    /// through `use`, while `unrelated.rs` calls its own `new` and `render`.
    /// The rows looked the same, so the real dependents could not be told
    /// from the coincidence.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "Windows prints relative paths with backslashes, #426"
    )]
    fn each_dependent_says_whether_an_import_confirms_it() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let files = [
            ("main.rs", "mod foo;\nmod bar;\n\nfn main() {\n    foo::run_engine();\n    let w = foo::Widget::new();\n    w.render();\n}\n"),
            ("foo.rs", "pub struct Widget;\n\nimpl Widget {\n    pub fn new() -> Self { Widget }\n    pub fn render(&self) {}\n}\n\npub fn run_engine() {}\n"),
            ("bar.rs", "use crate::foo::Widget;\nuse super::foo;\n\npub fn build() -> Widget { foo::run_engine(); Widget::new() }\npub fn rebuild() { foo::run_engine(); }\n"),
            // Sorts first by path, so only the rule puts it last.
            ("a_unrelated.rs", "pub struct Other;\nimpl Other { pub fn new() -> Self { Other } pub fn render(&self) {} }\npub fn go() { let o = Other::new(); o.render(); }\n"),
        ];
        for (name, text) in files {
            std::fs::write(src.join(name), text).unwrap();
        }
        let out = crate::run_deps(
            &src.join("foo.rs"),
            tmp.path(),
            None,
            false,
            tilth_core::search::walk::WalkConfig::default(),
        )
        .unwrap();
        assert!(
            out.contains(
                "## Used by\n\
                 src/bar.rs:4                   build                \u{2192} new, run_engine (imports it)\n\
                 src/bar.rs:5                   rebuild              \u{2192} run_engine\n\
                 src/main.rs:5                  main                 \u{2192} new, render, run_engine (imports it)\n\
                 src/a_unrelated.rs:3           go                   \u{2192} new, render (by name)\n"
            ),
            "{out}"
        );
    }

    /// 2 dependents that import the target, then 8 found by name only.
    fn mostly_by_name() -> DepsResult {
        let mut result = twenty_dependents();
        result.used_by.truncate(10);
        for d in &mut result.used_by[..8] {
            d.import_lines.clear();
        }
        result
    }

    /// On tilth's own `src/config.rs`, 42 of 46 rows were coincidental name
    /// matches, and the 15 listed ones hid the 4 an import confirms.
    #[test]
    fn dependents_an_import_confirms_come_first_and_by_name_ones_are_capped() {
        let out = format_deps(&mostly_by_name(), Path::new("/r"), None, false);
        let rows: Vec<&str> = out.lines().filter(|l| l.starts_with("user")).collect();
        assert_eq!(rows.len(), 7, "{out}");
        assert!(rows[0].starts_with("user08.ts:2") && rows[0].ends_with("(imports it)"));
        assert!(rows[1].starts_with("user09.ts:2") && rows[1].ends_with("(imports it)"));
        for (row, n) in rows[2..].iter().zip(0..) {
            assert!(row.starts_with(&format!("user{n:02}.ts:2")), "{out}");
            assert!(row.ends_with("(by name)"), "{out}");
        }
        assert!(out.contains("\n... and 3 more by name only"), "{out}");
        assert!(!out.contains("importing it"), "{out}");

        let full = format_deps(&mostly_by_name(), Path::new("/r"), None, true);
        assert_eq!(listed(&full), 10, "{full}");
        assert!(!full.contains("more by name only"), "{full}");
    }

    /// The header kept the by-name count under a budget while the note that
    /// explains it was cut first.
    #[test]
    fn the_by_name_note_stays_with_the_header_under_any_budget() {
        let result = mostly_by_name();
        for budget in [None, Some(60), Some(40), Some(1)] {
            let out = format_deps(&result, Path::new("/r"), budget, false);
            assert!(
                out.starts_with(
                    "# Deps: lib.ts — 0 local, 0 external, 10 dependents, 8 of them by name only\n\
                     > By name only: "
                ),
                "{budget:?}: {out}"
            );
        }
    }

    /// #379: Go files of one package call each other with no import, so
    /// each was a dependent by name only and capped with the coincidences,
    /// though a change to the target breaks them.
    #[test]
    #[cfg_attr(
        windows,
        ignore = "Windows prints relative paths with backslashes, #426"
    )]
    fn dependents_in_the_target_package_are_confirmed() {
        let tmp = tempfile::tempdir().unwrap();
        let files = [
            (
                "pkg/target.go",
                "package pkg\n\nfunc Helper() int { return 1 }\n",
            ),
            (
                "pkg/same.go",
                "package pkg\n\nfunc useIt() int { return Helper() }\n",
            ),
            (
                "other/far.go",
                "package other\n\nfunc far() int { return Helper() }\n",
            ),
        ];
        for (rel, text) in files {
            let path = tmp.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let out = crate::run_deps(
            &tmp.path().join("pkg/target.go"),
            tmp.path(),
            None,
            false,
            tilth_core::search::walk::WalkConfig::default(),
        )
        .unwrap();
        assert!(
            out.contains(
                "## Used by\n\
                 pkg/same.go:3                  useIt                \u{2192} Helper (same package)\n\
                 other/far.go:3                 far                  \u{2192} Helper (by name)\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("2 dependents, 1 of them by name only"),
            "{out}"
        );
    }

    /// Past the cap, the dependents in the target's package are counted
    /// apart from those that import it.
    #[test]
    fn unlisted_dependents_are_counted_by_how_they_reach_the_target() {
        let mut result = twenty_dependents();
        for d in &mut result.used_by[..10] {
            d.import_lines.clear();
            d.same_package = true;
        }
        let out = format_deps(&result, Path::new("/r"), None, false);
        let rows: Vec<&str> = out.lines().filter(|l| l.starts_with("user")).collect();
        assert_eq!(rows.len(), 15, "{out}");
        assert!(rows[0].ends_with("(same package)"), "{out}");
        assert!(rows[14].ends_with("(imports it)"), "{out}");
        assert!(out.contains("\n... and 5 more importing it"), "{out}");
        assert!(!out.contains("in its package"), "{out}");
        assert!(!out.contains("by name"), "{out}");

        for d in &mut result.used_by[10..] {
            d.import_lines.clear();
            d.same_package = true;
        }
        let out = format_deps(&result, Path::new("/r"), None, false);
        assert!(out.contains("\n... and 5 more in its package"), "{out}");
        assert!(!out.contains("importing it"), "{out}");
    }

    /// Where no import can name the target (a Go or Java file, a C source
    /// file), every dependent not in its package is found by name, and a
    /// cap would hide real ones with nothing to tell them apart.
    #[test]
    fn dependents_by_name_are_all_listed_when_no_import_can_name_the_target() {
        let mut result = mostly_by_name();
        result.importable = false;
        let out = format_deps(&result, Path::new("/r"), None, false);
        assert_eq!(listed(&out), 10, "{out}");
        assert!(!out.contains("more by name only"), "{out}");
        assert!(
            out.contains("10 dependents, 8 of them by name only"),
            "{out}"
        );
    }

    fn listed(out: &str) -> usize {
        out.lines().filter(|l| l.starts_with("user")).count()
    }

    #[test]
    fn default_lists_fifteen_and_counts_the_rest() {
        let out = format_deps(&twenty_dependents(), Path::new("/r"), None, false);
        assert!(out.contains("20 dependents"), "{out}");
        assert_eq!(listed(&out), 15, "{out}");
        assert!(out.contains("... and 5 more importing it"), "{out}");
    }

    /// `--full` used to change nothing: the cap sat in the analysis.
    #[test]
    fn full_lists_every_dependent() {
        let out = format_deps(&twenty_dependents(), Path::new("/r"), None, true);
        assert_eq!(listed(&out), 20, "{out}");
        assert!(out.contains("## Used by (tests)"), "{out}");
        assert!(!out.contains("more importing it"), "{out}");
    }

    /// Under a budget the tests section went first with nothing said, and
    /// the next level counted the dependents it showed as the ones it hid.
    #[test]
    fn a_budget_cut_says_how_many_dependents_are_not_listed() {
        let result = twenty_dependents();
        let full = format_deps(&result, Path::new("/r"), None, true);
        let full_tokens = crate::types::estimate_tokens(full.len() as u64) as usize;

        let no_tests = format_deps(&result, Path::new("/r"), Some(full_tokens - 20), true);
        assert_eq!(listed(&no_tests), 15, "{no_tests}");
        assert!(
            no_tests.contains("... and 5 more importing it"),
            "{no_tests}"
        );

        let none = format_deps(&result, Path::new("/r"), Some(30), true);
        assert_eq!(listed(&none), 0, "{none}");
        assert!(none.contains("... 20 dependents not listed"), "{none}");
    }
}
