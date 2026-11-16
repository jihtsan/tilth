//! How a path becomes text in tilth's output.

use std::fmt::{self, Display, Write};
use std::path::Path;

/// `path` as tilth prints it. Every place a path reaches output (a result
/// heading, a list, a tree, an error message) prints it through this.
///
/// A path prints as [`Path::display`] prints it, unless it holds a
/// character that would break the line it sits on: a control character
/// (C0, DEL or C1) or the line or paragraph separator (U+2028, U+2029). A
/// file name could otherwise end the line and start one that looks like a
/// result. Such a path prints in double quotes, with `\n`, `\r` and `\t`
/// for those three characters, `\u{…}` in lowercase hex for every other
/// one of them, and `\"` and `\\` for a quote and a backslash inside.
/// A path that starts with a double quote is quoted the same way, so a
/// quoted form is never also the plain form of another path.
///
/// A file named `evil`, newline, `x.rs` prints as `"evil\nx.rs"`.
pub fn path<P: AsRef<Path> + ?Sized>(path: &P) -> ShownPath<'_> {
    ShownPath(path.as_ref())
}

/// A path ready to print; see [`path`].
#[derive(Clone, Copy, Debug)]
pub struct ShownPath<'a>(&'a Path);

impl Display for ShownPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self.0.to_string_lossy();
        if !text.starts_with('"') && !text.chars().any(breaks_line) {
            #[expect(
                clippy::disallowed_methods,
                reason = "by design: the one place a path becomes text"
            )]
            let plain = self.0.display();
            return plain.fmt(f);
        }
        let mut quoted = String::with_capacity(text.len() + 2);
        quoted.push('"');
        for c in text.chars() {
            match c {
                '"' => quoted.push_str("\\\""),
                '\\' => quoted.push_str("\\\\"),
                '\n' => quoted.push_str("\\n"),
                '\r' => quoted.push_str("\\r"),
                '\t' => quoted.push_str("\\t"),
                c if breaks_line(c) => write!(quoted, "\\u{{{:x}}}", u32::from(c))?,
                c => quoted.push(c),
            }
        }
        quoted.push('"');
        f.pad(&quoted)
    }
}

/// A control character (C0, DEL, C1) or the Unicode line or paragraph
/// separator: a character a reader may take for the end of a line.
fn breaks_line(c: char) -> bool {
    c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_path_prints_as_display_does() {
        for p in ["src/main.rs", "a b/ç.rs", r"dir\file.rs", "x\"y", ""] {
            #[expect(clippy::disallowed_methods, reason = "by design: the reference")]
            let want = Path::new(p).display().to_string();
            assert_eq!(path(p).to_string(), want);
        }
        assert_eq!(format!("{:<8}|", path("a.rs")), "a.rs    |");
    }

    #[test]
    fn a_path_that_breaks_a_line_prints_quoted_and_escaped() {
        assert_eq!(
            path("evil\n## injected.rs:1 [definition]\nname.rs").to_string(),
            r#""evil\n## injected.rs:1 [definition]\nname.rs""#
        );
        assert_eq!(path("a\tb\rc").to_string(), r#""a\tb\rc""#);
        assert_eq!(
            path("a\u{0}\u{1b}\u{7f}").to_string(),
            r#""a\u{0}\u{1b}\u{7f}""#
        );
        assert_eq!(path("a\u{85}b").to_string(), r#""a\u{85}b""#);
        assert_eq!(
            path("a\u{2028}b\u{2029}").to_string(),
            r#""a\u{2028}b\u{2029}""#
        );
        assert_eq!(path("q\"\\\n").to_string(), r#""q\"\\\n""#);
    }

    #[test]
    fn a_path_that_starts_with_a_quote_prints_quoted() {
        // The plain form of `"a\nb"` would read as the quoted form of a
        // path with a newline in it.
        assert_eq!(path(r#""a\nb""#).to_string(), r#""\"a\\nb\"""#);
    }

    #[test]
    fn no_printed_path_spans_lines() {
        for c in (0..=0x10_ffff).filter_map(char::from_u32) {
            let shown = path(&format!("a{c}b")).to_string();
            assert!(
                !shown.contains(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}']),
                "{c:?} printed as {shown:?}"
            );
        }
    }
}
