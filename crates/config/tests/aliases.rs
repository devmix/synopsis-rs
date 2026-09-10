//! Integration tests for the dataset alias map (the ontology `<aliases>` blocks,
//! multilingual-entity-resolution task 4.3).
//!
//! Covers the five acceptance scenarios — (a) absent block → empty map, (b) global and/or
//! domain blocks → union map, (c) duplicate `name` within one file → load-time validation
//! error, (d) duplicate `name` across `global.xml` and a domain file → union-time validation
//! error, (e) empty `name`/`canonical` → load-time validation error. Generic words only
//! (change-wide NDA rule).

// Test target: unwrap/expect on temp-dir setup is intentional (setup failures are test bugs).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;

use config::{ConfigError, dataset_alias_map, load_domain_config, load_global_config};

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

    fn with_global(&self, document: &str) {
        std::fs::write(self.0.join("global.xml"), document).unwrap();
    }

    fn with_domain(&self, name: &str, document: &str) {
        let domains = self.0.join("domains");
        std::fs::create_dir_all(&domains).unwrap();
        std::fs::write(domains.join(format!("{name}.xml")), document).unwrap();
    }

    fn load_global(&self) -> Option<config::GlobalConfig> {
        load_global_config(&self.0).unwrap()
    }

    fn load_domains(&self, names: &[&str]) -> HashMap<String, config::DomainConfig> {
        names
            .iter()
            .map(|name| {
                let file = self.0.join("domains").join(format!("{name}.xml"));
                (name.to_string(), load_domain_config(&file).unwrap())
            })
            .collect()
    }
}

