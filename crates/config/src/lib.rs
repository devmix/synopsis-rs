//! Configuration loading and validation for Synopsis.
//!
//! This crate loads and validates the application configuration: YAML presets
//! ([`preset`]), the external ONNX model registry (`onnx.yaml`, task 2.1) and
//! the XML ontologies (task 3.x).
//!
//! The main entry point is [`load`](preset::load), which parses a YAML config
//! file into a typed [`Config`] with no defaults applied and no validation —
//! callers drive those as separate phases ([`Config::apply_defaults`] /
//! [`Config::validate`]). The external `onnx.yaml` registry loads through
//! [`load_onnx_config`](onnx::load_onnx_config), the global ontology
//! (`global.xml`) through [`load_global_config`](ontology::load_global_config)
//! and each per-domain ontology (`domains/*.xml`) through
//! [`load_domain_config`](domain::load_domain_config). The optional per-dataset
//! alias map lives in the ontology files themselves (the `<aliases>` blocks
//! of `global.xml` / `domains/*.xml`, parsed into the ontology configs) and is
//! derived as a flat name→canonical map through
//! [`dataset_alias_map`](aliases::dataset_alias_map). The SQLite batch-size
//! constants of the `db` crate live in [`db`] (single source of truth,
//! db-module task 1.18).

pub mod aliases;
pub mod db;
pub mod domain;
pub mod error;
pub mod onnx;
pub mod ontology;
pub mod preset;

/// Private read+parse helpers shared by the file loaders above (task 3.1).
mod io_util;

// Public re-exports so consumers can write `config::Config`, `config::load`, …
pub use aliases::dataset_alias_map;
pub use db::{ID_BATCH_SIZE, LINK_BATCH_SIZE};
pub use domain::{ConfidencePolicy, DomainConfig, EffectiveConfidence, load_domain_config};
pub use error::ConfigError;
pub use onnx::{OnnxConfig, load_onnx_config};
pub use ontology::{AliasDef, GlobalConfig, load_global_config};
pub use preset::{Config, load};
