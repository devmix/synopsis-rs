//! Filesystem source parsers (oracle: `internal/ingestion/parsers/`).
//!
//! Each format module implements [`crate::Parser`] on top of one shared walk:
//! recursive, `.synignore` filtered, deterministically ordered, best-effort
//! (every failure is collected, never fatal). The shared pieces live here so
//! the per-format parsers (markdown in task 1.2, json in task 1.4, …) stay
//! thin and the walk semantics cannot drift between formats.
//!
//! Exclusions (human decision 2026-08-23): user `.synignore` files with
//! gitignore semantics are the single exclusion mechanism — there is no
//! built-in skip list. Absence of `.synignore` is a valid case: everything
//! is walked (sources are explicitly configured paths).
//!
//! Implemented formats: markdown (task 1.2), json (task 1.4); the remaining
//! formats (mediawiki, webpage, unstructured) follow the same pattern.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ignore::{IncrementalIgnore, WalkBuilder};

use crate::error::IngestionError;

pub mod json;
pub mod markdown;

/// File name of the user exclusion file, read from the source root and every
/// subdirectory with gitignore semantics.
pub(crate) const SYNIGNORE_FILE_NAME: &str = ".synignore";

/// Walks `source_path` and calls `visit` for every file whose path satisfies
/// `matches` and is not excluded by a `.synignore` file.
///
/// Best-effort contract (design D1; oracle `filepath.WalkDir` semantics): a
/// missing root, an unreadable directory and a `visit` failure are all
/// appended to `errors` and the walk continues. `source_path` may be a
/// directory (walked recursively) or a single file (visited when `matches`
/// holds); symlinks are not followed, mirroring the oracle's Lstat-based
/// `os.DirEntry`. Directory entries are visited in sorted file-name order —
/// Rust's `std::fs::read_dir` is OS-ordered, and sorting restores the
/// deterministic lexical order the oracle's `WalkDir` already had.
///
/// `.synignore` files are matched per entry during this walk (the matcher
/// loads and caches them lazily per directory, so there is no second
/// traversal). A single-file source is an explicit inclusion and is not
/// subject to `.synignore`. Failures to read or parse a `.synignore` file
/// are collected in `errors` (non-fatal); valid rules in the same file still
/// apply.
pub(crate) fn walk_matched_files(
    source_path: &Path,
    matches: impl Fn(&Path) -> bool,
    mut visit: impl FnMut(&Path) -> Result<(), IngestionError>,
    errors: &mut Vec<IngestionError>,
) {
    match fs::symlink_metadata(source_path) {
        Ok(meta) if meta.is_dir() => {
            let mut synignore = synignore_matcher(source_path);
            walk_dir(source_path, &matches, &mut visit, &mut synignore, errors);
        }
        // A non-directory root (file or symlink with a matching name) is
        // visited directly, like the oracle's single-entry `WalkDir`.
        Ok(_) if matches(source_path) => push_visit(&mut visit, source_path, errors),
        Ok(_) => {} // root file without a matching extension: nothing to do
        Err(source) => errors.push(IngestionError::Io {
            path: source_path.to_path_buf(),
            source,
        }),
    }
}

/// Builds the `.synignore` matcher for a directory source.
///
/// Exclusions come exclusively from `.synignore` files inside the source
/// tree, so every other ignore source the `ignore` crate supports is turned
/// off: hidden-file filtering, ignore files in parent directories above the
/// root, `.ignore`, `.gitignore`, `.git/info/exclude` and the global
/// gitignore. (In `ignore` 0.4 the tree-aware `IgnoreBuilder` is
/// `pub(crate)`; the public entry points are
/// [`WalkBuilder::add_custom_ignore_filename`] +
/// [`WalkBuilder::build_matchers`], verified against the 0.4.33 registry
/// source in task 1.2b.)
///
/// The matcher loads ignore files lazily per directory and caches them, so
/// it performs no walk of its own. `None` (unreachable in practice: one
/// configured root always yields exactly one matcher) degrades to "no
/// exclusions", i.e. the walk behaves as if no `.synignore` existed.
fn synignore_matcher(root: &Path) -> Option<IncrementalIgnore> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_global(false)
        .git_ignore(false)
        .git_exclude(false)
        .add_custom_ignore_filename(SYNIGNORE_FILE_NAME);
    builder.build_matchers().pop()
}

fn walk_dir(
    dir: &Path,
    matches: &impl Fn(&Path) -> bool,
    visit: &mut impl FnMut(&Path) -> Result<(), IngestionError>,
    synignore: &mut Option<IncrementalIgnore>,
    errors: &mut Vec<IngestionError>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) => {
            errors.push(IngestionError::Io {
                path: dir.to_path_buf(),
                source,
            });
            return;
        }
    };
    // Collect first: a per-entry failure is collected but does not abort the
    // directory, and the sorted order makes the walk deterministic.
    let mut entries: Vec<_> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(source) => {
                errors.push(IngestionError::Io {
                    path: dir.to_path_buf(),
                    source,
                });
                None
            }
        })
        .collect();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        // `file_type` does not follow symlinks, like the oracle's DirEntry.
        let is_dir = entry.file_type().is_ok_and(|file_type| file_type.is_dir());
        // `.synignore` exclusions apply to files and directories alike; an
        // excluded directory prunes its whole subtree (gitignore semantics:
        // an excluded directory cannot be re-included from below).
        if is_excluded(&path, is_dir, synignore, errors) {
            continue;
        }
        if is_dir {
            walk_dir(&path, matches, visit, synignore, errors);
        } else if matches(&path) {
            push_visit(visit, &path, errors);
        }
    }
}

