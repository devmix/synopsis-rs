//! Filesystem source parsers (oracle: `internal/ingestion/parsers/`).
//!
//! Each format module implements [`crate::Parser`] on top of one shared walk:
//! recursive, skip-list filtered, deterministically ordered, best-effort
//! (every failure is collected, never fatal). The shared pieces live here so
//! the per-format parsers (markdown in task 1.2, json in task 1.4, …) stay
//! thin and the walk semantics cannot drift between formats — the oracle's
//! Go parsers share the same `skipDirs` and `WalkDir` structure.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::IngestionError;

pub mod markdown;

/// Directory names excluded from recursive walks at any depth (oracle:
/// `parsers.skipDirs`).
pub(crate) const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".idea",
    ".vscode",
    "__pycache__",
    ".opencode",
];

/// Walks `source_path` and calls `visit` for every file whose path satisfies
/// `matches`.
///
/// Best-effort contract (design D1; oracle `filepath.WalkDir` semantics): a
/// missing root, an unreadable directory and a `visit` failure are all
/// appended to `errors` and the walk continues. `source_path` may be a
/// directory (walked recursively) or a single file (visited when `matches`
/// holds); symlinks are not followed, mirroring the oracle's Lstat-based
/// `os.DirEntry`. Directory entries are visited in sorted file-name order —
/// Rust's `std::fs::read_dir` is OS-ordered, and sorting restores the
/// deterministic lexical order the oracle's `WalkDir` already had.
pub(crate) fn walk_matched_files(
    source_path: &Path,
    matches: impl Fn(&Path) -> bool,
    mut visit: impl FnMut(&Path) -> Result<(), IngestionError>,
    errors: &mut Vec<IngestionError>,
) {
    match fs::symlink_metadata(source_path) {
        Ok(meta) if meta.is_dir() => walk_dir(source_path, &matches, &mut visit, errors),
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

fn walk_dir(
    dir: &Path,
    matches: &impl Fn(&Path) -> bool,
    visit: &mut impl FnMut(&Path) -> Result<(), IngestionError>,
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
        if is_dir {
            // Skip-listed directories are pruned entirely (oracle: SkipDir).
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| SKIP_DIRS.contains(&name))
            {
                continue;
            }
            walk_dir(&path, matches, visit, errors);
        } else if matches(&path) {
            push_visit(visit, &path, errors);
        }
    }
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
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::time::Duration;

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
}
