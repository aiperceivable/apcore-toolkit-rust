//! Shared binding-structure cases exercise both public loader entry points.

use apcore_toolkit::BindingLoader;
use serde_json::Value;

#[test]
fn binding_structure_conformance() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("apcore-toolkit/conformance/fixtures/binding_structure.json");
    let document: Value = serde_json::from_str(&std::fs::read_to_string(fixture).unwrap()).unwrap();
    let cases = document["test_cases"].as_array().unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        for strict in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let file = directory.path().join("case.binding.yaml");
            std::fs::write(&file, serde_yaml_ng::to_string(&case["data"]).unwrap()).unwrap();
            let loader = BindingLoader::new();
            let error = case[if strict {
                "strict_error"
            } else {
                "loose_error"
            }]
            .as_str();
            for result in [
                loader.load_data(&case["data"], strict),
                loader.load(&file, strict, false),
            ] {
                if let Some(expected) = error {
                    let actual = result.expect_err(&format!("{} strict={strict}", case["id"]));
                    assert!(
                        actual.to_string().contains(expected),
                        "{} strict={strict}: {actual}",
                        case["id"]
                    );
                } else {
                    let modules = result
                        .unwrap_or_else(|error| panic!("{} strict={strict}: {error}", case["id"]));
                    assert_eq!(
                        modules.len(),
                        case["data"]["bindings"].as_array().unwrap().len()
                    );
                    if let Some(fields) = case["fields"].as_object() {
                        let actual = serde_json::to_value(&modules[0]).unwrap();
                        for (field, expected) in fields {
                            assert_eq!(
                                &actual[field], expected,
                                "{} strict={strict}: {field}",
                                case["id"]
                            );
                        }
                    }
                }
            }
        }
    }
}
