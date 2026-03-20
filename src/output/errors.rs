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
}

impl WriteError {
    /// Create a new WriteError.
    pub fn new(path: String, cause: String) -> Self {
        Self { path, cause }
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
    fn test_write_error_is_std_error() {
        let err = WriteError::new("/file".into(), "io error".into());
        // Verify it implements std::error::Error (source returns None by default)
        let std_err: &dyn std::error::Error = &err;
        assert!(std_err.source().is_none());
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
