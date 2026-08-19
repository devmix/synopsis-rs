//! ONNX model registry: typed structures for the external `onnx.yaml`, plus loading and lookup.
//!
//! This module mirrors the Go oracle's `internal/config/config.go` ONNX half: the
//! `ONNXConfig` / `ONNXRuntimeConfig` / `ONNXPlatformConfig` / `ONNXModelsConfig` /
//! `ModelInfo` / `ModelFile` structures, `LoadONNXConfig`, and the `PlatformForKey` /
//! `ModelForName` lookups. The registry is an *external* file (referenced from the main
//! config via `paths.onnx_config`) that lists the ONNX Runtime platform archives and the
//! embedding models available for download — performing downloads belongs to the
//! `embedding` crate, not here.
//!
//! Semantics stay faithful to the oracle: a missing or unreadable file is an
//! [`ConfigError::Io`] carrying the path, an unparseable document is
//! [`ConfigError::Yaml`] (spec scenario "Отсутствующий onnx.yaml"), unknown keys are
//! ignored, and [`ArchiveFormat`] is tolerant by design D7 — Go stores it as a plain string
//! that only the downloader interprets ("zip"/"tgz", else an error at download time), so an
//! unrecognized value must not fail config loading.

use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ConfigError;

// ── Root ──────────────────────────────────────────────────────────────────

/// External ONNX configuration loaded from `onnx.yaml` (oracle `ONNXConfig`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OnnxConfig {
    /// ONNX Runtime version and platform definitions.
    pub runtime: OnnxRuntimeConfig,
    /// Model registry and default model name.
    pub models: OnnxModelsConfig,
}

impl OnnxConfig {
    /// Returns the platform entry whose `key` matches (e.g. `"linux-amd64"`).
    ///
    /// Oracle parity (`ONNXRuntimeConfig.PlatformForKey`): first match wins; `None` for an
    /// unknown key.
    pub fn platform_for_key(&self, key: &str) -> Option<&OnnxPlatformConfig> {
        self.runtime
            .platforms
            .iter()
            .find(|platform| platform.key == key)
    }

    /// Returns the model entry whose `name` matches (e.g. `"bge-m3-int8"`).
    ///
    /// Oracle parity (`ONNXModelsConfig.ModelForName`): first match wins; `None` for an
    /// unknown name.
    pub fn model_for_name(&self, name: &str) -> Option<&ModelInfo> {
        self.models.entries.iter().find(|model| model.name == name)
    }
}

// ── Runtime / platforms ───────────────────────────────────────────────────

/// ONNX Runtime version and platform definitions (oracle `ONNXRuntimeConfig`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OnnxRuntimeConfig {
    /// Runtime version string (e.g. `"1.28.0"`).
    pub version: String,
    /// Per-platform download definitions.
    pub platforms: Vec<OnnxPlatformConfig>,
}

/// Download info for a single platform (oracle `ONNXPlatformConfig`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OnnxPlatformConfig {
    /// Platform key used in lookups (e.g. `"linux-amd64"`).
    pub key: String,
    /// OS name (`"windows"`, `"linux"`, `"darwin"`).
    pub os: String,
    /// CPU architecture (`"amd64"`, `"arm64"`).
    pub arch: String,
    /// URL of the release archive to download.
    pub archive_url: String,
    /// Archive container format (`"zip"` or `"tgz"`; tolerant — see [`ArchiveFormat`]).
    pub archive_format: ArchiveFormat,
    /// Library file name inside the archive (e.g. `"libonnxruntime.so.1.28.0"`).
    pub library_name: String,
    /// Path of the library relative to the archive root.
    pub library_path: String,
}

/// Archive container format for a platform entry (design D7: tolerant enum — the oracle
/// keeps this as a plain string and only interprets `"zip"`/`"tgz"` at download time, so an
/// unrecognized value is preserved verbatim instead of failing config loading).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// ZIP archive.
    Zip,
    /// Tar-gz archive.
    Tgz,
    /// A format the schema does not recognize (kept lowercase), or an absent/empty value —
    /// rejected later by download logic exactly like Go's "unsupported archive format".
    Unknown(String),
}

