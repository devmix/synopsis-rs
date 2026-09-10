//! Integration tests for the dataset alias map loader (`ontology/aliases.yaml`,
//! multilingual-entity-resolution task 4.1).
//!
//! Covers the five acceptance scenarios — missing file → empty map, empty file → empty map,
//! valid map → loaded, non-mapping document → configuration error, duplicate alias key →
//! configuration error (detected explicitly via `DuplicateKeyPolicy::Error`, not the YAML 1.2
//! silent last-wins default) — plus a non-string canonical value. Generic words only
//! (change-wide NDA rule).

// Test target: unwrap/expect on temp-dir setup is intentional (setup failures are test bugs).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;

use config::{ConfigError, load_aliases};

/// Owns a temp ontology dir; removed on drop even when a test panics. One instance per
/// scenario keeps parallel tests from clobbering each other.
struct TempOntology(PathBuf);

impl TempOntology {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("synopsis-aliases-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn with_aliases_file(&self, document: &str) {
        std::fs::write(self.0.join("aliases.yaml"), document).unwrap();
    }

    fn load(&self) -> Result<HashMap<String, String>, ConfigError> {
        load_aliases(&self.0)
    }
}

impl Drop for TempOntology {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Scenario 1: file absent → empty map, no error.
#[test]
fn missing_file_yields_empty_map() {
    let dir = TempOntology::new("missing");
    assert!(
        dir.load()
            .expect("a missing aliases.yaml must not be an error")
            .is_empty()
    );
}

/// Scenario 2: empty file (and other null-equivalent documents) → empty map.
#[test]
fn empty_file_yields_empty_map() {
    let cases = [
        ("zero-bytes", ""),
        ("whitespace", "  \n\n  \n"),
        ("explicit-null", "null\n"),
    ];
    for (name, document) in cases {
        let dir = TempOntology::new(name);
        dir.with_aliases_file(document);
        assert!(
            dir.load()
                .expect("a null-equivalent aliases.yaml must not be an error")
                .is_empty(),
            "{name}: expected an empty map"
        );
    }
}

/// Scenario 3: valid map → loaded as-is.
#[test]
fn valid_map_is_loaded() {
    let dir = TempOntology::new("valid");
    dir.with_aliases_file("alias-a: canonical-a\nalias-b: canonical-b\nalias-c: canonical-c\n");
    let aliases = dir.load().expect("a valid alias map must load");
    let expected = HashMap::from([
        ("alias-a".to_string(), "canonical-a".to_string()),
        ("alias-b".to_string(), "canonical-b".to_string()),
        ("alias-c".to_string(), "canonical-c".to_string()),
    ]);
    assert_eq!(aliases, expected);
}

/// Scenario 4: a document that is not a mapping → configuration error naming the file.
#[test]
fn non_mapping_document_is_rejected() {
    let cases = [
        ("scalar", "just-a-string\n"),
        ("sequence", "- alias-a\n- alias-b\n"),
        ("number", "42\n"),
    ];
    for (name, document) in cases {
        let dir = TempOntology::new(name);
        dir.with_aliases_file(document);
        match dir.load() {
            Err(ConfigError::Validation { message }) => {
                assert!(
                    message.contains("aliases.yaml"),
                    "the error should name the offending file: {message}"
                );
            }
            other => panic!("expected a Validation error for {name}, got: {other:?}"),
        }
    }
}

/// Scenario 5: the same alias key twice (two canonicals) → parse error, detected explicitly —
/// not the YAML 1.2 silent last-wins default (which would load `alias-a → canonical-b`).
#[test]
fn duplicate_alias_key_is_rejected() {
    let dir = TempOntology::new("duplicate");
    dir.with_aliases_file("alias-a: canonical-a\nalias-a: canonical-b\n");
    match dir.load() {
        Err(ConfigError::Yaml { path, source }) => {
            assert!(path.ends_with("aliases.yaml"), "path: {path}");
            let message = source.to_string();
            assert!(
                message.contains("alias-a"),
                "the error should name the duplicate key: {message}"
            );
        }
        other => panic!("expected a Yaml (parse) error for a duplicate key, got: {other:?}"),
    }
}

/// A canonical value that is not a string (e.g. a nested mapping) → configuration error naming
/// the offending alias.
#[test]
fn non_string_canonical_is_rejected() {
    let dir = TempOntology::new("non-string-canonical");
    dir.with_aliases_file("alias-a: {nested: value}\n");
    match dir.load() {
        Err(ConfigError::Validation { message }) => {
            assert!(
                message.contains("alias-a"),
                "the error should name the offending alias: {message}"
            );
        }
        other => panic!("expected a Validation error, got: {other:?}"),
    }
}
