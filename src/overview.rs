//! Project fingerprint for MCP initialization.
//! Gives agents instant orientation without a tool call.

use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::format::{self, noun};
use crate::git::Git;
use tilth_core::lang::detect_file_type;
use tilth_core::search::walk::{base_walk_builder, WalkConfig};
use tilth_core::shown;
use tilth_core::types::{FileType, Lang};

/// How many directories the `dirs:` line lists before it counts the rest.
const DIRS_SHOWN: usize = 10;
/// How many dependencies the `deps:` line lists before it counts the rest.
const DEPS_SHOWN: usize = 10;
/// How many hot files the `hot` line lists before it counts the rest.
const HOT_SHOWN: usize = 5;
/// How many walk entries the file count looks at before it stops and
/// shows its count as a lower bound ("N+ source files"), so a huge tree
/// stays within the overview's time limit.
const COUNT_CAP: usize = 10_000;
/// The walk depth of the deepest file the `dirs`, `hot` and `tests` lines
/// look at: one two directories below the root.
const SAMPLE_DEPTH: usize = 3;

/// How long the overview may take, all its parts together. It runs
/// synchronously in the MCP `initialize` handler, so the session waits on it.
pub const LIMIT: Duration = Duration::from_millis(250);

/// Compute a project fingerprint for MCP initialization, walking `root`
/// by the `walk` settings every search uses.
///
/// Every part stops at `deadline`, and a part that has not finished by then
/// is left out: a file count cut short reads as a lower bound (`N+`), and a
/// deadline already past gives an empty string. A part checks the deadline
/// between its steps, so one step that blocks (a slow read or stat) can
/// still run past it.
///
/// Returns empty string on any failure (no error propagation).
#[must_use]
pub fn fingerprint(root: &Path, walk: WalkConfig, deadline: Instant) -> String {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        #[cfg(feature = "panic-probe")]
        assert!(
            !root.ends_with("tilth-panic-probe"),
            "the overview panics on purpose"
        );
        fingerprint_inner(root, walk, COUNT_CAP, deadline)
    }));
    if let Some(over) = Instant::now().checked_duration_since(deadline) {
        crate::diag::emit(format_args!(
            "[tilth] the overview stopped at its deadline ({}ms over) and left out what it had not finished",
            over.as_millis()
        ));
    }
    result.unwrap_or_default()
}

/// Whether `deadline` has passed, so no new step may start.
fn past(deadline: Instant) -> bool {
    Instant::now() >= deadline
}