impl Default for ArchiveFormat {
    fn default() -> Self {
        // The oracle leaves this field empty when the key is absent, and there is no oracle
        // default to fall back on (unlike D12's five normalized enums). `Unknown("")` keeps
        // that state verbatim — same treatment as strict enums without a default
        // (`EmbeddingsMode`) — rather than fabricating a download format.
        Self::Unknown(String::new())
    }
}

impl Serialize for ArchiveFormat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Zip => serializer.serialize_str("zip"),
            Self::Tgz => serializer.serialize_str("tgz"),
            Self::Unknown(format) => serializer.serialize_str(format),
        }
    }
}

impl<'de> Deserialize<'de> for ArchiveFormat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        // Case-insensitive match on the known words (oracle uses lowercase); unknown values
        // are stored lowercased like every other tolerant enum in this crate, and an empty
        // string stays `Unknown("")` (see [`Default for ArchiveFormat`]).
        Ok(match raw.to_ascii_lowercase().as_str() {
            "zip" => Self::Zip,
            "tgz" => Self::Tgz,
            other => Self::Unknown(other.to_string()),
        })
    }
}

// ── Models ────────────────────────────────────────────────────────────────

/// Model registry and default model name (oracle `ONNXModelsConfig`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OnnxModelsConfig {
    /// Name of the default model (must match an entry in [`Self::entries`]).
    pub default: String,
    /// Models available in the registry.
    pub entries: Vec<ModelInfo>,
}

/// A model available in the registry (oracle `ModelInfo`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelInfo {
    /// Registry name used in lookups (e.g. `"bge-m3-int8"`).
    pub name: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Description of the model.
    pub description: String,
    /// Version string.
    pub version: String,
    /// Embedding vector dimension produced by this model.
    pub vector_dim: i32,
    /// Files that must be downloaded for this model.
    pub files: Vec<ModelFile>,
    /// Source registry (`"huggingface"`, `"github"`).
    pub source: String,
    /// Repository identifier (e.g. `"BAAI/bge-m3"`); empty when not applicable.
    pub repo: String,
}

/// A single file belonging to an embedding model (oracle `ModelFile`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelFile {
    /// File name (e.g. `"model.onnx"`).
    pub name: String,
    /// Download URL.
    pub url: String,
    /// Exact size in bytes; 0 if unknown.
    pub size_bytes: i64,
    /// Checksum in `"sha256:hex"` format; `None` when the entry has no checksum.
    pub checksum: Option<String>,
}

// ── Loading ───────────────────────────────────────────────────────────────

/// Reads and parses the external ONNX registry at `path` into an [`OnnxConfig`].
///
/// Oracle parity (`LoadONNXConfig`): a missing or unreadable file yields
/// [`ConfigError::Io`] carrying the path, and an unparseable document (including non-UTF-8
/// bytes) yields [`ConfigError::Yaml`] — both keep the path so callers can report which
/// registry failed. Unknown keys are ignored, matching the oracle. This performs parsing
/// only; there is no validation phase for this file in the oracle either.
pub fn load_onnx_config(path: impl AsRef<Path>) -> Result<OnnxConfig, ConfigError> {
    crate::io_util::read_yaml_file(path.as_ref(), "onnx config")
}

#[cfg(test)]
mod tests {
    // Test code: unwrap/expect are intentional (fixtures always parse).
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn parse(yaml: &str) -> OnnxConfig {
        noyalib::from_str(yaml).expect("test YAML should parse")
    }

    #[test]
    fn archive_format_maps_known_values_case_insensitively() {
        let cfg = parse(
            r#"runtime:
  platforms:
    - key: a
      archive_format: zip
    - key: b
      archive_format: TGZ
"#,
        );
        assert_eq!(cfg.runtime.platforms[0].archive_format, ArchiveFormat::Zip);
        assert_eq!(cfg.runtime.platforms[1].archive_format, ArchiveFormat::Tgz);
    }

