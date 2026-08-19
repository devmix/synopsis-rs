//! Integration tests for [`crate::onnx`] against the verbatim oracle fixture.
//!
//! The fixture `tests/data/onnx.yaml` is copied byte-for-byte from
//! `../synopsis/configs/onnx.yaml` (see `tests/data/README.md`). These tests assert that
//! the whole registry parses with its exact values and that both lookups behave like the
//! oracle's `PlatformForKey` / `ModelForName`; criteria (a)-(e) of task 2.1.

// Test target: expect on fixture loading is intentional (the files always exist).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use config::onnx::ArchiveFormat;
use config::{ConfigError, OnnxConfig, load_onnx_config};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/onnx.yaml")
}

#[test]
fn fixture_parses_registry_with_expected_values() {
    // Criterion (a): 5 platforms, 3 models, default name, exact file sizes.
    let cfg: OnnxConfig = load_onnx_config(fixture()).expect("onnx.yaml must parse");

    assert_eq!(cfg.runtime.version, "1.28.0");

    // Platforms -------------------------------------------------------------------
    assert_eq!(cfg.runtime.platforms.len(), 5);
    let keys: Vec<&str> = cfg
        .runtime
        .platforms
        .iter()
        .map(|p| p.key.as_str())
        .collect();
    assert_eq!(
        keys,
        vec![
            "windows-amd64",
            "linux-amd64",
            "linux-arm64",
            "darwin-amd64",
            "darwin-arm64"
        ]
    );

    let win = cfg
        .platform_for_key("windows-amd64")
        .expect("windows platform");
    assert_eq!(win.os, "windows");
    assert_eq!(win.arch, "amd64");
    assert_eq!(win.archive_format, ArchiveFormat::Zip);
    assert_eq!(win.library_name, "onnxruntime.dll");
    assert_eq!(
        win.archive_url,
        "https://github.com/microsoft/onnxruntime/releases/download/v1.28.0/onnxruntime-win-x64-1.28.0.zip"
    );

    let linux = cfg.platform_for_key("linux-amd64").expect("linux platform");
    assert_eq!(linux.archive_format, ArchiveFormat::Tgz);
    assert_eq!(linux.library_name, "libonnxruntime.so.1.28.0");
    assert_eq!(
        linux.library_path,
        "onnxruntime-linux-x64-1.28.0/lib/libonnxruntime.so.1.28.0"
    );

    for key in ["linux-arm64", "darwin-amd64", "darwin-arm64"] {
        let p = cfg.platform_for_key(key).expect("platform present");
        assert_eq!(p.archive_format, ArchiveFormat::Tgz);
        assert!(!p.library_name.is_empty() && !p.library_path.is_empty());
    }

    // Models ----------------------------------------------------------------------
    assert_eq!(cfg.models.default, "bge-small-en-v1.5");
    assert_eq!(cfg.models.entries.len(), 3);

    let bge_m3 = cfg.model_for_name("bge-m3-int8").expect("bge-m3 model");
    assert_eq!(bge_m3.display_name, "BGE-M3 INT8 Quantized");
    assert_eq!(bge_m3.version, "1.0.0");
    assert_eq!(bge_m3.vector_dim, 1024); // criterion (c)
    assert_eq!(bge_m3.source, "huggingface");
    assert_eq!(bge_m3.repo, "BAAI/bge-m3");
    let sizes: Vec<i64> = bge_m3.files.iter().map(|f| f.size_bytes).collect();
    assert_eq!(sizes, vec![724923, 2_266_820_608, 17_082_821]); // criterion (a)
    for file in &bge_m3.files {
        assert!(file.url.starts_with("https://huggingface.co/BAAI/bge-m3/"));
        assert!(file.checksum.is_none(), "fixture has no checksums");
    }

    let small = cfg
        .model_for_name("bge-small-en-v1.5")
        .expect("small model");
    assert_eq!(small.vector_dim, 384);
    assert_eq!(
        small.files.iter().map(|f| f.size_bytes).collect::<Vec<_>>(),
        vec![133_093_490, 711_396, 366]
    );

    let mini = cfg
        .model_for_name("paraphrase-multilingual-MiniLM-L12-v2")
        .expect("minilm model");
    assert_eq!(mini.vector_dim, 384);
    assert_eq!(mini.files.len(), 2);
}

#[test]
fn platform_for_key_matches_oracle_semantics() {
    // Criterion (b): known key -> Some with the file's archive_url; unknown -> None.
    let cfg: OnnxConfig = load_onnx_config(fixture()).expect("onnx.yaml must parse");

    let linux = cfg
        .platform_for_key("linux-amd64")
        .expect("known platform resolves");
    assert_eq!(
        linux.archive_url,
        "https://github.com/microsoft/onnxruntime/releases/download/v1.28.0/onnxruntime-linux-x64-1.28.0.tgz"
    );

    assert!(cfg.platform_for_key("linux-riscv64").is_none());
    assert!(cfg.platform_for_key("").is_none());
}

#[test]
fn model_for_name_matches_oracle_semantics() {
    // Criterion (c): known name -> Some (vector_dim=1024, 3 files); unknown -> None.
    let cfg: OnnxConfig = load_onnx_config(fixture()).expect("onnx.yaml must parse");

    let m = cfg
        .model_for_name("bge-m3-int8")
        .expect("known model resolves");
    assert_eq!(m.vector_dim, 1024);
    assert_eq!(m.files.len(), 3);

    assert!(cfg.model_for_name("bge-large-zh-v1.5").is_none());
    assert!(cfg.model_for_name("").is_none());
}

#[test]
fn missing_file_is_an_error_carrying_the_path() {
    // Criterion (d) / spec scenario "Отсутствующий onnx.yaml".
    let missing =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/definitely-missing-onnx.yaml");
    match load_onnx_config(&missing) {
        Err(ConfigError::Io { path, .. }) => {
            assert!(path.ends_with("definitely-missing-onnx.yaml"))
        }
        other => panic!("expected Io error for missing file, got: {other:?}"),
    }
}

#[test]
fn invalid_yaml_is_a_parse_error_carrying_the_path() {
    // Criterion (e). A temp file keeps the repo free of broken fixtures.
    let path = std::env::temp_dir().join(format!(
        "synopsis-config-onnx-test-{}.yaml",
        std::process::id()
    ));
    std::fs::write(&path, "runtime:\n  version: [broken\n").expect("write temp fixture");

    match load_onnx_config(&path) {
        Err(ConfigError::Yaml { path: p, .. }) => assert!(p.ends_with(".yaml")),
        other => panic!("expected Yaml error for invalid document, got: {other:?}"),
    }
    let _ = std::fs::remove_file(&path);
}
