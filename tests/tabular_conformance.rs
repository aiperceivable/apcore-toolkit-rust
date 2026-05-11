// Cross-language conformance harness for `format_csv` and `format_jsonl`.
//
// Reads the shared corpus at `apcore-toolkit/conformance/fixtures/` (used by
// Python and TypeScript SDKs as well) and asserts byte-identical output.

use std::path::PathBuf;

use apcore_toolkit::{format_csv, format_jsonl};
use serde_json::{Map, Value};

fn conformance_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("toolkit-rust dir must have a parent")
        .join("apcore-toolkit")
        .join("conformance")
        .join("fixtures")
}

fn load_fixture(name: &str) -> Vec<Value> {
    let path = conformance_dir().join(name);
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let doc: Value = serde_json::from_str(&content).expect("fixture must be valid JSON");
    doc["test_cases"].as_array().cloned().unwrap_or_default()
}

fn rows_from_value(v: &Value) -> Vec<Map<String, Value>> {
    v.as_array()
        .expect("rows must be a JSON array")
        .iter()
        .map(|row| row.as_object().expect("each row must be an object").clone())
        .collect()
}

#[test]
fn format_csv_matches_shared_conformance_fixture() {
    let cases = load_fixture("format_csv.json");
    if cases.is_empty() {
        eprintln!("WARN: conformance fixture not found; skipping");
        return;
    }
    for case in cases {
        let id = case["id"].as_str().unwrap_or("(no-id)");
        let rows = rows_from_value(&case["input"]["rows"]);
        let bom = case["input"]["bom"].as_bool().unwrap_or(false);
        let expected = case["expected"].as_str().expect("expected must be string");
        let actual = format_csv(&rows, bom);
        assert_eq!(
            actual, expected,
            "{}: byte mismatch\nexpected: {:?}\nactual:   {:?}",
            id, expected, actual
        );
    }
}

#[test]
fn format_jsonl_matches_shared_conformance_fixture() {
    let cases = load_fixture("format_jsonl.json");
    if cases.is_empty() {
        eprintln!("WARN: conformance fixture not found; skipping");
        return;
    }
    for case in cases {
        let id = case["id"].as_str().unwrap_or("(no-id)");
        let rows = rows_from_value(&case["input"]["rows"]);
        let expected = case["expected"].as_str().expect("expected must be string");
        let actual = format_jsonl(&rows);
        assert_eq!(
            actual, expected,
            "{}: byte mismatch\nexpected: {:?}\nactual:   {:?}",
            id, expected, actual
        );
    }
}
