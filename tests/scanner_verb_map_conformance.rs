// Cross-language conformance fixture test.
//
// Reads the shared `tests/fixtures/scanner_verb_map.json` fixture — used
// by the Python and TypeScript SDKs as well — and asserts
// `generate_suggested_alias` produces identical aliases for every case.
// A new case added to the fixture is automatically exercised here without
// any additional Rust code.

use std::path::PathBuf;

use apcore_toolkit::generate_suggested_alias;

#[test]
fn generate_suggested_alias_matches_shared_conformance_fixture() {
    let fixture_path: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("scanner_verb_map.json");

    let content = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("failed to read fixture {:?}: {}", fixture_path, e));

    let cases: serde_json::Value =
        serde_json::from_str(&content).expect("fixture must be valid JSON");
    let array = cases.as_array().expect("fixture must be a JSON array");
    assert!(!array.is_empty(), "fixture must contain at least one case");

    for case in array {
        let path = case["path"].as_str().expect("path must be a string");
        let method = case["method"].as_str().expect("method must be a string");
        let expected = case["expected_alias"]
            .as_str()
            .expect("expected_alias must be a string");

        let actual = generate_suggested_alias(path, method);
        assert_eq!(
            actual, expected,
            "fixture mismatch for {} {}: got {}, expected {}",
            method, path, actual, expected
        );
    }
}
