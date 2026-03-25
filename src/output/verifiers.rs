// Built-in verifiers for output writers.
//
// Each verifier implements the Verifier trait and checks a specific
// aspect of a written artifact.

use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::output::types::{Verifier, VerifyResult};

/// Verify that a Rust source file has valid syntax.
///
/// Uses the `syn` crate to parse the file as a Rust source file.
/// Analogous to `SyntaxVerifier` in Python (which uses `ast.parse`)
/// and TypeScript (which does a basic readability check).
pub struct SyntaxVerifier;

impl Verifier for SyntaxVerifier {
    fn verify(&self, path: &str, _module_id: &str) -> VerifyResult {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => return VerifyResult::fail(format!("Cannot read file: {e}")),
        };

        if content.trim().is_empty() {
            return VerifyResult::fail("File is empty".into());
        }

        match syn::parse_file(&content) {
            Ok(_) => VerifyResult::ok(),
            Err(e) => VerifyResult::fail(format!("Invalid Rust syntax: {e}")),
        }
    }
}

/// Verify that a YAML binding file is parseable and contains required fields.
pub struct YAMLVerifier;

impl Verifier for YAMLVerifier {
    fn verify(&self, path: &str, _module_id: &str) -> VerifyResult {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => return VerifyResult::fail(format!("Cannot read file: {e}")),
        };

        let parsed: serde_yaml::Value = match serde_yaml::from_str(&content) {
            Ok(v) => v,
            Err(e) => return VerifyResult::fail(format!("Invalid YAML: {e}")),
        };

        let bindings = match parsed.get("bindings") {
            Some(b) => b,
            None => return VerifyResult::fail("Missing or empty 'bindings' list".into()),
        };

        let bindings_seq = match bindings.as_sequence() {
            Some(s) if !s.is_empty() => s,
            _ => return VerifyResult::fail("Missing or empty 'bindings' list".into()),
        };

        let first = &bindings_seq[0];
        for field in &["module_id", "target"] {
            match first.get(*field) {
                Some(v) if !v.is_null() => {}
                _ => {
                    return VerifyResult::fail(format!(
                        "Missing required field '{field}' in binding"
                    ))
                }
            }
        }

        VerifyResult::ok()
    }
}

/// Verify that a file contains valid JSON, with optional schema validation.
pub struct JSONVerifier {
    // Schema validation is omitted (would require jsonschema crate).
    // Only checks that the file is valid JSON.
}

impl JSONVerifier {
    /// Create a new JSONVerifier.
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for JSONVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Verifier for JSONVerifier {
    fn verify(&self, path: &str, _module_id: &str) -> VerifyResult {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => return VerifyResult::fail(format!("Cannot read file: {e}")),
        };

        match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(_) => VerifyResult::ok(),
            Err(e) => VerifyResult::fail(format!("Invalid JSON: {e}")),
        }
    }
}

/// Verify that a file starts with expected magic bytes.
pub struct MagicBytesVerifier {
    expected: Vec<u8>,
}

impl MagicBytesVerifier {
    /// Create a new MagicBytesVerifier with the expected byte sequence.
    pub fn new(expected: Vec<u8>) -> Self {
        Self { expected }
    }
}

impl Verifier for MagicBytesVerifier {
    fn verify(&self, path: &str, _module_id: &str) -> VerifyResult {
        let content = match fs::read(path) {
            Ok(c) => c,
            Err(e) => return VerifyResult::fail(format!("Cannot read file: {e}")),
        };

        if content.len() < self.expected.len() {
            return VerifyResult::fail(format!(
                "File too short: expected at least {} bytes, got {}",
                self.expected.len(),
                content.len()
            ));
        }

        let header = &content[..self.expected.len()];
        if header != self.expected.as_slice() {
            return VerifyResult::fail(format!(
                "Magic bytes mismatch: expected {:?}, got {:?}",
                self.expected, header
            ));
        }

        VerifyResult::ok()
    }
}

/// Verify that a module is registered and retrievable from a registry.
pub struct RegistryVerifier<'a> {
    registry: &'a apcore::Registry,
}

impl<'a> RegistryVerifier<'a> {
    /// Create a new RegistryVerifier checking against the given registry.
    pub fn new(registry: &'a apcore::Registry) -> Self {
        Self { registry }
    }
}

impl Verifier for RegistryVerifier<'_> {
    fn verify(&self, _path: &str, module_id: &str) -> VerifyResult {
        if self.registry.has(module_id) {
            VerifyResult::ok()
        } else {
            VerifyResult::fail(format!(
                "Module '{module_id}' not found in registry after registration"
            ))
        }
    }
}