/// True if `path` is excluded by the walk's `.synignore` matcher.
fn is_excluded(
    path: &Path,
    is_dir: bool,
    synignore: &mut Option<IncrementalIgnore>,
    errors: &mut Vec<IngestionError>,
) -> bool {
    let Some(matcher) = synignore.as_mut() else {
        return false;
    };
    // The walk never leaves the root, so stripping always succeeds.
    let Some(relative) = path.strip_prefix(matcher.root()).ok() else {
        return false;
    };
    let (matched, err) = matcher.matched_with_errors(relative, is_dir);
    if let Some(err) = err {
        collect_ignore_error(err, matcher.root(), errors);
    }
    matched.is_ignore()
}

/// Records a `.synignore` read/parse failure as a non-fatal walk error.
fn collect_ignore_error(err: ignore::Error, root: &Path, errors: &mut Vec<IngestionError>) {
    let path = match &err {
        ignore::Error::WithPath { path, .. } => path.clone(),
        _ => root.to_path_buf(),
    };
    // Glob parse errors are not `io::Error`s; carry them as `InvalidData` so
    // the message (with the offending glob) survives in `ParseResult::errors`.
    let message = err.to_string();
    let source = err
        .into_io_error()
        .unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, message));
    errors.push(IngestionError::Io { path, source });
}

fn push_visit(
    visit: &mut impl FnMut(&Path) -> Result<(), IngestionError>,
    path: &Path,
    errors: &mut Vec<IngestionError>,
) {
    if let Err(error) = visit(path) {
        errors.push(error);
    }
}