fn fingerprint_inner(root: &Path, walk: WalkConfig, cap: usize, deadline: Instant) -> String {
    let mut lines: Vec<String> = Vec::new();

    // Out of time before the walk began, so there is nothing to describe.
    let Some(count) = count_sources(root, walk, cap, deadline) else {
        return String::new();
    };
    // The `dirs`, `hot` and `tests` lines and the header's directory count
    // describe this sample, so each is left out when it is cut short.
    let sample = sample_files(root, walk, deadline);

    // Primary language: the most files, and on a tie the name first in
    // alphabetical order, since the map iterates in a random order.
    let primary_lang = count
        .by_lang
        .iter()
        .max_by(|(a, a_count), (b, b_count)| {
            a_count
                .cmp(b_count)
                .then_with(|| lang_display_name(**b).cmp(lang_display_name(**a)))
        })
        .map(|(lang, _)| *lang);

    let lang_name = primary_lang.map_or("Unknown", lang_display_name);
    let total_files = primary_lang
        .and_then(|l| count.by_lang.get(&l))
        .copied()
        .unwrap_or_else(|| count.by_lang.values().sum::<usize>());
    let at_least = if count.capped { "+" } else { "" };

    let modules = sample.as_ref().map(|sample| modules(sample, primary_lang));

    // Header line
    let mut header = format!(
        "[tilth] {lang_name} project — {total_files}{at_least} source {}",
        noun(total_files, "file", "files")
    );
    if let Some(modules) = &modules {
        let dir_count = modules.len();
        let _ = write!(
            header,
            ", {dir_count} {}",
            noun(dir_count, "directory", "directories")
        );
    }
    lines.push(header);

    // Directories, most files first
    if let Some(modules) = modules.filter(|modules| !modules.is_empty()) {
        let mut dirs: Vec<String> = modules
            .iter()
            .take(DIRS_SHOWN)
            .map(|m| format!("{}/", shown::path(m)))
            .collect();
        dirs.extend(format::and_more(modules.len(), DIRS_SHOWN));
        lines.push(format!("  dirs: {}", dirs.join(" ")));
    }

    // Manifest — name, version, deps
    let manifest = if past(deadline) {
        None
    } else {
        find_manifest(root)
    };
    if let Some(manifest) = manifest {
        if let Some(info) = parse_manifest(root, &manifest) {
            // Deps line
            if !info.deps.is_empty() {
                let dep_str = format::capped_list(info.deps.iter(), DEPS_SHOWN);
                lines.push(format!("  deps: {dep_str}"));
            }

            // Hot files (only for projects with local imports)
            if let Some(hot) = hot_files(root, sample.as_ref(), primary_lang, deadline) {
                lines.push(format!("  hot (× = importers): {hot}"));
            }

            // Git context
            if let Some(git) = git_context(root, deadline) {
                lines.push(format!("  git: {git}"));
            }

            // Test style
            if let Some(tests) = test_style(root, sample.as_ref(), primary_lang, deadline) {
                lines.push(format!("  tests: {tests}"));
            }

            // Manifest line
            let mut manifest_line = format!("  manifest: {manifest}");
            if let Some(name) = &info.name {
                let _ = write!(manifest_line, " ({name}");
                if let Some(version) = &info.version {
                    let _ = write!(manifest_line, " v{version}");
                }
                manifest_line.push(')');
            }
            lines.push(manifest_line);
        }
    } else {
        // No manifest — still show hot, git, tests
        if let Some(hot) = hot_files(root, sample.as_ref(), primary_lang, deadline) {
            lines.push(format!("  hot (× = importers): {hot}"));
        }
        if let Some(git) = git_context(root, deadline) {
            lines.push(format!("  git: {git}"));
        }
        if let Some(tests) = test_style(root, sample.as_ref(), primary_lang, deadline) {
            lines.push(format!("  tests: {tests}"));
        }
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Modules
// ---------------------------------------------------------------------------

/// The directories the `dirs` line lists, most files first: those with two
/// or more files of the primary language, with their common prefix
/// stripped. Keys in `module_lang_counts` may be "dir" or "dir/subdir" (for
/// deeply nested projects).
fn modules(sample: &Sample, primary_lang: Option<Lang>) -> Vec<String> {
    // Collect dirs with >=2 primary-language files
    let mut mods: Vec<(String, usize)> = sample
        .module_lang_counts
        .iter()
        .filter_map(|(name, lang_map)| {
            let count = primary_lang
                .and_then(|l| lang_map.get(&l))
                .copied()
                .unwrap_or(0);
            if count >= 2 {
                Some((name.clone(), count))
            } else {
                None
            }
        })
        .collect();

    // If all modules (or at least most) share a common top-level prefix
    // (e.g., all are "src/..."), strip it so we display short names
    // ("diff/" not "src/diff/"). Also exclude the bare prefix entry itself.
    if mods.len() >= 2 {
        let prefix = common_dir_prefix(&mods);
        if !prefix.is_empty() {
            // The prefix without trailing slash (e.g., "src")
            let prefix_bare = prefix.trim_end_matches('/');
            mods = mods
                .into_iter()
                .filter_map(|(name, count)| {
                    if name == prefix_bare {
                        // Drop the bare prefix itself (it's the container, not a module)
                        None
                    } else if let Some(stripped) = name.strip_prefix(&prefix) {
                        let s = stripped.trim_start_matches('/');
                        if s.is_empty() {
                            None
                        } else {
                            Some((s.to_string(), count))
                        }
                    } else {
                        Some((name, count))
                    }
                })
                .collect();
        }
    }
    // Filter out well-known non-source directories
    let non_source = [
        "test",
        "tests",
        "__tests__",
        "spec",
        "specs",
        "doc",
        "docs",
        "docs_src",
        "documentation",
        "example",
        "examples",
        "sample",
        "samples",
        "script",
        "scripts",
        "tools",
        "fixtures",
        "benchmark",
        "benchmarks",
        "bench",
        ".github",
        ".vscode",
        ".idea",
        "vendor",
        "node_modules",
        "target",
        "dist",
        "build",
    ];
    mods.retain(|(name, _)| {
        let lower = name.to_lowercase();
        // Check if ANY path component is a non-source dir
        !lower.split('/').any(|part| non_source.contains(&part))
    });
    // Most files first, ties by name. The names come out of a hash map
    // in a random order, so the name decides every tie.
    mods.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    mods.into_iter().map(|(name, _)| name).collect()
}

// ---------------------------------------------------------------------------
// Common dir prefix helper
// ---------------------------------------------------------------------------

/// If all module names (which may be "a/b" style) share the same first path
/// component, return that component followed by "/". Otherwise return "".
fn common_dir_prefix(mods: &[(String, usize)]) -> String {
    if mods.is_empty() {
        return String::new();
    }
    // Extract the first path component from each name
    let first_components: Vec<&str> = mods
        .iter()
        .map(|(n, _)| n.split('/').next().unwrap_or(n))
        .collect();
    let first = first_components[0];
    if first_components.iter().all(|c| *c == first) && mods.iter().any(|(n, _)| n.contains('/')) {
        // All share the same first component and at least some have a subdir
        format!("{first}/")
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Language display
// ---------------------------------------------------------------------------

fn lang_display_name(lang: Lang) -> &'static str {
    tilth_core::lang::spec::spec(lang).display
}

// ---------------------------------------------------------------------------
// File walks
// ---------------------------------------------------------------------------

/// Source files per language, from a walk of the whole tree that may stop
/// at its cap or its deadline.
struct SourceCount {
    by_lang: HashMap<Lang, usize>,
    /// Whether the walk stopped with entries left, so the tree may hold
    /// more files than were counted.
    capped: bool,
}

/// Count the source files under `root` by the walk policy every search
/// uses. The walk looks at no more than `cap` entries and stops at
/// `deadline`, so a huge tree stays within the overview's time limit and
/// its count reads as a lower bound. `None` when the deadline had passed
/// before the walk began.
fn count_sources(
    root: &Path,
    walk: WalkConfig,
    cap: usize,
    deadline: Instant,
) -> Option<SourceCount> {
    if past(deadline) {
        return None;
    }
    let mut by_lang: HashMap<Lang, usize> = HashMap::new();
    let mut entries = base_walk_builder(root, walk).build().flatten();
    let mut seen = 0;
    let capped = loop {
        let Some(entry) = entries.next() else {
            break false;
        };
        if seen == cap || past(deadline) {
            break true;
        }
        seen += 1;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if let FileType::Code(lang) = detect_file_type(entry.path()) {
            *by_lang.entry(lang).or_insert(0) += 1;
        }
    };
    Some(SourceCount { by_lang, capped })
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "by design: one flag per test-directory convention, read by test_style"
)]
#[derive(Default)]
struct Sample {
    /// Top-level dirs → per-language file counts
    module_lang_counts: HashMap<String, HashMap<Lang, usize>>,
    /// Code files found: (path relative to root, size in bytes)
    code_files: Vec<(String, u64)>,
    /// Whether specific test dirs exist
    has_tests_dir: bool,
    has_test_dir: bool,
    has_dunder_tests: bool,
    has_spec_dir: bool,
}

/// The top of the tree the `dirs`, `hot` and `tests` lines describe: files
/// up to two directories below the root, and directories down to that depth.
/// `None` when `deadline` passes before the walk is done.
fn sample_files(root: &Path, walk: WalkConfig, deadline: Instant) -> Option<Sample> {
    let mut sample = Sample::default();
    let entries = base_walk_builder(root, walk)
        .max_depth(Some(SAMPLE_DEPTH))
        .build()
        .flatten();
    for entry in entries {
        if past(deadline) {
            return None;
        }
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            // The root is the project, never one of its test directories.
            if entry.depth() == 0 {
                continue;
            }
            match entry.file_name().to_str() {
                Some("tests") => sample.has_tests_dir = true,
                Some("test") => sample.has_test_dir = true,
                Some("__tests__") => sample.has_dunder_tests = true,
                Some("spec") => sample.has_spec_dir = true,
                _ => {}
            }
        } else if ft.is_file() {
            let path = entry.path();
            let FileType::Code(lang) = detect_file_type(path) else {
                continue;
            };
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            // Track size for hot files
            let size = entry.metadata().map_or(0, |m| m.len());
            sample
                .code_files
                .push((rel.to_string_lossy().to_string(), size));

            // Track module — use up to 2 path components as the key,
            // but only for files nested at least one level deep.
            // e.g. src/diff/mod.rs → key "src/diff", lib.rs → skipped
            let dirs: Vec<_> = rel
                .parent()
                .into_iter()
                .flat_map(Path::components)
                .take(2)
                .map(|c| c.as_os_str().to_string_lossy())
                .collect();
            if !dirs.is_empty() {
                *sample
                    .module_lang_counts
                    .entry(dirs.join("/"))
                    .or_default()
                    .entry(lang)
                    .or_insert(0) += 1;
            }
        }
    }
    Some(sample)
}

// ---------------------------------------------------------------------------
// Manifest parsing
// ---------------------------------------------------------------------------

fn find_manifest(root: &Path) -> Option<String> {
    const MANIFESTS: &[&str] = &["Cargo.toml", "package.json", "go.mod", "pyproject.toml"];
    for m in MANIFESTS {
        if root.join(m).exists() {
            return Some((*m).to_string());
        }
    }
    None
}

struct ManifestInfo {
    name: Option<String>,
    version: Option<String>,
    deps: Vec<String>,
}

fn parse_manifest(root: &Path, manifest: &str) -> Option<ManifestInfo> {
    match manifest {
        "Cargo.toml" => parse_cargo_toml(root),
        "package.json" => parse_package_json(root),
        "go.mod" => parse_go_mod(root),
        "pyproject.toml" => parse_pyproject_toml(root),
        _ => None,
    }
}

fn parse_cargo_toml(root: &Path) -> Option<ManifestInfo> {
    #[derive(Deserialize)]
    struct CargoToml {
        package: Option<Package>,
        dependencies: Option<toml::Table>,
    }
    #[derive(Deserialize)]
    struct Package {
        name: Option<String>,
        version: Option<String>,
    }

    let content = tilth_core::source::read_source(&root.join("Cargo.toml"))
        .ok()
        .flatten()?;
    let parsed: CargoToml = toml::from_str(&content).ok()?;
    let (name, version) = parsed.package.map_or((None, None), |p| (p.name, p.version));
    let mut deps: Vec<String> = parsed
        .dependencies
        .map(|d| d.into_iter().map(|(k, _)| k).collect())
        .unwrap_or_default();
    deps.sort();
    Some(ManifestInfo {
        name,
        version,
        deps,
    })
}

fn parse_package_json(root: &Path) -> Option<ManifestInfo> {
    #[derive(Deserialize)]
    struct PackageJson {
        name: Option<String>,
        version: Option<String>,
        dependencies: Option<serde_json::Map<String, serde_json::Value>>,
    }

    let content = tilth_core::source::read_source(&root.join("package.json"))
        .ok()
        .flatten()?;
    let parsed: PackageJson = serde_json::from_str(&content).ok()?;
    let mut deps: Vec<String> = parsed
        .dependencies
        .map(|d| d.into_iter().map(|(k, _)| k).collect())
        .unwrap_or_default();
    deps.sort();
    Some(ManifestInfo {
        name: parsed.name,
        version: parsed.version,
        deps,
    })
}

fn parse_go_mod(root: &Path) -> Option<ManifestInfo> {
    let content = tilth_core::source::read_source(&root.join("go.mod"))
        .ok()
        .flatten()?;
    let mut name = None;
    let mut deps: Vec<String> = Vec::new();
    let mut in_require = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("module ") {
            name = Some(rest.trim().to_string());
        }
        if trimmed == "require (" {
            in_require = true;
            continue;
        }
        if trimmed == ")" {
            in_require = false;
            continue;
        }
        if in_require {
            // e.g. "github.com/gin-gonic/gin v1.9.0"
            if let Some(dep) = trimmed.split_whitespace().next() {
                if !dep.starts_with("//") {
                    // Use short name (last segment of module path)
                    let short = dep.rsplit('/').next().unwrap_or(dep);
                    deps.push(short.to_string());
                }
            }
        }
    }

    deps.sort();

    Some(ManifestInfo {
        name,
        version: None,
        deps,
    })
}

