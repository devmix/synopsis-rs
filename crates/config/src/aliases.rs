//! Dataset alias map (`ontology/aliases.yaml`) loading (multilingual-entity-resolution, task 4.1).
//!
//! Each dataset MAY provide an optional `aliases.yaml` in its ontology directory: a YAML mapping
//! of an alias surface name (string) to a canonical name (string). The file is optional — a
//! missing file, an empty file, or an explicit `null` document all load as an empty map (the
//! feature behaves as if it were disabled). A document that is not a mapping, or a canonical
//! value that is not a string, is a configuration error. A duplicate alias key (one alias mapped
//! to two canonicals) is rejected **explicitly** through
//! [`noyalib::DuplicateKeyPolicy::Error`] — never by the YAML 1.2 silent last-wins default — and
//! surfaces as a parse error carrying the file path.

use std::collections::HashMap;
use std::path::Path;

use noyalib::Value;

use crate::error::ConfigError;

/// File name of the dataset alias map inside the ontology directory.
pub const ALIASES_YAML_FILE: &str = "aliases.yaml";

/// Loads the dataset alias map from `ontology_dir/aliases.yaml`.
///
/// File-presence semantics mirror [`crate::ontology::load_global_config`]: a missing file yields
/// an empty map (no error), as do an empty file and an explicit `null` document. A structurally
/// valid mapping is returned as-is (as a [`HashMap`] — no order guarantee). A non-mapping
/// document, or a canonical value that is not a string, is a [`ConfigError::Validation`]; a
/// duplicate alias key (rejected explicitly, see the module docs) and any other read/parse
/// failure are a [`ConfigError::Io`] / [`ConfigError::Yaml`] carrying the file path.
pub fn load_aliases(
    ontology_dir: impl AsRef<Path>,
) -> Result<HashMap<String, String>, ConfigError> {
    let dir = ontology_dir.as_ref();
    let path = dir.join(ALIASES_YAML_FILE);
    let value = match std::fs::metadata(&path) {
        // Existence established; the read/parse pair and its error decoration live in `io_util`.
        Ok(_) => {
            let config = noyalib::ParserConfig::new()
                .duplicate_key_policy(noyalib::DuplicateKeyPolicy::Error);
            crate::io_util::read_yaml_file_with_config::<Value>(&path, "alias map", &config)?
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(source) => {
            return Err(ConfigError::Io {
                path: crate::io_util::display_path(&path),
                source,
            });
        }
    };
    alias_map_from_value(&path, value)
}

/// Maps a parsed document to the alias map: `null` → empty map, mapping → extracted string
/// pairs, anything else → a [`ConfigError::Validation`] naming the file and the shape found.
fn alias_map_from_value(path: &Path, value: Value) -> Result<HashMap<String, String>, ConfigError> {
    // An empty file and an explicit `null` document both parse to `Value::Null` → no aliases.
    let Value::Mapping(mapping) = value else {
        if value.is_null() {
            return Ok(HashMap::new());
        }
        return Err(ConfigError::Validation {
            message: format!(
                "alias map file {} must be a mapping of alias to canonical name, found {}",
                crate::io_util::display_path(path),
                describe(&value),
            ),
        });
    };
    let mut aliases = HashMap::with_capacity(mapping.len());
    for (alias, canonical) in mapping.iter() {
        let Some(canonical) = canonical.as_str() else {
            return Err(ConfigError::Validation {
                message: format!(
                    "alias map file {}: alias {alias:?} must map to a string canonical name",
                    crate::io_util::display_path(path),
                ),
            });
        };
        aliases.insert(alias.clone(), canonical.to_string());
    }
    Ok(aliases)
}

/// Short human-readable name of a [`Value`]'s shape for validation messages.
fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "a null document",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Sequence(_) => "a sequence",
        Value::Mapping(_) => "a mapping",
        Value::Tagged(_) => "a tagged value",
    }
}