    #[test]
    fn archive_format_preserves_unknown_and_empty_values() {
        // Tolerant (D7): an unrecognized value must not fail loading. Unknown values are
        // stored lowercased like every other tolerant enum in this crate; "" stays
        // Unknown("") because the field has no oracle default.
        let cfg = parse(
            r#"runtime:
  platforms:
    - key: a
      archive_format: "7z"
    - key: b
      archive_format: ""
"#,
        );
        assert_eq!(
            cfg.runtime.platforms[0].archive_format,
            ArchiveFormat::Unknown("7z".into())
        );
        assert_eq!(
            cfg.runtime.platforms[1].archive_format,
            ArchiveFormat::Unknown(String::new())
        );

        // An absent key decodes to the same state as the oracle's empty string.
        let absent = parse("runtime:\n  platforms:\n    - key: c\n");
        assert_eq!(
            absent.runtime.platforms[0].archive_format,
            ArchiveFormat::Unknown(String::new())
        );
    }

    #[test]
    fn archive_format_round_trips_through_noyalib() {
        let doc = (ArchiveFormat::Zip, ArchiveFormat::Tgz);
        let yaml = noyalib::to_string(&doc).expect("serialize");
        assert_eq!(
            noyalib::from_str::<(ArchiveFormat, ArchiveFormat)>(&yaml).unwrap(),
            doc
        );

        let unknown = (ArchiveFormat::Unknown("7z".into()),);
        let yaml2 = noyalib::to_string(&unknown).expect("serialize");
        assert_eq!(
            noyalib::from_str::<(ArchiveFormat,)>(&yaml2).unwrap(),
            unknown
        );
    }

    #[test]
    fn model_file_checksum_is_optional() {
        let cfg = parse(
            r#"models:
  entries:
    - name: m
      files:
        - name: a.onnx
          url: http://x/a
          size_bytes: 1
          checksum: sha256:abc
        - name: b.json
          url: http://x/b
"#,
        );
        let files = &cfg.models.entries[0].files;
        assert_eq!(files[0].checksum.as_deref(), Some("sha256:abc"));
        assert!(files[1].checksum.is_none());
    }

    #[test]
    fn unknown_keys_are_ignored_like_in_the_oracle() {
        let cfg = parse(
            r#"runtime:
  version: "9.9"
  future_field: whatever
models:
  default: m
  entries:
    - name: m
      vector_dim: 8
"#,
        );
        assert_eq!(cfg.runtime.version, "9.9");
        assert_eq!(cfg.models.default, "m");
        assert_eq!(cfg.model_for_name("m").unwrap().vector_dim, 8);
    }

    #[test]
    fn lookups_return_none_on_empty_registry() {
        let cfg = OnnxConfig::default();
        assert!(cfg.platform_for_key("linux-amd64").is_none());
        assert!(cfg.model_for_name("bge-m3-int8").is_none());
    }

    #[test]
    fn load_missing_file_returns_io_error_with_path() {
        let missing = Path::new("/definitely/not/here/onnx.yaml");
        match load_onnx_config(missing) {
            Err(ConfigError::Io { path, .. }) => assert!(path.ends_with("onnx.yaml")),
            other => panic!("expected Io error for missing file, got: {other:?}"),
        }
    }

    #[test]
    fn load_invalid_yaml_returns_parse_error() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "synopsis-config-onnx-test-{}.yaml",
            std::process::id()
        ));
        // Syntactically broken YAML (unclosed flow sequence).
        std::fs::write(&path, "runtime:\n  version: [broken\n").expect("write temp fixture");
        match load_onnx_config(&path) {
            Err(ConfigError::Yaml { path: p, .. }) => assert!(p.ends_with(".yaml")),
            other => panic!("expected Yaml error for invalid document, got: {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }
}