fn parse_pyproject_toml(root: &Path) -> Option<ManifestInfo> {
    #[derive(Default, Deserialize)]
    struct PyProject {
        project: Option<Project>,
    }
    #[derive(Default, Deserialize)]
    struct Project {
        name: Option<String>,
        version: Option<String>,
        dependencies: Option<Vec<String>>,
    }

    let content = tilth_core::source::read_source(&root.join("pyproject.toml"))
        .ok()
        .flatten()?;
    let parsed: PyProject = toml::from_str(&content).ok()?;
    let project = parsed.project.unwrap_or_default();
    let mut deps: Vec<String> = project
        .dependencies
        .unwrap_or_default()
        .into_iter()
        .filter_map(|spec| {
            let bare = spec
                .split(&['>', '<', '=', '~', '!', ';', '[', ' '][..])
                .next()?
                .trim();
            (!bare.is_empty()).then(|| bare.to_string())
        })
        .collect();
    deps.sort();
    Some(ManifestInfo {
        name: project.name,
        version: project.version,
        deps,
    })
}

// ---------------------------------------------------------------------------
// Git context
// ---------------------------------------------------------------------------

/// The `git:` line: the branch, or the short commit id on a detached HEAD,
/// and how many files have uncommitted changes.
///
/// `None` unless git answered in full by `deadline`. A count git could not
/// finish is left out with the rest of the line rather than shown as
/// `clean` (#356). Every failure is swallowed: git context is a cosmetic
/// fingerprint and must never break the primary read/search path.
fn git_context(root: &Path, deadline: Instant) -> Option<String> {
    // One command gives both the branch and the changes. Every header line
    // starts with `# `, and every other line is one changed file.
    let status = Git::new(root, "status")
        .args(&["--porcelain=v2", "--branch"])
        .deadline(deadline)
        .run()
        .ok()?;
    let head = status
        .lines()
        .find_map(|line| line.strip_prefix("# branch.head "))?;
    let changes = status
        .lines()
        .filter(|line| !line.starts_with("# "))
        .count();

    let branch = if head == "(detached)" {
        Git::new(root, "rev-parse")
            .args(&["--short", "HEAD"])
            .deadline(deadline)
            .run()
            .ok()?
            .trim()
            .to_string()
    } else {
        head.to_string()
    };

    let dirty_str = if changes == 0 {
        "clean".to_string()
    } else {
        format!(
            "{changes} uncommitted {}",
            crate::format::noun(changes, "file", "files")
        )
    };

    Some(format!("branch {branch}, {dirty_str}"))
}

