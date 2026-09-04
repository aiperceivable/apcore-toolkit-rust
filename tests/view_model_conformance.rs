// Conformance harness: assert Rust's TuiViewModel implementation matches
// the shared fixture corpus at
// `apcore-toolkit/conformance/fixtures/view_model.json`.
//
// The Python and TypeScript SDKs run the same fixture file through their
// own `modules_to_view_model` / `format_view_model` and assert
// byte-identical JSON output for `expected`. This is the cross-SDK
// byte-identity contract for the TUI View Model proposal (see
// `apcore-toolkit/docs/features/tui-view-model.md`). `expected` here is a
// **string** — the exact canonical compact JSON — compared byte-for-byte,
// unlike `openapi_scan.json`'s structured `expected.modules`.

use std::path::PathBuf;

use apcore::module::ModuleAnnotations;
use apcore_toolkit::{
    format_view_model, modules_to_view_model, Direction, Exposure, Filter,
    ModulesToViewModelOptions, ScannedModule, Sort, TonePalette, ToneRule, View, ViewGroupBy,
};
use serde_json::Value;

fn conformance_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("toolkit-rust dir must have a parent")
        .join("apcore-toolkit")
        .join("conformance")
        .join("fixtures")
}

fn load_cases() -> Vec<Value> {
    let path = conformance_dir().join("view_model.json");
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

fn build_module(raw: &Value) -> ScannedModule {
    let mut module = ScannedModule::new(
        raw["module_id"].as_str().unwrap_or_default().to_string(),
        raw.get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Value::Object(Default::default()),
        Value::Object(Default::default()),
        raw.get("tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        "fixture:noop".to_string(),
    );

    if let Some(raw_ann) = raw.get("annotations") {
        let extra = raw_ann
            .get("extra")
            .and_then(Value::as_object)
            .cloned()
            .map(|m| m.into_iter().collect())
            .unwrap_or_default();
        module.annotations = Some(ModuleAnnotations {
            discoverable: raw_ann
                .get("discoverable")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            extra,
            ..Default::default()
        });
    }

    if let Some(display) = raw.get("display") {
        if !display.is_null() {
            module.display = Some(display.clone());
        }
    }

    module
}

fn build_view(raw: Option<&str>) -> View {
    match raw {
        Some("grouped") => View::Grouped,
        _ => View::List,
    }
}

fn build_filter(raw: Option<&Value>) -> Option<Filter> {
    let raw = raw?;
    Some(Filter {
        tags: raw
            .get("tags")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        search: raw
            .get("search")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        annotations: raw
            .get("annotations")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        exposure: match raw.get("exposure").and_then(Value::as_str) {
            Some("exposed") => Exposure::Exposed,
            Some("hidden") => Exposure::Hidden,
            _ => Exposure::All,
        },
        deprecated: raw
            .get("deprecated")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

fn build_sort(raw: Option<&Value>) -> Option<Sort> {
    let raw = raw?;
    Some(Sort {
        key: raw["key"].as_str().unwrap_or_default().to_string(),
        direction: match raw.get("direction").and_then(Value::as_str) {
            Some("desc") => Direction::Desc,
            _ => Direction::Asc,
        },
    })
}

fn build_group_by(raw: Option<&str>) -> Option<ViewGroupBy> {
    match raw {
        Some("tag") => Some(ViewGroupBy::Tag),
        Some("prefix") => Some(ViewGroupBy::Prefix),
        _ => None,
    }
}

fn build_tone_palettes(raw: Option<&Value>) -> Vec<TonePalette> {
    let Some(arr) = raw.and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .map(|p| {
            let rules = p
                .get("rules")
                .and_then(Value::as_array)
                .map(|rules| {
                    rules
                        .iter()
                        .map(|r| ToneRule {
                            value: r["value"].as_str().unwrap_or_default().to_string(),
                            tone: match r["tone"].as_str() {
                                Some("positive") => apcore_toolkit::Tone::Positive,
                                Some("negative") => apcore_toolkit::Tone::Negative,
                                Some("warning") => apcore_toolkit::Tone::Warning,
                                Some("info") => apcore_toolkit::Tone::Info,
                                _ => apcore_toolkit::Tone::Neutral,
                            },
                        })
                        .collect()
                })
                .unwrap_or_default();
            TonePalette {
                name: p["name"].as_str().unwrap_or_default().to_string(),
                rules,
            }
        })
        .collect()
}

#[test]
fn view_model_matches_shared_conformance_fixture() {
    let cases = load_cases();
    if cases.is_empty() {
        return;
    }

    let mut failures = Vec::new();

    for case in &cases {
        let id = case["id"].as_str().unwrap_or("(no-id)");
        let description = case["description"].as_str().unwrap_or("");
        let input = &case["input"];
        let options_raw = &input["options"];

        let modules: Vec<ScannedModule> = input["modules"]
            .as_array()
            .map(|arr| arr.iter().map(build_module).collect())
            .unwrap_or_default();

        let options = ModulesToViewModelOptions {
            view: build_view(options_raw.get("view").and_then(Value::as_str)),
            columns: options_raw
                .get("columns")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            title: options_raw
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            filter: build_filter(options_raw.get("filter")),
            sort: build_sort(options_raw.get("sort")),
            group_by: build_group_by(options_raw.get("group_by").and_then(Value::as_str)),
            tone_palettes: build_tone_palettes(options_raw.get("tone_palettes")),
            display: options_raw
                .get("display")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        };

        let vm = modules_to_view_model(&modules, &options);
        let actual = format_view_model(&vm);
        let expected = case["expected"]
            .as_str()
            .expect("expected must be a string");

        if actual != expected {
            failures.push(format!(
                "\nCase {id}: {description}\nExpected: {expected}\nActual:   {actual}"
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
