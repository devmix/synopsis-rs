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
    // `load-test` (task 1.10) is implemented; without an installed embedding
    // model it exits 1 with a model-not-found error.
    let out = run(&["--config", cfg.to_str().unwrap(), "load-test"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error"), "stderr: {stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `onnx-runtime` with an undeclared sub-action is a clap usage error:
/// exit 1 (oracle parity: `Unknown command` + usage + `os.Exit(1)`).
#[test]
fn onnx_runtime_unknown_subaction_exits_one() {
    let out = run(&["onnx-runtime", "bogus"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("error"), "stderr: {stderr}");
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
    // flags after it. `onnx-runtime status` (task 1.9) is a read-only
    // subcommand, so the parsing contract is probed end-to-end: if the global
    // flags were not parsed before the subcommand, clap would reject the
    // invocation with exit 1.
    let dir = std::env::temp_dir().join(format!(
        "synopsis-cli-bin-test-{}-flags",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // `status` needs an onnx.yaml with a platform entry for the current host.
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        std::env::consts::OS
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "amd64"
    } else {
        "arm64"
    };
    let key = format!("{os}-{arch}");
    let onnx = dir.join("onnx.yaml");
    std::fs::write(
        &onnx,
        format!(
            "runtime:\n  version: \"1.28.0\"\n  platforms:\n    - key: {key}\n      os: {os}\n      arch: {arch}\n      archive_url: http://127.0.0.1:1/onnxruntime-{key}.zip\n      archive_format: zip\n      library_name: libonnxruntime.so.1.28.0\n      library_path: onnxruntime-pkg/lib/libonnxruntime.so.1.28.0\n"
        ),
    )
    .unwrap();
    let cfg = dir.join("config.yaml");
    std::fs::write(
        &cfg,
        format!(
            "paths:\n  workspace_dir: {data}\n  onnx_config: {onnx}\n",
            data = dir.join("data").display(),
            onnx = onnx.display()
        ),
    )
    .unwrap();

    let out = run(&[
        "--preset",
        "default",
        "--config",
        cfg.to_str().unwrap(),
        "--db",
        "/tmp/knowledge.db",
        "onnx-runtime",
        "status",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ONNX Runtime Status"), "{stdout:?}");
    assert!(stdout.contains("Status:      Not installed"), "{stdout:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `model list` (task 1.8) prints the registry table and exits 0: the binary
/// end-to-end path (parse → resolve config → load → model_flow) with a
/// temp-dir config and an uninstalled registry entry.
#[test]
fn model_list_prints_registry_table_and_exits_zero() {
    let dir = std::env::temp_dir().join(format!(
        "synopsis-cli-bin-test-{}-model",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let onnx = dir.join("onnx.yaml");
    std::fs::write(
        &onnx,
        "models:\n  default: bge-m3-int8\n  entries:\n    - name: bge-m3-int8\n      display_name: BGE-M3 int8\n      version: 1.0.0\n      vector_dim: 1024\n",
    )
    .unwrap();
    let cfg = dir.join("config.yaml");
    std::fs::write(
        &cfg,
        format!(
            "paths:\n  workspace_dir: {data}\n  onnx_config: {onnx}\n",
            data = dir.join("data").display(),
            onnx = onnx.display()
        ),
    )
    .unwrap();

    let out = run(&["--config", cfg.to_str().unwrap(), "model", "list"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Available Models:"), "{stdout:?}");
    assert!(stdout.contains("BGE-M3 int8"), "{stdout:?}");
    assert!(stdout.contains("not installed"), "{stdout:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