// ---------------------------------------------------------------------------
// Test style detection
// ---------------------------------------------------------------------------

/// The test conventions the sample shows. `None` when there are none, or
/// when `deadline` passes first.
fn test_style(
    root: &Path,
    sample: Option<&Sample>,
    primary_lang: Option<Lang>,
    deadline: Instant,
) -> Option<String> {
    let sample = sample.filter(|_| !past(deadline))?;
    let mut styles: Vec<String> = Vec::new();

    // Directory-based test detection
    if sample.has_tests_dir {
        styles.push("tests/".to_string());
    }
    if sample.has_test_dir {
        styles.push("test/".to_string());
    }
    if sample.has_dunder_tests {
        styles.push("__tests__/".to_string());
    }
    if sample.has_spec_dir {
        styles.push("spec/".to_string());
    }

    // File pattern detection
    let has_test_files = sample
        .code_files
        .iter()
        .any(|(path, _)| path.contains(".test.") || path.contains(".spec."));
    let has_go_tests = sample
        .code_files
        .iter()
        .any(|(path, _)| path.ends_with("_test.go"));
    let has_py_tests = sample
        .code_files
        .iter()
        .any(|(path, _)| path.starts_with("test_") || path.contains("/test_"));

    if has_test_files && !sample.has_dunder_tests {
        styles.push("*.test/spec files".to_string());
    }
    if has_go_tests {
        styles.push("_test.go".to_string());
    }
    if has_py_tests {
        styles.push("test_*.py".to_string());
    }

    // Rust in-source test detection
    if primary_lang == Some(Lang::Rust) {
        let rust_files = sample
            .code_files
            .iter()
            .filter(|(path, _)| {
                Path::new(path)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
            })
            .take(5);
        let mut has_cfg_test = false;
        for (path, _) in rust_files {
            if past(deadline) {
                return None;
            }
            let full = root.join(path);
            if tilth_core::source::read_source(&full)
                .is_ok_and(|content| content.is_some_and(|c| c.contains("#[cfg(test)]")))
            {
                has_cfg_test = true;
                break;
            }
        }
        if has_cfg_test {
            styles.push("in-source #[cfg(test)]".to_string());
        }
    }

    if styles.is_empty() {
        None
    } else {
        Some(styles.join(", "))
    }
}

