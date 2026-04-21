// Black-box integration tests that exercise apcore-toolkit's public API
// from outside the crate. Complements the inline unit tests in `src/` by
// proving the crate-root re-exports are callable without relying on
// `pub(crate)` paths. Mirrors the per-module test files present in the
// Python and TypeScript SDKs.

use std::collections::{HashMap, HashSet};

use apcore_toolkit::{
    extract_path_param_names, generate_suggested_alias, get_writer, has_path_params,
    resolve_http_verb, substitute_path_params, OutputFormat, OutputFormatError, SCANNER_VERB_MAP,
    VERSION,
};

#[test]
fn crate_exposes_version_constant() {
    // VERSION must be non-empty and match the Cargo.toml version.
    assert!(!VERSION.is_empty());
    assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
}

#[test]
fn http_verb_helpers_are_reachable_from_crate_root() {
    assert!(has_path_params("/tasks/{id}"));
    assert!(!has_path_params("/tasks"));
    assert_eq!(resolve_http_verb("POST", false), "create");
    assert_eq!(
        generate_suggested_alias("/tasks/user_data", "POST"),
        "tasks.user_data.create"
    );
    assert_eq!(SCANNER_VERB_MAP.get("DELETE").copied(), Some("delete"));
}

#[test]
fn extract_path_param_names_round_trip_with_substitute() {
    let path = "/orgs/{org_id}/members/:member_id";
    let names: HashSet<String> = extract_path_param_names(path);
    assert!(names.contains("org_id"));
    assert!(names.contains("member_id"));

    let mut values: HashMap<&str, String> = HashMap::new();
    values.insert("org_id", "7".to_string());
    values.insert("member_id", "42".to_string());
    assert_eq!(substitute_path_params(path, &values), "/orgs/7/members/42");
}

#[test]
fn get_writer_returns_expected_variants_for_known_formats() {
    assert_eq!(get_writer("yaml").unwrap(), OutputFormat::Yaml);
    assert_eq!(get_writer("registry").unwrap(), OutputFormat::Registry);
    assert_eq!(get_writer("YAML").unwrap(), OutputFormat::Yaml);
}

#[test]
fn get_writer_returns_error_for_unknown_format() {
    let err = get_writer("xml").unwrap_err();
    assert!(matches!(err, OutputFormatError::Unknown(_)));
    assert!(err.to_string().contains("xml"));
}
