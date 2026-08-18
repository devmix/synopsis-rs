//! Configuration loading and validation for Synopsis (Rust).
//!
//! This crate ports the Go oracle's `internal/config` package: YAML presets
//! ([`preset`]), the external ONNX model registry (`onnx.yaml`, task 2.1) and
//! the XML ontologies (task 3.x). Oracle mapping: `../synopsis/internal/config`.
//!
//! The main entry point is [`load`](preset::load), which parses a YAML config
//! file into a typed [`Config`] with no defaults applied and no validation —
//! callers drive those as separate phases ([`Config::apply_defaults`] /
//! [`Config::validate`]).

pub mod error;
pub mod preset;

// Public re-exports so consumers can write `config::Config`, `config::load`, …
pub use error::ConfigError;
pub use preset::{Config, load};
