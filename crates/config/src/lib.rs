//! Configuration loading and validation for Synopsis (Rust).
//!
//! This crate ports the Go oracle's `internal/config` package: YAML presets
//! ([`preset`]), the external ONNX model registry (`onnx.yaml`, task 2.1) and
//! the XML ontologies (task 3.x). Oracle mapping: `../synopsis/internal/config`.
//!
//! The main entry point is [`load`](preset::load), which parses a YAML config
//! file into a typed [`Config`] with no defaults applied and no validation —
//! callers drive those as separate phases ([`Config::apply_defaults`] /
//! [`Config::validate`]). The external `onnx.yaml` registry loads through
//! [`load_onnx_config`](onnx::load_onnx_config), and the global ontology
//! (`global.xml`) through [`load_global_config`](ontology::load_global_config).

pub mod error;
pub mod onnx;
pub mod ontology;
pub mod preset;

/// Private read+parse helpers shared by the file loaders above (task 3.1).
mod io_util;

// Public re-exports so consumers can write `config::Config`, `config::load`, …
pub use error::ConfigError;
pub use onnx::{OnnxConfig, load_onnx_config};
pub use ontology::{GlobalConfig, load_global_config};
pub use preset::{Config, load};
