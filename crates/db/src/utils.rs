//! Small text helpers shared by the DAOs (design D6: local `db::utils`
//! module — a shared `utils` crate would be premature abstraction; extract
//! only if a second consumer appears).

/// Prepare arbitrary text for matching: trim surrounding whitespace,
/// collapse internal whitespace runs to single spaces, lowercase
/// (Unicode-aware).
pub fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Escape `\`, `%` and `_` in user input for use in `LIKE ... ESCAPE '\'`
/// patterns (the backslash is escaped first).
pub fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn normalize_edges() {
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("   "), "");
        assert_eq!(normalize("  Hello   WORLD  "), "hello world");
        // Tabs, newlines and multiple spaces collapse to one space.
        assert_eq!(normalize("a\tb\nc  d"), "a b c d");
        // Unicode casing.
        assert_eq!(normalize("  ÜBER   "), "über");
    }

    #[test]
    fn escape_like_edges() {
        assert_eq!(escape_like(""), "");
        assert_eq!(escape_like("plain"), "plain");
        assert_eq!(escape_like("100%_off"), "100\\%\\_off");
        // Backslash is escaped first: the 2-char input `\%` becomes `\\\%`.
        assert_eq!(escape_like("\\%"), "\\\\\\%");
        assert_eq!(escape_like("a_b%c\\d"), "a\\_b\\%c\\\\d");
    }
}
