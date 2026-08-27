//! Configuration file path resolution (design D1).
//!
//! Port of the oracle's `resolveConfigPath` / `resolveConfigCandidates`
//! (`../synopsis/cmd/app/main.go`): an explicit `--config` path wins outright;
//! otherwise `config.{preset}.yaml` is searched relative to the executable
//! first, then relative to the CWD, with a final fallback that lets
//! `config::load` surface the error.
//!
//! Deliberate deviation from the oracle (design D1): the oracle resolves
//! symlinks of the executable path; Rust binaries are not symlinked in this
//! deployment, so `std::env::current_exe()` is used as-is.

use std::path::{Path, PathBuf};

/// Returns the effective configuration file path.
///
/// An explicit `cli_path` (`--config`) wins outright; otherwise the
/// preset-based candidate search runs using the real executable and CWD
/// (see [`resolve_config_candidates`]).
pub fn resolve_config_path(cli_path: Option<&Path>, preset: &str) -> PathBuf {
    if let Some(path) = cli_path {
        return path.to_path_buf();
    }
    let exe = std::env::current_exe().ok();
    let cwd = std::env::current_dir().ok();
    resolve_config_candidates(exe.as_deref(), cwd.as_deref(), preset)
}

/// Searches for `config.{preset}.yaml` in candidate order:
///
/// 1. `<exeDir>/configs/`
/// 2. `<exeDir>/`
/// 3. `<parent(exeDir)>/`
/// 4. `<parent(exeDir)>/configs/`
/// 5. `<cwd>/configs/`
/// 6. `<cwd>/`
///
/// When nothing exists (or `exe` / `cwd` are unavailable), falls back to the
/// relative `configs/config.{preset}.yaml` so that `config::load` surfaces the
/// error with the path the user would have passed via `--config`.
pub fn resolve_config_candidates(exe: Option<&Path>, cwd: Option<&Path>, preset: &str) -> PathBuf {
    let config_name = format!("config.{preset}.yaml");

    if let Some(exe_dir) = exe.and_then(Path::parent) {
        for candidate in [
            Some(exe_dir.join("configs").join(&config_name)),
            Some(exe_dir.join(&config_name)),
            exe_dir.parent().map(|parent| parent.join(&config_name)),
            exe_dir
                .parent()
                .map(|parent| parent.join("configs").join(&config_name)),
        ]
        .into_iter()
        .flatten()
        {
            if candidate.exists() {
                return candidate;
            }
        }
    }

    if let Some(cwd) = cwd {
        for candidate in [
            cwd.join("configs").join(&config_name),
            cwd.join(&config_name),
        ] {
            if candidate.exists() {
                return candidate;
            }
        }
    }

    PathBuf::from("configs").join(config_name)
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures are set up by hand).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::SystemTime;

    use super::*;

    /// Creates a unique per-process temp directory and removes it on drop.
    struct TempDir(PathBuf);

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "synopsis-cli-test-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                COUNTER.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn file(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, "{}").unwrap();
    }

    #[test]
    fn explicit_cli_path_wins() {
        let dir = TempDir::new();
        let explicit = dir.0.join("explicit.yaml");
        assert_eq!(resolve_config_path(Some(&explicit), "default"), explicit);
        // Even when a candidate would have matched.
        let fallback = dir.0.join("config.default.yaml");
        file(&fallback);
        assert_eq!(resolve_config_path(Some(&explicit), "default"), explicit);
    }

    #[test]
    fn exe_dir_configs_candidate_wins() {
        let dir = TempDir::new();
        let exe = dir.0.join("bin").join("synopsis");
        let match_path = dir
            .0
            .join("bin")
            .join("configs")
            .join("config.default.yaml");
        file(&match_path);
        assert_eq!(
            resolve_config_candidates(Some(&exe), None, "default"),
            match_path
        );
    }

    #[test]
    fn exe_dir_candidate_beats_parent_candidates() {
        let dir = TempDir::new();
        let exe = dir.0.join("bin").join("synopsis");
        let match_path = dir.0.join("bin").join("config.default.yaml");
        file(&match_path);
        file(&dir.0.join("config.default.yaml"));
        assert_eq!(
            resolve_config_candidates(Some(&exe), None, "default"),
            match_path
        );
    }

    #[test]
    fn parent_candidate_is_found() {
        let dir = TempDir::new();
        let exe = dir.0.join("bin").join("synopsis");
        let match_path = dir.0.join("config.default.yaml");
        file(&match_path);
        assert_eq!(
            resolve_config_candidates(Some(&exe), None, "default"),
            match_path
        );
    }

    #[test]
    fn parent_configs_candidate_is_found() {
        let dir = TempDir::new();
        let exe = dir.0.join("bin").join("synopsis");
        let match_path = dir.0.join("configs").join("config.default.yaml");
        file(&match_path);
        assert_eq!(
            resolve_config_candidates(Some(&exe), None, "default"),
            match_path
        );
    }

    #[test]
    fn cwd_configs_candidate_is_found() {
        let dir = TempDir::new();
        let match_path = dir.0.join("configs").join("config.default.yaml");
        file(&match_path);
        assert_eq!(
            resolve_config_candidates(None, Some(&dir.0), "default"),
            match_path
        );
    }

    #[test]
    fn cwd_candidate_is_found() {
        let dir = TempDir::new();
        let match_path = dir.0.join("config.default.yaml");
        file(&match_path);
        assert_eq!(
            resolve_config_candidates(None, Some(&dir.0), "default"),
            match_path
        );
    }

    #[test]
    fn exe_candidates_win_over_cwd() {
        let dir = TempDir::new();
        let exe = dir.0.join("bin").join("synopsis");
        let exe_match = dir
            .0
            .join("bin")
            .join("configs")
            .join("config.default.yaml");
        let cwd_match = dir.0.join("config.default.yaml");
        file(&exe_match);
        file(&cwd_match);
        assert_eq!(
            resolve_config_candidates(Some(&exe), Some(&dir.0), "default"),
            exe_match
        );
    }

    #[test]
    fn preset_name_is_respected() {
        let dir = TempDir::new();
        let match_path = dir.0.join("config.prod.yaml");
        file(&match_path);
        assert_eq!(
            resolve_config_candidates(None, Some(&dir.0), "prod"),
            match_path
        );
    }

    #[test]
    fn fallback_when_nothing_exists() {
        let dir = TempDir::new();
        assert_eq!(
            resolve_config_candidates(Some(&dir.0.join("synopsis")), Some(&dir.0), "default"),
            PathBuf::from("configs").join("config.default.yaml")
        );
    }

    #[test]
    fn fallback_carries_preset() {
        assert_eq!(
            resolve_config_candidates(None, None, "nightly"),
            PathBuf::from("configs").join("config.nightly.yaml")
        );
    }

    #[test]
    fn exe_without_parent_is_skipped_safely() {
        // A bare file name has no meaningful parent; the search must not
        // crash and must fall through to the CWD candidates.
        let dir = TempDir::new();
        let match_path = dir.0.join("config.default.yaml");
        file(&match_path);
        assert_eq!(
            resolve_config_candidates(Some(Path::new("synopsis")), Some(&dir.0), "default"),
            match_path
        );
    }
}