/// The document's `source_file` metadata value: `path` relative to the walk
/// root. Falls back to the bare file name when the path cannot be made
/// relative (single-file source or an unrelated root) — the oracle left this
/// empty on `filepath.Rel` failure, and a non-empty value is strictly more
/// useful for downstream deduplication.
pub(crate) fn source_file_name(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .filter(|rel| !rel.as_os_str().is_empty())
        .map(|rel| rel.to_string_lossy().into_owned())
        .or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Formats `time` as an RFC 3339 UTC string at second precision
/// (`2026-08-23T12:34:56Z`) — the shape of Go's `time.RFC3339` for UTC
/// instants. Returns `None` for instants before the Unix epoch (not
/// representable as a `SystemTime` duration).
///
/// Dependency-free on purpose: `chrono` is not in the frozen palette, and
/// second-precision UTC formatting is a few lines of calendar arithmetic.
pub(crate) fn format_rfc3339_utc(time: SystemTime) -> Option<String> {
    let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let remainder = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (remainder / 3_600, (remainder % 3_600) / 60, remainder % 60);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

/// Days since the civil epoch 1970-01-01 to a (year, month, day) triple
/// (Howard Hinnant's `civil_from_days`; `div_euclid` keeps pre-1970 dates
/// correct).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month + 2) / 5 + 1;
    let month = if month < 10 { month + 3 } else { month - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
// `pub(crate)`: the shared `TempTree` fixture is reused by the sibling
// modules' tests (parsers, sources).
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    /// A temporary directory that removes itself on drop.
    pub(crate) struct TempTree(pub(crate) PathBuf);

    impl TempTree {
        pub(crate) fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "synopsis-walk-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp tree");
            Self(path)
        }

        /// Writes `content` to `rel` under the tree, creating parent dirs.
        pub(crate) fn write(&self, rel: &str, content: &str) -> PathBuf {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent dirs");
            }
            fs::write(&path, content).expect("write file");
            path
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Walks `root` and returns the file names (relative to `root`) of every
    /// visited file, in visit order. The predicate accepts everything except
    /// the `.synignore` files themselves (they are plain files, and real
    /// parser predicates reject them by extension).
    fn walked_files(root: &Path) -> (Vec<String>, Vec<IngestionError>) {
        let mut files = Vec::new();
        let mut errors = Vec::new();
        walk_matched_files(
            root,
            |path| {
                path.file_name()
                    .is_some_and(|name| name != std::ffi::OsStr::new(SYNIGNORE_FILE_NAME))
            },
            |path| {
                files.push(source_file_name(path, root));
                Ok(())
            },
            &mut errors,
        );
        (files, errors)
    }

    #[test]
    fn rfc3339_formats_known_instants() {
        // 1_767_225_600 s since the Unix epoch = 2026-01-01T00:00:00Z.
        let instant = UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        assert_eq!(format_rfc3339_utc(instant).unwrap(), "2026-01-01T00:00:00Z");
        assert_eq!(
            format_rfc3339_utc(UNIX_EPOCH).unwrap(),
            "1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn rfc3339_rejects_pre_epoch_instants() {
        let pre_epoch = UNIX_EPOCH - Duration::from_secs(1);
        assert!(format_rfc3339_utc(pre_epoch).is_none());
    }

    #[test]
    fn source_file_name_is_relative_with_fallbacks() {
        assert_eq!(
            source_file_name(Path::new("/root/a/b.md"), Path::new("/root")),
            "a/b.md"
        );
        // Single-file source: the path IS the root -> bare file name.
        assert_eq!(
            source_file_name(Path::new("/root/a.md"), Path::new("/root/a.md")),
            "a.md"
        );
        // Unrelated root: bare file name, never empty.
        assert_eq!(
            source_file_name(Path::new("/x/a.md"), Path::new("/y")),
            "a.md"
        );
    }

    #[test]
    fn synignore_at_root_excludes_dir_file_and_pattern() {
        let tree = TempTree::new();
        tree.write(".synignore", "build/\nsecret.md\n*.log\n");
        tree.write("a.md", "kept");
        tree.write("build/out.md", "excluded dir");
        tree.write("secret.md", "excluded file");
        tree.write("notes.log", "excluded pattern");
        tree.write("sub/other.log", "excluded pattern at depth");

        let (files, errors) = walked_files(&tree.0);

        assert!(errors.is_empty(), "errors: {:?}", errors);
        assert_eq!(files, vec!["a.md"]);
    }

    #[test]
    fn nested_synignore_scopes_to_its_subtree() {
        let tree = TempTree::new();
        tree.write("sub/.synignore", "private.md\n");
        tree.write("private.md", "root level: kept");
        tree.write("sub/private.md", "excluded by sub/.synignore");
        tree.write("sub/kept.md", "kept");
        tree.write("top.md", "kept");

        let (files, errors) = walked_files(&tree.0);

        assert!(errors.is_empty(), "errors: {:?}", errors);
        // The sub-directory rule does not leak to the root level.
        assert_eq!(files, vec!["private.md", "sub/kept.md", "top.md"]);
    }

    #[test]
    fn nested_synignore_can_reinclude() {
        let tree = TempTree::new();
        tree.write(".synignore", "*.draft.md\n");
        tree.write("sub/.synignore", "!keep.draft.md\n");
        tree.write("top.draft.md", "excluded by root rule");
        tree.write("sub/drop.draft.md", "still excluded");
        tree.write("sub/keep.draft.md", "re-included by deeper rule");

        let (files, errors) = walked_files(&tree.0);

        assert!(errors.is_empty(), "errors: {:?}", errors);
        // Gitignore precedence: the deepest matching rule wins.
        assert_eq!(files, vec!["sub/keep.draft.md"]);
    }

    #[test]
    fn no_synignore_walks_everything() {
        let tree = TempTree::new();
        tree.write(".git/HEAD.md", "walked now that SKIP_DIRS is gone");
        tree.write(
            "node_modules/pkg/readme.md",
            "walked now that SKIP_DIRS is gone",
        );
        tree.write("visible.md", "kept");

        let (files, errors) = walked_files(&tree.0);

        assert!(errors.is_empty(), "errors: {:?}", errors);
        assert_eq!(
            files,
            vec![".git/HEAD.md", "node_modules/pkg/readme.md", "visible.md"]
        );
    }

    #[test]
    fn deterministic_order_preserved_with_synignore() {
        let tree = TempTree::new();
        // Deliberately non-sorted creation order.
        tree.write("z.md", "z");
        tree.write(".synignore", "excluded.md\n");
        tree.write("excluded.md", "excluded");
        tree.write("m/d.md", "d");
        tree.write("m/b.md", "b");
        tree.write("a.md", "a");
        tree.write("m/c.md", "c");

        let (files, errors) = walked_files(&tree.0);

        assert!(errors.is_empty(), "errors: {:?}", errors);
        assert_eq!(files, vec!["a.md", "m/b.md", "m/c.md", "m/d.md", "z.md"]);
    }

    #[test]
    fn broken_synignore_rule_is_collected_and_valid_rules_still_apply() {
        let tree = TempTree::new();
        // `{bad` is an invalid glob; `*.tmp` in the same file must still apply.
        tree.write(".synignore", "{bad\n*.tmp\n");
        tree.write("drop.tmp", "excluded by the valid rule");
        tree.write("keep.md", "kept");

        let (files, errors) = walked_files(&tree.0);

        assert_eq!(files, vec!["keep.md"]);
        assert_eq!(errors.len(), 1, "one non-fatal .synignore error");
        assert!(
            matches!(
                errors[0],
                IngestionError::Io { ref path, .. } if path == &tree.0.join(".synignore")
            ),
            "error must carry the offending .synignore path, got {:?}",
            errors[0]
        );
    }
}
