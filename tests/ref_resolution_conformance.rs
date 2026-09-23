// Conformance harness: assert Rust's `deep_resolve_refs` matches the shared
// fixture corpus at `apcore-toolkit/conformance/fixtures/ref_resolution.json`.
//
// The Python and TypeScript SDKs run the same fixture file through their own
// resolvers and must agree case-for-case. This is the cross-SDK contract for
// `$ref` resolution and sibling-key merging (see
// `apcore-toolkit/docs/features/openapi.md#ref-sibling-keys-are-preserved`).
//
// The sibling-merge half is a security property, not a fidelity nicety: apcore
// reads `x-sensitive` off the *resolved* schema to decide what to redact, and
// this toolkit produces the schemas apcore reads. A marking dropped here is a
// credential logged in plaintext downstream. apcore closed the same hole in
// its own resolver as D-98 in 0.31.0.

use std::path::PathBuf;

use apcore_toolkit::deep_resolve_refs;
use serde_json::Value;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("toolkit-rust dir must have a parent")
        .join("apcore-toolkit")
        .join("conformance")
        .join("fixtures")
}

#[test]
fn ref_resolution_matches_shared_conformance_fixture() {
    let path = fixtures_dir().join("ref_resolution.json");
    if !path.exists() {
        eprintln!(
            "ref_resolution: shared corpus not found at {} — skipping",
            path.display()
        );
        return;
    }

    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture must be readable"))
            .expect("fixture must be valid JSON");
    let cases = doc["test_cases"]
        .as_array()
        .expect("fixture must carry a test_cases array");
    assert!(!cases.is_empty(), "the shared corpus is empty");

    let mut failures = Vec::new();
    for case in cases {
        let id = case["id"].as_str().unwrap_or("<unnamed>");
        let got = deep_resolve_refs(&case["input"]["schema"], &case["input"]["openapi_doc"], 0);
        if got != case["expected"] {
            failures.push(format!(
                "{id}\n     got  {got}\n     want {}\n     ({})",
                case["expected"],
                case["description"].as_str().unwrap_or("")
            ));
        }
    }

    println!(
        "ref_resolution: running {} shared conformance case(s)",
        cases.len()
    );
    assert!(
        failures.is_empty(),
        "{} of {} case(s) diverge from the shared corpus:\n  - {}",
        failures.len(),
        cases.len(),
        failures.join("\n  - ")
    );
}

#[test]
fn deep_resolve_refs_does_not_mutate_its_input() {
    // `deep_resolve_refs` takes `&Value` and is documented pure; the merge must
    // not write back into the caller's schema.
    let schema: Value = serde_json::json!({"$ref": "#/components/schemas/T", "x-sensitive": true});
    let doc: Value = serde_json::json!({"components": {"schemas": {"T": {"type": "string"}}}});
    let before = schema.clone();
    let _ = deep_resolve_refs(&schema, &doc, 0);
    assert_eq!(schema, before);
}
