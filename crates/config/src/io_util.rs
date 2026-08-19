//! Private read-and-parse helpers shared by every file-based loader in this crate.
//!
//! The YAML loaders ([`crate::preset::load`], [`crate::onnx::load_onnx_config`]) and the XML
//! ontology loaders (task 3.1/3.2) all follow one shape: read bytes → validate UTF-8 where a
//! string is needed (`hint` names the file kind in that failure message) → deserialize →
//! decorate every error with the offending path. Keeping it here means one [`display_path`] and
//! no duplicated I/O boilerplate (loader-duplication refactor, task 3.1).

use std::path::Path;

use serde::de::DeserializeOwned;

use crate::error::ConfigError;

/// Renders a path for diagnostics (lossy conversion is acceptable in error messages).
pub(crate) fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Reads and parses a YAML config file at `path` into `T`.
///
/// `hint` names the file kind in the UTF-8 failure message (`"config"` for the main preset,
/// `"onnx config"` for the model registry), preserving each loader's historical wording.
pub(crate) fn read_yaml_file<T>(path: &Path, hint: &str) -> Result<T, ConfigError>
where
    T: DeserializeOwned + 'static,
{
    let data = std::fs::read(path).map_err(|source| ConfigError::Io {
        path: display_path(path),
        source,
    })?;
    let text = String::from_utf8(data).map_err(|e| ConfigError::Yaml {
        path: display_path(path),
        source: noyalib::Error::Custom(format!("{hint} file is not valid UTF-8: {e}")),
    })?;
    noyalib::from_str(&text).map_err(|source| ConfigError::Yaml {
        path: display_path(path),
        source,
    })
}

/// Reads and parses an XML config file at `path` into `T`.
///
/// quick-xml consumes the bytes directly (undecodable input surfaces as its parse error), so —
/// unlike [`read_yaml_file`] — there is no separate UTF-8 step; both failure kinds land in
/// [`ConfigError::Xml`] carrying the path. Callers that need Go-style "missing file → `None`"
/// semantics check existence before calling (see [`crate::ontology::load_global_config`]).
pub(crate) fn read_xml_file<T>(path: &Path) -> Result<T, ConfigError>
where
    T: DeserializeOwned,
{
    let data = std::fs::read(path).map_err(|source| ConfigError::Io {
        path: display_path(path),
        source,
    })?;
    quick_xml::de::from_reader(std::io::Cursor::new(data)).map_err(|source| ConfigError::Xml {
        path: display_path(path),
        source,
    })
}