// ---------------------------------------------------------------------------
// Hot files — most imported local files
// ---------------------------------------------------------------------------

/// The most imported local files, from the imports of the 100 smallest code
/// files. The file limit is a count, so a loaded machine reads the same
/// files as an idle one (#281); a `deadline` that passes before they are
/// all read leaves the line out rather than show a partial count.
fn hot_files(
    root: &Path,
    sample: Option<&Sample>,
    primary_lang: Option<Lang>,
    deadline: Instant,
) -> Option<String> {
    primary_lang?; // require a detected language
    let sample = sample?;

    // Sort by size (smallest first) and take first 100
    let mut files: Vec<&(String, u64)> = sample.code_files.iter().collect();
    files.sort_by_key(|(_, size)| *size);
    files.truncate(100);

    // Use resolve_related_files to get real file paths for imports.
    // Count how many files import each target path.
    let mut path_counts: HashMap<std::path::PathBuf, usize> = HashMap::new();
    // Also collect every local import source for symbol extraction later
    let mut all_import_sources: Vec<String> = Vec::new();

    for (rel_path, _) in &files {
        if past(deadline) {
            return None;
        }
        let full = root.join(rel_path);
        let Ok(Some(content)) = tilth_core::source::read_source(&full) else {
            continue;
        };

        // Each file is read in its own language, as the resolver always did.
        let FileType::Code(file_lang) =
            tilth_core::lang::detect_file_type_with_content(&full, &content)
        else {
            continue;
        };

        // Resolve imports to actual file paths using the proven import
        // resolver, which counts a Rust `mod` declaration as an import too.
        for target_path in tilth_core::resolve_related_files_with_content(&full, &content) {
            *path_counts.entry(target_path).or_insert(0) += 1;
        }

        // Collect import source strings for symbol extraction
        for import in tilth_core::extract_imports(&content, file_lang) {
            if !tilth_core::imports::is_external(&import.source, file_lang) {
                all_import_sources.push(import.source);
            }
        }
    }

    // A file is hot when two or more files import it. Most importers first,
    // ties by path.
    let mut sorted: Vec<(std::path::PathBuf, usize)> = path_counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    // For each hot file shown, find the most commonly imported symbol by
    // scanning import sources that reference this file's module name.
    let parts = sorted.iter().map(|(path, count)| {
        let rel = path.strip_prefix(root).unwrap_or(path);
        let rel_str = shown::path(rel).to_string();

        // Derive the module name from the file path
        // src/types.rs → "types", src/lang/mod.rs → "lang", src/error.rs → "error"
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let module_name = if stem == "mod" || stem == "index" || stem == "__init__" {
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or(stem)
        } else {
            stem
        };

        // Count symbols imported from this module across all import sources
        let mut symbol_counts: HashMap<String, usize> = HashMap::new();
        for source in &all_import_sources {
            // Match imports that reference this module
            // e.g., for module "types": "tilth_core::types::OutlineEntry" matches
            let segments: Vec<&str> = source.split("::").collect();
            if let Some(mod_pos) = segments.iter().position(|s| *s == module_name) {
                // Everything after the module name is a symbol path
                for &sym in segments.iter().skip(mod_pos + 1) {
                    if !sym.is_empty() && !sym.contains('*') && !sym.contains('{') && sym != "self"
                    {
                        *symbol_counts.entry(sym.to_string()).or_insert(0) += 1;
                    }
                }
            }
        }

        // Pick the most frequently imported symbol (break ties alphabetically for determinism)
        let top_sym = symbol_counts
            .into_iter()
            .max_by(|(a_sym, a_c), (b_sym, b_c)| a_c.cmp(b_c).then(b_sym.cmp(a_sym)))
            .map(|(sym, _)| sym);

        top_sym.map_or_else(
            || format!("{rel_str} ×{count}"),
            |sym| format!("{rel_str}({sym}) ×{count}"),
        )
    });

    Some(format::capped_list(parts, HOT_SHOWN))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::git;
    use std::fs;

    #[test]
    fn test_fingerprint_on_tilth() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let output = fingerprint(root, WalkConfig::default(), far_off());

        assert!(!output.is_empty(), "fingerprint should not be empty");
        assert!(
            output.contains("Rust"),
            "should detect Rust as primary language"
        );
        assert!(output.contains("Cargo.toml"), "should detect manifest");
        assert!(output.contains("tilth"), "should find project name");

        // Token budget: output should be compact
        let estimated_tokens = output.len() / 4;
        assert!(
            estimated_tokens < 300,
            "fingerprint should be <300 tokens, got {estimated_tokens}"
        );
    }

    /// A file imported only through `use` groups is still hot, and the
    /// symbol named most inside the groups is the one shown. Read as one
    /// source per line, each group was `crate` and named no file.
    #[test]
    fn hot_files_count_imports_inside_use_groups() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("types.rs"), "pub struct Outline;\n").unwrap();
        fs::write(src.join("other.rs"), "").unwrap();
        for user in ["a.rs", "b.rs"] {
            fs::write(
                src.join(user),
                "use crate::{other, types::Outline};\nfn f(_: Outline) {}\n",
            )
            .unwrap();
        }
        let sample = sample_files(tmp.path(), WalkConfig::default(), far_off());
        let hot = hot_files(tmp.path(), sample.as_ref(), Some(Lang::Rust), far_off());
        assert_eq!(
            hot.as_deref(),
            Some("src/other.rs ×2, src/types.rs(Outline) ×2")
        );
    }

    /// Counts that tie are ordered by name, so the same tree gives the same
    /// overview on every build. The counts came out of hash maps, and each
    /// map iterates in its own random order, so ties used to land anywhere
    /// (#281). Here eight directories tie on file count and two languages
    /// tie as the primary one.
    #[test]
    fn tied_counts_give_the_same_overview_every_time() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = [
            "golf", "alpha", "hotel", "delta", "bravo", "foxtrot", "charlie", "echo",
        ];
        for dir in dirs {
            let path = tmp.path().join(dir);
            fs::create_dir_all(&path).unwrap();
            for file in ["a.py", "b.py", "a.rs", "b.rs"] {
                fs::write(path.join(file), "x = 1\n").unwrap();
            }
        }
        let expected = "[tilth] Python project — 16 source files, 8 directories\n  \
             dirs: alpha/ bravo/ charlie/ delta/ echo/ foxtrot/ golf/ hotel/";
        for _ in 0..10 {
            assert_eq!(
                fingerprint(tmp.path(), WalkConfig::default(), far_off()),
                expected
            );
        }
    }

    /// A tree of `n` directories that each hold two Python files.
    fn tree_of_dirs(n: usize) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..n {
            let dir = tmp.path().join(format!("d{i:02}"));
            fs::create_dir_all(&dir).unwrap();
            for file in ["a.py", "b.py"] {
                fs::write(dir.join(file), "x = 1\n").unwrap();
            }
        }
        tmp
    }

    /// The summary listed ten directories and counted only those ten in its
    /// header, so a tree with more read as if it had ten (#343).
    #[test]
    fn the_startup_summary_counts_the_dirs_it_leaves_out() {
        let tmp = tree_of_dirs(DIRS_SHOWN + 1);
        let expected = "[tilth] Python project — 22 source files, 11 directories\n  \
             dirs: d00/ d01/ d02/ d03/ d04/ d05/ d06/ d07/ d08/ d09/ and 1 more";
        assert_eq!(
            fingerprint(tmp.path(), WalkConfig::default(), far_off()),
            expected
        );

        let tmp = tree_of_dirs(DIRS_SHOWN);
        let expected = "[tilth] Python project — 20 source files, 10 directories\n  \
             dirs: d00/ d01/ d02/ d03/ d04/ d05/ d06/ d07/ d08/ d09/";
        assert_eq!(
            fingerprint(tmp.path(), WalkConfig::default(), far_off()),
            expected
        );
    }

    /// A tree holding each of `files`, a path relative to its root, with
    /// one line of Python in it.
    fn tree_of_files(files: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for file in files {
            let path = tmp.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x = 1\n").unwrap();
        }
        tmp
    }

    /// The header counted the files of the top two directory levels and
    /// called that the project's size (#383).
    #[test]
    fn the_file_count_covers_the_whole_tree() {
        let tmp = tree_of_files(&[
            "pkg/mod/a.py",
            "pkg/mod/b.py",
            "pkg/mod/sub/c.py",
            "pkg/mod/sub/deeper/d.py",
        ]);
        assert_eq!(
            fingerprint(tmp.path(), WalkConfig::default(), far_off()),
            "[tilth] Python project — 4 source files, 1 directory\n  dirs: pkg/mod/"
        );
    }

    /// The overview walked by hand, so it counted what `.tilthignore` hides
    /// from every search (#383).
    #[test]
    fn the_overview_skips_what_tilthignore_hides() {
        let tmp = tree_of_files(&["app/a.py", "app/b.py", "gen/x.py", "gen/y.py", "gen/z.py"]);
        fs::write(tmp.path().join(".tilthignore"), "gen/\n").unwrap();
        assert_eq!(
            fingerprint(tmp.path(), WalkConfig::default(), far_off()),
            "[tilth] Python project — 2 source files, 1 directory\n  dirs: app/"
        );
    }

    /// The count follows the gitignore setting, as every search does.
    #[test]
    fn the_file_count_follows_the_gitignore_setting() {
        let tmp = tree_of_files(&["app/a.py", "app/b.py", "gen/x.py"]);
        fs::write(tmp.path().join(".gitignore"), "gen/\n").unwrap();
        let header = |respect_gitignore| {
            let walk = WalkConfig {
                respect_gitignore,
                ..WalkConfig::default()
            };
            fingerprint(tmp.path(), walk, far_off())
                .lines()
                .next()
                .map(str::to_string)
        };
        assert_eq!(
            header(false).as_deref(),
            Some("[tilth] Python project — 3 source files, 1 directory")
        );
        assert_eq!(
            header(true).as_deref(),
            Some("[tilth] Python project — 2 source files, 1 directory")
        );
    }

    /// A walk that stops at its cap shows its count as a lower bound,
    /// never as an exact size (#383). The walk's entries are the root, the
    /// directory and its files, so a cap of four counts two of three files.
    #[test]
    fn a_count_cut_short_says_so() {
        let tmp = tree_of_files(&["app/a.py", "app/b.py", "app/c.py"]);
        let header = |cap| {
            fingerprint_inner(tmp.path(), WalkConfig::default(), cap, far_off())
                .lines()
                .next()
                .map(str::to_string)
        };
        assert_eq!(
            header(4).as_deref(),
            Some("[tilth] Python project — 2+ source files, 1 directory")
        );
        assert_eq!(
            header(5).as_deref(),
            Some("[tilth] Python project — 3 source files, 1 directory")
        );
    }

    /// The `deps:` line of a manifest with `n` dependencies.
    fn deps_line(n: usize) -> String {
        let tmp = tempfile::tempdir().unwrap();
        let deps: Vec<String> = (0..n).map(|i| format!("\"dep{i:02}\": \"1\"")).collect();
        fs::write(
            tmp.path().join("package.json"),
            format!("{{\"dependencies\": {{{}}}}}", deps.join(", ")),
        )
        .unwrap();
        let summary = fingerprint(tmp.path(), WalkConfig::default(), far_off());
        summary
            .lines()
            .find(|line| line.starts_with("  deps: "))
            .unwrap_or_else(|| panic!("no deps line: {summary}"))
            .to_string()
    }

    /// The summary listed ten dependencies and said nothing of the rest
    /// (#343).
    #[test]
    fn the_startup_summary_counts_the_deps_it_leaves_out() {
        assert_eq!(
            deps_line(DEPS_SHOWN + 1),
            "  deps: dep00, dep01, dep02, dep03, dep04, dep05, dep06, dep07, dep08, dep09, \
             and 1 more"
        );
        assert_eq!(
            deps_line(DEPS_SHOWN),
            "  deps: dep00, dep01, dep02, dep03, dep04, dep05, dep06, dep07, dep08, dep09"
        );
    }

    /// The hot files of a Rust tree where two files import each of `n`
    /// modules.
    fn hot_of(n: usize) -> Option<String> {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let modules: Vec<String> = (0..n).map(|i| format!("h{i}")).collect();
        for module in &modules {
            fs::write(src.join(format!("{module}.rs")), "").unwrap();
        }
        for user in ["a.rs", "b.rs"] {
            fs::write(
                src.join(user),
                format!("use crate::{{{}}};\n", modules.join(", ")),
            )
            .unwrap();
        }
        let sample = sample_files(tmp.path(), WalkConfig::default(), far_off());
        hot_files(tmp.path(), sample.as_ref(), Some(Lang::Rust), far_off())
    }

    /// The summary listed five hot files and said nothing of the rest
    /// (#343).
    #[test]
    fn the_startup_summary_counts_the_hot_files_it_leaves_out() {
        assert_eq!(
            hot_of(HOT_SHOWN + 1).as_deref(),
            Some(
                "src/h0.rs ×2, src/h1.rs ×2, src/h2.rs ×2, src/h3.rs ×2, src/h4.rs ×2, and 1 more"
            )
        );
        assert_eq!(
            hot_of(HOT_SHOWN).as_deref(),
            Some("src/h0.rs ×2, src/h1.rs ×2, src/h2.rs ×2, src/h3.rs ×2, src/h4.rs ×2")
        );
    }

    /// Editors on Windows save `package.json` with a byte-order mark, and
    /// `serde_json` refuses one, so the manifest vanished from the fingerprint.
    #[test]
    fn a_byte_order_mark_keeps_the_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("package.json"),
            "\u{feff}{\"name\": \"bom-app\", \"version\": \"1.2.3\"}\n",
        )
        .unwrap();
        let manifest = parse_package_json(tmp.path()).expect("manifest parsed");
        assert_eq!(manifest.name.as_deref(), Some("bom-app"));
    }

    /// Git's output was read only once git exited, and a pipe holds 64 KiB,
    /// so a `git status` that printed more waited for the time limit, and the
    /// line then said `clean` (#356).
    #[test]
    fn a_status_longer_than_a_pipe_is_counted() {
        let repo = crate::git::tests::repo();
        for i in 0..2500 {
            fs::write(
                repo.path()
                    .join(format!("untracked-file-with-a-long-name-{i}.txt")),
                "",
            )
            .unwrap();
        }
        let line = git_context(repo.path(), far_off()).expect("a git line");
        assert!(line.ends_with(", 2500 uncommitted files"), "{line}");
    }

    /// A deadline only a hung git would miss.
    fn far_off() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// The branch is named, and on a detached HEAD the short commit id,
    /// each as git itself names it.
    #[test]
    fn the_git_line_names_the_branch_or_the_detached_commit() {
        let repo = crate::git::tests::repo();
        let dir = repo.path();
        git(
            dir,
            &[
                "-c",
                "user.name=tilth",
                "-c",
                "user.email=tilth@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--no-verify",
                "--allow-empty",
                "-m",
                "first",
            ],
        );
        let branch = git(dir, &["branch", "--show-current"]);
        assert_eq!(
            git_context(dir, far_off()),
            Some(format!("branch {branch}, clean"))
        );

        git(dir, &["checkout", "-q", "--detach"]);
        let commit = git(dir, &["rev-parse", "--short", "HEAD"]);
        assert_eq!(
            git_context(dir, far_off()),
            Some(format!("branch {commit}, clean"))
        );
    }

    /// A `git:` line git could not finish in time is left out, so it never
    /// calls a dirty checkout `clean` (#356).
    #[test]
    fn a_git_line_out_of_time_is_left_out() {
        let repo = crate::git::tests::repo();
        fs::write(repo.path().join("new.txt"), "").unwrap();
        let line = git_context(repo.path(), far_off()).expect("a git line");
        assert!(line.ends_with(", 1 uncommitted file"), "{line}");
        assert_eq!(git_context(repo.path(), Instant::now()), None);
    }

    /// The 250 ms budget was a log line: the overview measured its time,
    /// wrote to stderr when it ran over, and did all the work anyway
    /// (#385). Each part now checks the deadline, so one already past
    /// starts no part at all. This tree has a line of every kind, so each
    /// part is shown to have work to skip.
    #[test]
    fn a_deadline_already_past_gives_no_overview() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let walk = WalkConfig::default();
        let past = Instant::now();

        assert!(count_sources(root, walk, COUNT_CAP, far_off()).is_some());
        assert!(count_sources(root, walk, COUNT_CAP, past).is_none());

        let sample = sample_files(root, walk, far_off());
        assert!(sample.is_some());
        assert!(sample_files(root, walk, past).is_none());

        let rust = Some(Lang::Rust);
        assert!(hot_files(root, sample.as_ref(), rust, far_off()).is_some());
        assert_eq!(hot_files(root, sample.as_ref(), rust, past), None);
        assert!(test_style(root, sample.as_ref(), rust, far_off()).is_some());
        assert_eq!(test_style(root, sample.as_ref(), rust, past), None);
        // Without Rust the line reads no files, and still starts no later.
        assert!(test_style(root, sample.as_ref(), None, far_off()).is_some());
        assert_eq!(test_style(root, sample.as_ref(), None, past), None);

        assert_eq!(fingerprint(root, walk, past), "");
    }

    /// The overview of a tree of 100,000 small files, all within the
    /// sample's depth, ends within its limit and one step. Wall time is
    /// measured here, never asserted in CI.
    #[test]
    #[ignore = "measures wall time: cargo test --release -- --ignored a_tree_of_100k"]
    fn a_tree_of_100k_files_ends_within_the_limit() {
        let tmp = tempfile::tempdir().unwrap();
        for d in 0..100 {
            let dir = tmp.path().join(format!("d{d:02}"));
            fs::create_dir_all(&dir).unwrap();
            for f in 0..1000 {
                fs::write(dir.join(format!("f{f:04}.py")), "x = 1\n").unwrap();
            }
        }
        let start = Instant::now();
        let summary = fingerprint(tmp.path(), WalkConfig::default(), start + LIMIT);
        let elapsed = start.elapsed();
        println!("{}ms\n{summary}", elapsed.as_millis());
        assert!(elapsed < LIMIT + Duration::from_millis(50), "{elapsed:?}");
    }

    #[test]
    fn test_fingerprint_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let output = fingerprint(tmp.path(), WalkConfig::default(), far_off());

        // Empty dir: should produce minimal output or empty
        // With 0 files and 0 modules, the header will say "0 source files"
        // but that's fine — it's still useful context
        assert!(
            output.is_empty() || output.contains("0 source files"),
            "empty dir should produce empty or minimal output, got: {output}"
        );
    }

    #[test]
    fn test_manifest_parsing() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let info = parse_cargo_toml(root).expect("should parse Cargo.toml");

        assert_eq!(info.name.as_deref(), Some("tilth"));
        assert!(info.version.is_some(), "should have a version");
        assert!(
            info.deps.iter().any(|d| d == "clap"),
            "deps should include clap: {:?}",
            info.deps
        );
        assert!(
            info.deps.iter().any(|d| d == "dashmap"),
            "deps should include dashmap: {:?}",
            info.deps
        );
    }
}
