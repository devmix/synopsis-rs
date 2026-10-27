//! Dataset alias map (the ontology `<aliases>` blocks, multilingual-entity-resolution task 4.3).
//!
//! Both `global.xml` and each `domains/*.xml` MAY carry an optional top-level `<aliases>` block
//! of `<alias name="..." canonical="..."/>` entries (design D4, revised 2026-09-10): an alias
//! surface name → the canonical name of the same real-world entity. The blocks are parsed by
//! the ontology loaders ([`crate::ontology`], [`crate::domain`]) into the config structs and
//! validated per file (non-empty `name`/`canonical`, unique `name` within the file). This
//! module derives the effective dataset map — the flat union of the global block and all
//! domain blocks — and enforces the one invariant that spans files: a duplicate `name` across
//! the dataset is a configuration error (one alias maps to exactly one canonical name).
//!
//! The original `ontology/aliases.yaml` loader (task 4.1) was reverted by this task: the alias
//! map lives in the ontology files themselves, and the bootstrap derives it from the
//! already-parsed ontology configs — no separate file read.

use std::collections::HashMap;

use crate::domain::DomainConfig;
use crate::error::ConfigError;
use crate::ontology::GlobalConfig;

/// Builds a [`ConfigError::Validation`] from a message (same per-module helper as
/// [`crate::ontology`] and [`crate::domain`]).
fn validation(message: impl Into<String>) -> ConfigError {
    ConfigError::Validation {
        message: message.into(),
    }
}

/// Derives the effective dataset alias map: the flat union of the global `<aliases>` block and
/// every domain `<aliases>` block (alias surface name → canonical name).
///
/// Expects configs returned by [`crate::load_global_config`] / [`crate::load_domain_config`]:
/// the per-file invariants (non-empty `name`/`canonical`, unique `name` within a file) are
/// already enforced there. This function enforces the cross-file invariant — the same `name`
/// in more than one file is a [`ConfigError::Validation`] naming both sources. Domains are
/// merged in sorted name order so the error is deterministic.
///
/// An absent global config and/or an empty domain set yields an empty map (no aliases): the
/// feature behaves as if disabled.
pub fn dataset_alias_map(
    global: Option<&GlobalConfig>,
    domains: &HashMap<String, DomainConfig>,
) -> Result<HashMap<String, String>, ConfigError> {
    let mut map: HashMap<String, String> = HashMap::new();
    // Where each alias name was first seen — named in the cross-file duplicate error.
    let mut sources: HashMap<String, String> = HashMap::new();

    if let Some(cfg) = global {
        for alias in &cfg.aliases {
            insert_alias(
                &mut map,
                &mut sources,
                &alias.name,
                &alias.canonical,
                "global.xml",
            )?;
        }
    }

    // Sorted domain order: a cross-file duplicate names both sources deterministically.
    let mut names: Vec<&String> = domains.keys().collect();
    names.sort();
    for name in names {
        let source = format!("domain {name}");
        for alias in &domains[name].aliases {
            insert_alias(
                &mut map,
                &mut sources,
                &alias.name,
                &alias.canonical,
                &source,
            )?;
        }
    }
    Ok(map)
}

/// Inserts one alias into the union map, rejecting a duplicate `name` with the sources of both
/// occurrences named.
fn insert_alias(
    map: &mut HashMap<String, String>,
    sources: &mut HashMap<String, String>,
    name: &str,
    canonical: &str,
    source: &str,
) -> Result<(), ConfigError> {
    if let Some(existing) = sources.get(name) {
        return Err(validation(format!(
            "duplicate alias name: {name} ({existing} and {source})"
        )));
    }
    sources.insert(name.to_string(), source.to_string());
    map.insert(name.to_string(), canonical.to_string());
    Ok(())
}
