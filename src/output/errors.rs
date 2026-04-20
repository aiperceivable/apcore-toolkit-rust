// Error types for output writers.

use thiserror::Error;

/// Raised when a writer fails to write an artifact to disk.
#[derive(Debug, Error)]
#[error("Failed to write {path}: {cause}")]
pub struct WriteError {
    /// The file path that could not be written.
    pub path: String,
    /// Description of the underlying error.
    pub cause: String,
    /// Underlying I/O error, when available.
    ///
    /// Preserves `io::Error` kind and errno so callers can inspect the root
    /// cause. `None` for non-I/O errors (e.g. YAML serialization failures).
    #[source]
    pub io_source: Option<std::io::Error>,
}

impl WriteError {
    /// Create a WriteError without an I/O source (e.g. for serialization failures).
    pub fn new(path: String, cause: String) -> Self {
        Self {
            path,
            cause,
            io_source: None,
        }
    }

    /// Create a WriteError wrapping an I/O error, preserving the source chain.
    pub fn io(path: String, source: std::io::Error) -> Self {
        let cause = source.to_string();
        Self {
            path,
            cause,
            io_source: Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_error_display() {
        let err = WriteError::new("/tmp/test.yaml".into(), "permission denied".into());
        assert_eq!(
            err.to_string(),
            "Failed to write /tmp/test.yaml: permission denied"
        );
    }

    #[test]
    fn test_write_error_fields() {
        let err = WriteError::new("/path".into(), "cause".into());
        assert_eq!(err.path, "/path");
        assert_eq!(err.cause, "cause");
    }

    #[test]
    fn test_write_error_new_source_is_none() {
        let err = WriteError::new("/file".into(), "io error".into());
        let std_err: &dyn std::error::Error = &err;
        assert!(std_err.source().is_none());
    }

    #[test]
    fn test_write_error_io_source_is_some() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err = WriteError::io("/file".into(), io_err);
        let std_err: &dyn std::error::Error = &err;
        assert!(
            std_err.source().is_some(),
            "WriteError::io should expose the I/O error as source()"
        );
        assert_eq!(err.path, "/file");
        assert_eq!(err.cause, "access denied");
    }

    #[test]
    fn test_write_error_io_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let err = WriteError::io("/missing.yaml".into(), io_err);
        assert_eq!(
            err.to_string(),
            "Failed to write /missing.yaml: no such file"
        );
    }

    #[test]
    fn test_write_error_debug_format() {
        let err = WriteError::new("/tmp/out.yaml".into(), "disk full".into());
        let debug_str = format!("{err:?}");
        assert!(debug_str.contains("WriteError"));
        assert!(debug_str.contains("/tmp/out.yaml"));
        assert!(debug_str.contains("disk full"));
    }
}