impl Drop for TempOntology {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Asserts `global.xml` fails to load with a [`ConfigError::Validation`] carrying exactly the
/// expected message.
fn assert_global_load_error(dir: &TempOntology, expected_message: &str) {
    match load_global_config(&dir.0) {
        Err(ConfigError::Validation { message }) => assert_eq!(message, expected_message),
        other => panic!("expected Validation error with {expected_message:?}, got: {other:?}"),
    }
}

/// Asserts `domains/{name}.xml` fails to load with a [`ConfigError::Validation`] carrying
/// exactly the expected message.
fn assert_domain_load_error(dir: &TempOntology, name: &str, expected_message: &str) {
    let file = dir.0.join("domains").join(format!("{name}.xml"));
    match load_domain_config(&file) {
        Err(ConfigError::Validation { message }) => assert_eq!(message, expected_message),
        other => panic!("expected Validation error with {expected_message:?}, got: {other:?}"),
    }
}

/// (a) No `<aliases>` block anywhere → empty map, no error — including the no-`global.xml`
/// case (the global config is optional).
#[test]
fn absent_block_yields_empty_map() {
    let dir = TempOntology::new("absent");
    dir.with_global(r#"<global version="1.0"></global>"#);
    dir.with_domain("alpha", r#"<domain name="alpha" version="1.0"></domain>"#);
    let global = dir.load_global();
    let domains = dir.load_domains(&["alpha"]);
    assert!(
        dataset_alias_map(global.as_ref(), &domains)
            .expect("no aliases anywhere: must not be an error")
            .is_empty()
    );

    // No global.xml at all → still an empty map.
    let bare = TempOntology::new("absent-bare");
    bare.with_domain("alpha", r#"<domain name="alpha" version="1.0"></domain>"#);
    let domains = bare.load_domains(&["alpha"]);
    assert!(
        dataset_alias_map(None, &domains)
            .expect("no global config: must not be an error")
            .is_empty()
    );
}

/// (b) A global block and/or domain blocks → the flat union map (alias name → canonical).
#[test]
fn global_and_domain_blocks_union() {
    // Global only.
    let dir = TempOntology::new("union-global");
    dir.with_global(
        r#"<global version="1.0">
            <aliases>
                <alias name="alias-a" canonical="canonical-a"/>
                <alias name="alias-b" canonical="canonical-b"/>
            </aliases>
        </global>"#,
    );
    let global = dir.load_global();
    let domains: HashMap<String, config::DomainConfig> = HashMap::new();
    let map = dataset_alias_map(global.as_ref(), &domains).expect("valid blocks must union");
    assert_eq!(map.len(), 2);
    assert_eq!(map["alias-a"], "canonical-a");
    assert_eq!(map["alias-b"], "canonical-b");

    // Global + two domains: every block contributes to the union.
    let dir = TempOntology::new("union-all");
    dir.with_global(
        r#"<global version="1.0">
            <aliases><alias name="alias-a" canonical="canonical-a"/></aliases>
        </global>"#,
    );
    dir.with_domain(
        "alpha",
        r#"<domain name="alpha" version="1.0">
            <aliases><alias name="alias-b" canonical="canonical-b"/></aliases>
        </domain>"#,
    );
    dir.with_domain(
        "beta",
        r#"<domain name="beta" version="1.0">
            <aliases><alias name="alias-c" canonical="canonical-c"/></aliases>
        </domain>"#,
    );
    let global = dir.load_global();
    let domains = dir.load_domains(&["alpha", "beta"]);
    let map = dataset_alias_map(global.as_ref(), &domains).expect("valid blocks must union");
    assert_eq!(map.len(), 3);
    assert_eq!(map["alias-a"], "canonical-a");
    assert_eq!(map["alias-b"], "canonical-b");
    assert_eq!(map["alias-c"], "canonical-c");

    // Domain blocks only (no global.xml).
    let bare = TempOntology::new("union-domains");
    bare.with_domain(
        "alpha",
        r#"<domain name="alpha" version="1.0">
            <aliases><alias name="alias-d" canonical="canonical-d"/></aliases>
        </domain>"#,
    );
    let domains = bare.load_domains(&["alpha"]);
    let map = dataset_alias_map(None, &domains).expect("domain-only blocks must union");
    assert_eq!(map.len(), 1);
    assert_eq!(map["alias-d"], "canonical-d");
}

/// (c) The same `name` twice within one file → load-time validation error (both layers).
#[test]
fn duplicate_name_within_file_is_rejected() {
    let dir = TempOntology::new("dup-global");
    dir.with_global(
        r#"<global version="1.0">
            <aliases>
                <alias name="alias-a" canonical="canonical-a"/>
                <alias name="alias-a" canonical="canonical-b"/>
            </aliases>
        </global>"#,
    );
    assert_global_load_error(&dir, "duplicate alias name: alias-a");

    let dir = TempOntology::new("dup-domain");
    dir.with_domain(
        "alpha",
        r#"<domain name="alpha" version="1.0">
            <aliases>
                <alias name="alias-a" canonical="canonical-a"/>
                <alias name="alias-a" canonical="canonical-b"/>
            </aliases>
        </domain>"#,
    );
    assert_domain_load_error(&dir, "alpha", "duplicate alias name: alias-a");
}

/// (d) The same `name` in `global.xml` and a domain file (or in two domain files) → union-time
/// validation error naming both sources; each file is individually valid.
#[test]
fn duplicate_name_across_files_is_rejected() {
    let dir = TempOntology::new("dup-cross");
    dir.with_global(
        r#"<global version="1.0">
            <aliases><alias name="alias-a" canonical="canonical-a"/></aliases>
        </global>"#,
    );
    dir.with_domain(
        "alpha",
        r#"<domain name="alpha" version="1.0">
            <aliases><alias name="alias-a" canonical="canonical-b"/></aliases>
        </domain>"#,
    );
    let global = dir.load_global();
    let domains = dir.load_domains(&["alpha"]);
    // Each file is individually valid — the error comes from the union, not the loaders.
    assert_eq!(
        global.as_ref().expect("global.xml must load").aliases.len(),
        1
    );
    match dataset_alias_map(global.as_ref(), &domains) {
        Err(ConfigError::Validation { message }) => {
            assert_eq!(
                message,
                "duplicate alias name: alias-a (global.xml and domain alpha)"
            );
        }
        other => panic!("expected a cross-file duplicate error, got: {other:?}"),
    }

    // Two domain files: sorted domain order names the sources deterministically.
    let dir = TempOntology::new("dup-domains");
    dir.with_domain(
        "alpha",
        r#"<domain name="alpha" version="1.0">
            <aliases><alias name="alias-a" canonical="canonical-a"/></aliases>
        </domain>"#,
    );
    dir.with_domain(
        "beta",
        r#"<domain name="beta" version="1.0">
            <aliases><alias name="alias-a" canonical="canonical-b"/></aliases>
        </domain>"#,
    );
    let domains = dir.load_domains(&["alpha", "beta"]);
    match dataset_alias_map(None, &domains) {
        Err(ConfigError::Validation { message }) => {
            assert_eq!(
                message,
                "duplicate alias name: alias-a (domain alpha and domain beta)"
            );
        }
        other => panic!("expected a cross-file duplicate error, got: {other:?}"),
    }
}

/// (e) An empty `name` or `canonical` → load-time validation error naming the alias position
/// (numbered from 1).
#[test]
fn empty_name_or_canonical_is_rejected() {
    let dir = TempOntology::new("empty-name");
    dir.with_global(
        r#"<global version="1.0">
            <aliases><alias name="" canonical="canonical-a"/></aliases>
        </global>"#,
    );
    assert_global_load_error(&dir, "alias 1: name is required");

    let dir = TempOntology::new("empty-canonical");
    dir.with_global(
        r#"<global version="1.0">
            <aliases><alias name="alias-a" canonical=""/></aliases>
        </global>"#,
    );
    assert_global_load_error(&dir, "alias 1: canonical is required");

    // Missing attributes entirely are rejected the same way.
    let dir = TempOntology::new("missing-attributes");
    dir.with_domain(
        "alpha",
        r#"<domain name="alpha" version="1.0">
            <aliases><alias/></aliases>
        </domain>"#,
    );
    assert_domain_load_error(&dir, "alpha", "alias 1: name is required");

    // Positions are numbered from 1 across the block.
    let dir = TempOntology::new("empty-second");
    dir.with_domain(
        "alpha",
        r#"<domain name="alpha" version="1.0">
            <aliases>
                <alias name="alias-a" canonical="canonical-a"/>
                <alias name="" canonical="canonical-b"/>
            </aliases>
        </domain>"#,
    );
    assert_domain_load_error(&dir, "alpha", "alias 2: name is required");
}
