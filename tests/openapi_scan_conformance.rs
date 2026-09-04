// Conformance harness: assert Rust's OpenAPIScanner implementation matches
// the shared fixture corpus at
// `apcore-toolkit/conformance/fixtures/openapi_scan.json`.
//
// The Python and TypeScript SDKs run the same fixture file through their
// own `OpenAPIScanner` and assert structurally identical (parsed-JSON deep
// equality) module lists — unlike `view_model.json`, `expected` here is a
// **structured object**, not a canonical string, since `ScannedModule`
// output is compared field-by-field rather than byte-for-byte. See
// `apcore-toolkit/docs/features/openapi-scanner.md`.
//
// Fixture cases `openapi_scan_021` through `openapi_scan_023` install a
// named test-only hook from `install_hook` below — the fixture's
// `input.hooks` key names which one, so all three SDKs install
// byte-identical hook behavior without serializing a closure through JSON.

use std::path::PathBuf;

use apcore_toolkit::{OpenAPIScanner, ScanOptions, ScannedModule, ScannerError};
use serde_json::{json, Value};

fn conformance_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("toolkit-rust dir must have a parent")
        .join("apcore-toolkit")
        .join("conformance")
        .join("fixtures")
}

fn load_cases() -> Vec<Value> {
    let path = conformance_dir().join("openapi_scan.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("WARN: conformance fixture not found at {path:?}; skipping");
            return Vec::new();
        }
    };
    let doc: Value = serde_json::from_str(&content).expect("fixture must be valid JSON");
    doc["test_cases"].as_array().cloned().unwrap_or_default()
}

/// Install the one named hook this fixture case requires (`hook_key` is
/// `"transform_operation"` or `"derive_module_id"`; `hook_name` is one of
/// the three fixed names below). Mirrors
/// `apcore-toolkit-python/tests/test_openapi_scan_conformance.py`'s
/// `_HOOKS` table exactly.
fn install_hook(options: &mut ScanOptions, hook_key: &str, hook_name: &str) {
    match (hook_key, hook_name) {
        ("transform_operation", "skip_if_x_skip_true") => {
            options.transform_operation = Some(Box::new(|_path, _method, operation| {
                if operation
                    .get("x-skip")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    None
                } else {
                    Some(operation.clone())
                }
            }));
        }
        ("derive_module_id", "custom_name_for_operation_id_custom_else_default") => {
            options.derive_module_id = Some(Box::new(|_path, _method, operation| {
                if operation.get("operationId").and_then(Value::as_str) == Some("custom") {
                    Some("custom.name".to_string())
                } else {
                    None
                }
            }));
        }
        ("derive_module_id", "always_returns_dup_op") => {
            options.derive_module_id = Some(Box::new(|_path, _method, _operation| {
                Some("dup.op".to_string())
            }));
        }
        _ => panic!("unknown test hook: {hook_key} -> {hook_name}"),
    }
}

/// Build the same structural JSON representation the Python harness's
/// `_module_repr` builds: only truthy `ModuleAnnotations` flags (from the
/// fixed set the fixtures exercise) plus `extra` when non-empty, rather
/// than the crate's own full `Serialize` impl (which always emits every
/// field, including protocol defaults like `open_world: true` that the
/// fixtures never mention).
fn module_repr(m: &ScannedModule) -> Value {
    let mut annotations = serde_json::Map::new();
    if let Some(ann) = &m.annotations {
        for (flag, value) in [
            ("readonly", ann.readonly),
            ("destructive", ann.destructive),
            ("idempotent", ann.idempotent),
            ("cacheable", ann.cacheable),
        ] {
            if value {
                annotations.insert(flag.to_string(), Value::Bool(true));
            }
        }
        if !ann.extra.is_empty() {
            annotations.insert(
                "extra".to_string(),
                Value::Object(ann.extra.clone().into_iter().collect()),
            );
        }
    }

    json!({
        "module_id": m.module_id,
        "description": m.description,
        "documentation": m.documentation,
        "tags": m.tags,
        "version": m.version,
        "target": m.target,
        "annotations": Value::Object(annotations),
        "metadata": m.metadata,
        "input_schema": m.input_schema,
        "output_schema": m.output_schema,
        "warnings": m.warnings,
    })
}

#[test]
fn openapi_scan_matches_shared_conformance_fixture() {
    let cases = load_cases();
    if cases.is_empty() {
        return;
    }

    let mut failures = Vec::new();

    for case in &cases {
        let id = case["id"].as_str().unwrap_or("(no-id)");
        let description = case["description"].as_str().unwrap_or("");
        let input = &case["input"];
        let expected = &case["expected"];
        let spec = &input["spec"];

        let mut options = ScanOptions::new();
        let raw_options = &input["options"];
        if let Some(include) = raw_options.get("include").and_then(Value::as_str) {
            options.include = Some(include.to_string());
        }
        if let Some(exclude) = raw_options.get("exclude").and_then(Value::as_str) {
            options.exclude = Some(exclude.to_string());
        }
        if let Some(prefix) = raw_options.get("base_path_prefix").and_then(Value::as_str) {
            options.base_path_prefix = Some(prefix.to_string());
        }
        if let Some(include_deprecated) = raw_options
            .get("include_deprecated")
            .and_then(Value::as_bool)
        {
            options.include_deprecated = include_deprecated;
        }
        if let Some(hooks) = input.get("hooks").and_then(Value::as_object) {
            for (hook_key, hook_name) in hooks {
                install_hook(
                    &mut options,
                    hook_key,
                    hook_name.as_str().unwrap_or_default(),
                );
            }
        }

        let scanner = OpenAPIScanner::new();
        let result = futures::executor::block_on(scanner.scan(spec, &options));

        if expected.get("raises").is_some() {
            match result {
                Err(ScannerError::InvalidSpec(_)) => {}
                other => failures.push(format!(
                    "\nCase {id}: {description}\nExpected: Err(ScannerError::InvalidSpec(_))\nActual:   {other:?}"
                )),
            }
            continue;
        }

        let modules = match result {
            Ok(m) => m,
            Err(e) => {
                failures.push(format!("\nCase {id}: {description}\nUnexpected error: {e}"));
                continue;
            }
        };

        let actual: Vec<Value> = modules.iter().map(module_repr).collect();
        let expected_modules = expected["modules"].as_array().cloned().unwrap_or_default();

        if actual != expected_modules {
            failures.push(format!(
                "\nCase {id}: {description}\nExpected: {}\nActual:   {}",
                serde_json::to_string_pretty(&expected_modules).unwrap_or_default(),
                serde_json::to_string_pretty(&actual).unwrap_or_default(),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} case(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
