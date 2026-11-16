//! Paths as tests compare them.

use std::path::Path;

/// `path` below `root`, its components joined with `/` on every platform,
/// so one expected string holds on Windows too.
pub(crate) fn relative(path: &Path, root: &Path) -> String {
    let rel = path
        .strip_prefix(root)
        .unwrap_or_else(|_| panic!("{path:?} is not under {root:?}"));
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
