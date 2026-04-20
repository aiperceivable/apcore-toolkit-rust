// Output writers for ScannedModule data.
//
// Provides writers for different output formats (YAML, registry, HTTP proxy).

pub mod errors;
pub mod registry_writer;
pub mod types;
pub mod verifiers;
pub mod yaml_writer;

#[cfg(feature = "http-proxy")]
pub mod http_proxy_writer;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error returned by [`get_writer`] when an unrecognised format string is supplied.
#[derive(Debug, Error, PartialEq)]
pub enum OutputFormatError {
    /// The format string does not map to a known [`OutputFormat`] variant.
    #[error("Unknown output format: {0}")]
    Unknown(String),
}

/// Supported output format variants.
///
/// Used by `get_writer` to select the appropriate writer implementation.
/// Each variant corresponds to a distinct writer struct with its own `write()`
/// signature, so the factory returns the enum itself rather than a trait object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormat {
    /// Write `.binding.yaml` files to disk.
    Yaml,
    /// Register modules directly into an apcore Registry.
    Registry,
    /// Register modules as HTTP proxy modules (requires `http-proxy` feature).
    #[cfg(feature = "http-proxy")]
    HttpProxy,
}

/// Convenience factory that returns the `OutputFormat` variant for a given
/// format string.
///
/// # Accepted values (case-insensitive)
///
/// | Input | Variant |
/// |-------|---------|
/// | `"yaml"` | `OutputFormat::Yaml` |
/// | `"registry"` | `OutputFormat::Registry` |
/// | `"http_proxy"` / `"http-proxy"` / `"httpproxy"` | `OutputFormat::HttpProxy` |
///
/// Returns `Err` for unrecognised strings.
///
/// # Usage
///
/// ```rust
/// use apcore_toolkit::output::get_writer;
/// use apcore_toolkit::output::OutputFormat;
///
/// let fmt = get_writer("yaml").unwrap();
/// assert_eq!(fmt, OutputFormat::Yaml);
///
/// // Then instantiate the concrete writer:
/// match fmt {
///     OutputFormat::Yaml => { /* use YAMLWriter */ }
///     OutputFormat::Registry => { /* use RegistryWriter */ }
///     OutputFormat::HttpProxy => { /* use HTTPProxyRegistryWriter */ }
/// }
/// ```
pub fn get_writer(format: &str) -> Result<OutputFormat, OutputFormatError> {
    match format.to_ascii_lowercase().as_str() {
        "yaml" => Ok(OutputFormat::Yaml),
        "registry" => Ok(OutputFormat::Registry),
        #[cfg(feature = "http-proxy")]
        "http_proxy" | "http-proxy" | "httpproxy" => Ok(OutputFormat::HttpProxy),
        _ => Err(OutputFormatError::Unknown(format.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_writer_yaml() {
        assert_eq!(get_writer("yaml"), Ok(OutputFormat::Yaml));
    }

    #[test]
    fn test_get_writer_registry() {
        assert_eq!(get_writer("registry"), Ok(OutputFormat::Registry));
    }

    #[cfg(feature = "http-proxy")]
    #[test]
    fn test_get_writer_http_proxy_variants() {
        assert_eq!(get_writer("http_proxy"), Ok(OutputFormat::HttpProxy));
        assert_eq!(get_writer("http-proxy"), Ok(OutputFormat::HttpProxy));
        assert_eq!(get_writer("httpproxy"), Ok(OutputFormat::HttpProxy));
    }

    #[test]
    fn test_get_writer_case_insensitive() {
        assert_eq!(get_writer("YAML"), Ok(OutputFormat::Yaml));
        assert_eq!(get_writer("Registry"), Ok(OutputFormat::Registry));
    }

    #[cfg(feature = "http-proxy")]
    #[test]
    fn test_get_writer_case_insensitive_http_proxy() {
        assert_eq!(get_writer("HTTP_PROXY"), Ok(OutputFormat::HttpProxy));
    }

    #[test]
    fn test_get_writer_unknown() {
        assert!(get_writer("xml").is_err());
        assert!(get_writer("").is_err());
        assert!(get_writer("xml")
            .unwrap_err()
            .to_string()
            .contains("Unknown output format"));
    }

    #[test]
    fn test_output_format_serde_roundtrip() {
        let fmt = OutputFormat::Yaml;
        let json = serde_json::to_string(&fmt).unwrap();
        let deserialized: OutputFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, fmt);
    }
}