/// Run verifiers in order; stop on first failure.
///
/// Each verifier call is wrapped in `catch_unwind` so that a panicking
/// verifier does not crash the caller. A panic is reported as a
/// `VerifyResult::fail` with a descriptive message.
pub fn run_verifier_chain(
    verifiers: &[&dyn Verifier],
    path: &str,
    module_id: &str,
) -> VerifyResult {
    for verifier in verifiers {
        let verifier = AssertUnwindSafe(verifier);
        let path = path.to_string();
        let module_id = module_id.to_string();
        let outcome = catch_unwind(move || verifier.verify(&path, &module_id));
        match outcome {
            Ok(result) if !result.ok => return result,
            Ok(_) => {} // passed, continue
            Err(panic_info) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                return VerifyResult::fail(format!("Verifier crashed: {msg}"));
            }
        }
    }
    VerifyResult::ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_yaml_verifier_valid() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "bindings:\n  - module_id: test\n    target: app:func").unwrap();
        let result = YAMLVerifier.verify(f.path().to_str().unwrap(), "test");
        assert!(result.ok);
    }

    #[test]
    fn test_yaml_verifier_invalid_yaml() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{{invalid: yaml: [}}").unwrap();
        let result = YAMLVerifier.verify(f.path().to_str().unwrap(), "test");
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Invalid YAML"));
    }

    #[test]
    fn test_yaml_verifier_missing_bindings() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "other_key: value").unwrap();
        let result = YAMLVerifier.verify(f.path().to_str().unwrap(), "test");
        assert!(!result.ok);
    }

    #[test]
    fn test_yaml_verifier_missing_required_field() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "bindings:\n  - module_id: test").unwrap();
        let result = YAMLVerifier.verify(f.path().to_str().unwrap(), "test");
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("target"));
    }

    #[test]
    fn test_json_verifier_valid() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"key": "value"}}"#).unwrap();
        let result = JSONVerifier::new().verify(f.path().to_str().unwrap(), "test");
        assert!(result.ok);
    }

    #[test]
    fn test_json_verifier_invalid() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "not json").unwrap();
        let result = JSONVerifier::new().verify(f.path().to_str().unwrap(), "test");
        assert!(!result.ok);
    }

    #[test]
    fn test_magic_bytes_verifier_match() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"\x89PNG\r\n\x1a\nrest of file").unwrap();
        let verifier = MagicBytesVerifier::new(b"\x89PNG\r\n\x1a\n".to_vec());
        let result = verifier.verify(f.path().to_str().unwrap(), "test");
        assert!(result.ok);
    }

    #[test]
    fn test_magic_bytes_verifier_mismatch() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"NOT PNG").unwrap();
        let verifier = MagicBytesVerifier::new(b"\x89PNG".to_vec());
        let result = verifier.verify(f.path().to_str().unwrap(), "test");
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("mismatch"));
    }

    #[test]
    fn test_run_verifier_chain_all_pass() {
        let v1 = JSONVerifier::new();
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"ok": true}}"#).unwrap();
        let verifiers: Vec<&dyn Verifier> = vec![&v1];
        let result = run_verifier_chain(&verifiers, f.path().to_str().unwrap(), "test");
        assert!(result.ok);
    }

    #[test]
    fn test_run_verifier_chain_stops_on_failure() {
        let v1 = JSONVerifier::new();
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "not json").unwrap();
        let verifiers: Vec<&dyn Verifier> = vec![&v1];
        let result = run_verifier_chain(&verifiers, f.path().to_str().unwrap(), "test");
        assert!(!result.ok);
    }

    #[test]
    fn test_run_verifier_chain_empty() {
        let verifiers: Vec<&dyn Verifier> = vec![];
        let result = run_verifier_chain(&verifiers, "", "test");
        assert!(result.ok);
    }

    #[test]
    fn test_yaml_verifier_nonexistent_file() {
        let result = YAMLVerifier.verify("/tmp/nonexistent_file_abc123.yaml", "test");
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Cannot read file"));
    }

    #[test]
    fn test_json_verifier_nonexistent_file() {
        let result = JSONVerifier::new().verify("/tmp/nonexistent_file_abc123.json", "test");
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Cannot read file"));
    }

    #[test]
    fn test_magic_bytes_verifier_file_too_short() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"AB").unwrap();
        let verifier = MagicBytesVerifier::new(b"ABCDEF".to_vec());
        let result = verifier.verify(f.path().to_str().unwrap(), "test");
        assert!(!result.ok);
        let err = result.error.unwrap();
        assert!(err.contains("File too short"), "got: {err}");
        assert!(err.contains("6"), "should mention expected length");
        assert!(err.contains("2"), "should mention actual length");
    }

    /// A verifier that panics, used to test catch_unwind in the chain.
    struct PanickingVerifier;

    impl Verifier for PanickingVerifier {
        fn verify(&self, _path: &str, _module_id: &str) -> VerifyResult {
            panic!("verifier exploded");
        }
    }

    #[test]
    fn test_run_verifier_chain_panic_caught() {
        let bad = PanickingVerifier;
        let verifiers: Vec<&dyn Verifier> = vec![&bad];
        let result = run_verifier_chain(&verifiers, "/fake", "test");
        assert!(!result.ok);
        let err = result.error.unwrap();
        assert!(
            err.contains("Verifier crashed"),
            "expected crash message, got: {err}"
        );
        assert!(
            err.contains("verifier exploded"),
            "expected panic message, got: {err}"
        );
    }

    /// A verifier that always returns an error, simulating a "crash" without panicking.
    struct AlwaysFailVerifier {
        message: String,
    }

    impl AlwaysFailVerifier {
        fn new(message: &str) -> Self {
            Self {
                message: message.to_string(),
            }
        }
    }

    impl Verifier for AlwaysFailVerifier {
        fn verify(&self, _path: &str, _module_id: &str) -> VerifyResult {
            VerifyResult::fail(self.message.clone())
        }
    }

    /// A verifier that always passes.
    struct AlwaysPassVerifier;

    impl Verifier for AlwaysPassVerifier {
        fn verify(&self, _path: &str, _module_id: &str) -> VerifyResult {
            VerifyResult::ok()
        }
    }

    #[test]
    fn test_run_verifier_chain_crash_caught() {
        let bad = AlwaysFailVerifier::new("simulated crash");
        let verifiers: Vec<&dyn Verifier> = vec![&bad];
        let result = run_verifier_chain(&verifiers, "/fake", "test");
        assert!(!result.ok);
        assert_eq!(result.error.as_deref(), Some("simulated crash"));
    }

    #[test]
    fn test_run_verifier_chain_first_failure_stops() {
        // The first verifier fails, so the second should never matter.
        let fail_v = AlwaysFailVerifier::new("first failed");
        let pass_v = AlwaysPassVerifier;
        let verifiers: Vec<&dyn Verifier> = vec![&fail_v, &pass_v];
        let result = run_verifier_chain(&verifiers, "/fake", "test");
        assert!(!result.ok);
        assert_eq!(result.error.as_deref(), Some("first failed"));
    }

    #[test]
    fn test_syntax_verifier_valid_rust() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "fn main() {{\n    println!(\"hello\");\n}}").unwrap();
        let result = SyntaxVerifier.verify(f.path().to_str().unwrap(), "test");
        assert!(result.ok, "expected ok, got: {:?}", result.error);
    }

    #[test]
    fn test_syntax_verifier_invalid_rust() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "fn main() {{{{{{").unwrap();
        let result = SyntaxVerifier.verify(f.path().to_str().unwrap(), "test");
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Invalid Rust syntax"));
    }

    #[test]
    fn test_syntax_verifier_empty_file() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "").unwrap();
        let result = SyntaxVerifier.verify(f.path().to_str().unwrap(), "test");
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("File is empty"));
    }

    #[test]
    fn test_syntax_verifier_nonexistent_file() {
        let result = SyntaxVerifier.verify("/tmp/nonexistent_rs_file_abc123.rs", "test");
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Cannot read file"));
    }

    #[test]
    fn test_syntax_verifier_whitespace_only() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "   \n\n  \t  ").unwrap();
        let result = SyntaxVerifier.verify(f.path().to_str().unwrap(), "test");
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("File is empty"));
    }

    #[test]
    fn test_syntax_verifier_complex_valid() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            "use std::collections::HashMap;\n\
             \n\
             pub struct Foo {{\n\
                 pub name: String,\n\
                 pub values: HashMap<String, i32>,\n\
             }}\n\
             \n\
             impl Foo {{\n\
                 pub fn new(name: String) -> Self {{\n\
                     Self {{ name, values: HashMap::new() }}\n\
                 }}\n\
             }}"
        )
        .unwrap();
        let result = SyntaxVerifier.verify(f.path().to_str().unwrap(), "test");
        assert!(result.ok, "expected ok, got: {:?}", result.error);
    }
}
