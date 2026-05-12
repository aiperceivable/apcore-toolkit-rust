// Cross-SDK conformance harness for DisplayResolver.
//
// Reads the shared corpus at `apcore-toolkit/conformance/fixtures/display_resolve.json`
// (used by Python and TypeScript SDKs as well) and asserts the Rust
// DisplayResolver produces matching resolved display output for every case.

use std::collections::HashMap;
use std::path::PathBuf;

use apcore_toolkit::display::DisplayResolver;
use apcore_toolkit::ScannedModule;
use serde_json::{json, Map, Value};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("toolkit-rust dir must have a parent")
        .join("apcore-toolkit")
        .join("conformance")
        .join("fixtures")
        .join("display_resolve.json")
}

fn load_cases() -> Vec<Value> {
    let path = fixture_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let doc: Value = serde_json::from_str(&content).expect("fixture must be valid JSON");
    doc["test_cases"].as_array().cloned().unwrap_or_default()
}

fn build_module(raw: &Value) -> ScannedModule {
    let module_id = raw["module_id"].as_str().unwrap_or("").to_string();
    let description = raw["description"].as_str().unwrap_or("").to_string();
    let tags: Vec<String> = raw["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let metadata: HashMap<String, Value> = raw["metadata"]
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let mut module = ScannedModule::new(
        module_id,
        description,
        raw["input_schema"].clone(),
        raw["output_schema"].clone(),
        tags,
        raw["target"].as_str().unwrap_or("fixture:noop").to_string(),
    );
    module.metadata = metadata;
    if let Some(doc) = raw["documentation"].as_str() {
        module.documentation = Some(doc.to_string());
    }
    module
}

fn binding_map(raw: &Value) -> Value {
    raw.clone()
}

/// Assert every key in `expected` is present in `actual` with an equal
/// value. Permits `actual` to carry additional keys.
fn assert_partial_match(expected: &Map<String, Value>, actual: &Map<String, Value>, path: &str) {
    for (key, exp_val) in expected.iter() {
        let act_val = actual
            .get(key)
            .unwrap_or_else(|| panic!("missing key {path}.{key} in resolved display"));
        match (exp_val, act_val) {
            (Value::Object(e), Value::Object(a)) => {
                assert_partial_match(e, a, &format!("{path}.{key}"));
            }
            _ => {
                assert_eq!(
                    exp_val, act_val,
                    "{path}.{key}: expected {exp_val} got {act_val}"
                );
            }
        }
    }
}

#[test]
fn display_resolver_matches_shared_conformance_fixture() {
    let cases = load_cases();
    if cases.is_empty() {
        eprintln!("WARN: display_resolve.json fixture not found; skipping");
        return;
    }
    for case in &cases {
        let id = case["id"].as_str().unwrap_or("(no-id)");
        let inp = &case["input"];
        let exp = &case["expected"];
        let binding = binding_map(&inp["binding_map"]);

        let resolver = DisplayResolver::new();

        // Error case (e.g. MCP alias exceeds 64-char hard limit)
        if exp.get("error").is_some() {
            let raw_mod = &inp["scanned_module"];
            let module = build_module(raw_mod);
            let result = resolver.resolve(vec![module], None, Some(&binding));
            assert!(result.is_err(), "{id}: expected error variant");
            continue;
        }

        // Multi-module case
        if inp.get("scanned_modules").is_some() {
            let modules: Vec<ScannedModule> = inp["scanned_modules"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .map(build_module)
                .collect();
            let resolved = resolver
                .resolve(modules, None, Some(&binding))
                .unwrap_or_else(|e| panic!("{id}: resolve failed: {e:?}"));
            for exp_result in exp["results"].as_array().unwrap_or(&Vec::new()) {
                let want_id = exp_result["module_id"].as_str().unwrap_or("");
                let mod_idx = resolved
                    .iter()
                    .position(|m| m.module_id == want_id)
                    .unwrap_or_else(|| panic!("{id}: missing module {want_id}"));
                let actual_display = resolved[mod_idx]
                    .metadata
                    .get("display")
                    .and_then(Value::as_object)
                    .unwrap_or_else(|| panic!("{id}: module {want_id} missing display"));
                let expected_display = exp_result["display"]
                    .as_object()
                    .expect("expected.results[].display must be object");
                assert_partial_match(expected_display, actual_display, "display");
            }
            continue;
        }

        // Single-module display-equality case (cases 1-9, 11-12, 14)
        let module = build_module(&inp["scanned_module"]);
        let resolved = resolver
            .resolve(vec![module], None, Some(&binding))
            .unwrap_or_else(|e| panic!("{id}: resolve failed: {e:?}"));
        let actual_display = resolved[0]
            .metadata
            .get("display")
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("{id}: resolved module missing display"));
        let expected_display = exp["display"]
            .as_object()
            .unwrap_or_else(|| panic!("{id}: expected.display must be object"));
        assert_partial_match(expected_display, actual_display, "display");

        // Silence unused-variable warning when nothing branches above
        let _ = json!({});
    }
}
