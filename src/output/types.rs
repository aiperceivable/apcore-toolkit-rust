// Shared types for output writers.

use serde::{Deserialize, Serialize};

/// Result of a single verifier check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResult {
    /// Whether the verification passed.
    pub ok: bool,
    /// Error message if verification failed.
    pub error: Option<String>,
}

impl VerifyResult {
    /// Create a passing result.
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }

    /// Create a failing result with an error message.
    pub fn fail(error: String) -> Self {
        Self {
            ok: false,
            error: Some(error),
        }
    }
}

/// Protocol for pluggable output verifiers.
///
/// Implementations check that a written artifact is well-formed
/// according to domain-specific rules.
pub trait Verifier {
    /// Verify the artifact at `path` for the given `module_id`.
    fn verify(&self, path: &str, module_id: &str) -> VerifyResult;
}

/// Result of writing a single module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteResult {
    /// The module that was written.
    pub module_id: String,
    /// Output file path (None for RegistryWriter).
    pub path: Option<String>,
    /// Whether verification passed (always true if verify=false).
    pub verified: bool,
    /// Error message if verification failed.
    pub verification_error: Option<String>,
}

impl WriteResult {
    /// Create a basic result for a module (dry-run or no-path).
    pub fn new(module_id: String) -> Self {
        Self {
            module_id,
            path: None,
            verified: true,
            verification_error: None,
        }
    }

    /// Create a result with a file path.
    pub fn with_path(module_id: String, path: String) -> Self {
        Self {
            module_id,
            path: Some(path),
            verified: true,
            verification_error: None,
        }
    }

    /// Create a failed verification result.
    pub fn failed(module_id: String, path: Option<String>, error: String) -> Self {
        Self {
            module_id,
            path,
            verified: false,
            verification_error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_result_ok() {
        let r = VerifyResult::ok();
        assert!(r.ok);
        assert!(r.error.is_none());
    }

    #[test]
    fn test_verify_result_fail() {
        let r = VerifyResult::fail("bad".into());
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("bad"));
    }

    #[test]
    fn test_write_result_new() {
        let r = WriteResult::new("test.mod".into());
        assert_eq!(r.module_id, "test.mod");
        assert!(r.verified);
        assert!(r.path.is_none());
    }

    #[test]
    fn test_write_result_with_path() {
        let r = WriteResult::with_path("m".into(), "/tmp/m.yaml".into());
        assert_eq!(r.path.as_deref(), Some("/tmp/m.yaml"));
    }

    #[test]
    fn test_write_result_failed() {
        let r = WriteResult::failed("m".into(), Some("/tmp/m.yaml".into()), "err".into());
        assert!(!r.verified);
        assert_eq!(r.verification_error.as_deref(), Some("err"));
    }

    #[test]
    fn test_write_result_failed_no_path() {
        let r = WriteResult::failed("m".into(), None, "no path error".into());
        assert!(!r.verified);
        assert!(r.path.is_none());
        assert_eq!(r.verification_error.as_deref(), Some("no path error"));
    }

    #[test]
    fn test_write_result_serde_roundtrip() {
        let r = WriteResult::with_path("mod.x".into(), "/tmp/mod.yaml".into());
        let json = serde_json::to_string(&r).unwrap();
        let deserialized: WriteResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.module_id, "mod.x");
        assert_eq!(deserialized.path.as_deref(), Some("/tmp/mod.yaml"));
        assert!(deserialized.verified);
        assert!(deserialized.verification_error.is_none());
    }

    #[test]
    fn test_verify_result_serde_roundtrip() {
        let r = VerifyResult::fail("bad input".into());
        let json = serde_json::to_string(&r).unwrap();
        let deserialized: VerifyResult = serde_json::from_str(&json).unwrap();
        assert!(!deserialized.ok);
        assert_eq!(deserialized.error.as_deref(), Some("bad input"));
    }
}
