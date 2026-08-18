//! Fixture loader API (design D6).
//!
//! Parity fixtures are exported once from the Go oracle (`../synopsis`): a copy
//! of `knowledge.db` (SQLite, schema from the five oracle migrations) plus
//! `vectors.bin`, a binary vector dump. The binary format is fixed by the
//! `native-seam-spikes` change; until then this module only validates that a
//! fixture set exists on disk and exposes its paths — parsing arrives with the
//! module changes that need it (task 3.1 keeps the loader a documented stub).

use std::path::{Path, PathBuf};

use crate::HarnessError;

/// File names expected in a fixture directory.
const KNOWNLEDGE_DB: &str = "knowledge.db";
const VECTORS_BIN: &str = "vectors.bin";

/// One exported fixture set: paths to the knowledge database and vector dump.
#[derive(Debug, Clone)]
pub struct FixtureSet {
    /// Path to the `knowledge.db` SQLite file (oracle schema, read-only).
    pub db_path: PathBuf,
    /// Path to the `vectors.bin` binary vector dump (format: native-seam-spikes).
    pub vectors_path: PathBuf,
}

impl FixtureSet {
    /// Load a fixture set from `dir`, which must contain both `knowledge.db`
    /// and `vectors.bin`.
    ///
    /// TODO(native-seam-spikes): parse the binary vector dump once its format is
    /// fixed there; this skeleton only validates file presence (task 3.1).
    pub fn load(dir: impl Into<PathBuf>) -> Result<Self, HarnessError> {
        let dir = dir.into();
        for name in [KNOWNLEDGE_DB, VECTORS_BIN] {
            let path = dir.join(name);
            if !path.is_file() {
                return Err(HarnessError::Fixture {
                    reason: format!("file not found (expected inside {})", dir.display()),
                    path,
                });
            }
        }
        Ok(Self {
            db_path: dir.join(KNOWNLEDGE_DB),
            vectors_path: dir.join(VECTORS_BIN),
        })
    }

    /// Directory containing this fixture set (empty relative path if `db_path`
    /// has no parent component; never panics).
    pub fn dir(&self) -> &Path {
        self.db_path.parent().unwrap_or(Path::new(""))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Scratch fixture dir under the system temp area (never real user data).
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("parity-harness-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn load_missing_files_is_typed_fixture_error() {
        let dir = scratch_dir("missing");
        let err = FixtureSet::load(&dir).unwrap_err();
        match &err {
            HarnessError::Fixture { path, reason } => {
                assert!(reason.contains("not found"), "got: {reason}");
                assert!(path.file_name().is_some());
            }
            other => panic!("expected Fixture error, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_present_files_exposes_paths() {
        let dir = scratch_dir("present");
        for name in [KNOWNLEDGE_DB, VECTORS_BIN] {
            std::fs::write(dir.join(name), b"stub").expect("write stub fixture");
        }

        let set = FixtureSet::load(&dir).unwrap();
        assert_eq!(set.db_path, dir.join(KNOWNLEDGE_DB));
        assert_eq!(set.vectors_path, dir.join(VECTORS_BIN));
        assert_eq!(set.dir(), dir.as_path());

        // Removing one file makes the set unloadable again.
        std::fs::remove_file(set.vectors_path.as_path()).expect("remove");
        assert!(FixtureSet::load(&dir).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
