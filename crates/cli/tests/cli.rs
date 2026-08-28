//! Binary-level tests for the CLI surface (task 1.1): version output, usage
//! error exit codes, and stub subcommand dispatch.

// Test code: unwrap/expect are intentional (asserting on well-defined outcomes).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Command, Output};

fn synopsis() -> Command {
    Command::new(env!("CARGO_BIN_EXE_synopsis"))
}

fn run(args: &[&str]) -> Output {
    synopsis().args(args).output().unwrap()
}

#[test]
fn version_exits_zero_and_prints_name_and_version() {
    let out = run(&["--version"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with(&format!("synopsis {}", env!("CARGO_PKG_VERSION"))),
        "unexpected --version output: {stdout:?}"
    );
}

#[test]
fn unknown_subcommand_exits_one_with_error() {
    let out = run(&["bogus-command"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error"), "stderr: {stderr}");
}

#[test]
fn missing_subcommand_prints_usage_and_exits_one() {
    let out = run(&[]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Usage"), "stderr: {stderr}");
}

/// Writes a minimal parseable config and returns its path.
fn write_config(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("config.default.yaml");
    std::fs::write(&path, "logging:\n  level: info\n").unwrap();
    path
}

#[test]
fn subcommand_stub_prints_error_and_exits_one() {
    let dir =
        std::env::temp_dir().join(format!("synopsis-cli-bin-test-{}-stub", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = write_config(&dir);
    // `serve` is implemented (task 1.6) and no longer stubs; the remaining
    // subcommands still dispatch to the not-implemented stub.
    for sub in ["sync", "model list", "onnx-runtime status", "load-test"] {
        let args: Vec<&str> = ["--config", cfg.to_str().unwrap()]
            .into_iter()
            .chain(sub.split_whitespace())
            .collect();
        let out = run(&args);
        assert_eq!(
            out.status.code(),
            Some(1),
            "subcommand {sub:?}: stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("not yet implemented"),
            "subcommand {sub:?}: stderr: {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_config_exits_one_with_error() {
    let out = run(&["--config", "/nonexistent/path/config.default.yaml", "sync"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("failed to load config"), "stderr: {stderr}");
}

#[test]
fn global_flags_precede_subcommand() {
    // Frozen contract scenario: global flags before the subcommand, per-command
    // flags after it.
    let dir = std::env::temp_dir().join(format!(
        "synopsis-cli-bin-test-{}-flags",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = write_config(&dir);
    // `serve` is implemented (task 1.6) and would start a real server, so the
    // parsing contract is probed with `sync` (still a stub): global flags
    // before the subcommand, per-command flags after it.
    let out = run(&[
        "--preset",
        "default",
        "--config",
        cfg.to_str().unwrap(),
        "--db",
        "/tmp/knowledge.db",
        "sync",
        "--rebuild",
    ]);
    assert_eq!(out.status.code(), Some(1), "stub must still exit 1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not yet implemented"), "stderr: {stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}
