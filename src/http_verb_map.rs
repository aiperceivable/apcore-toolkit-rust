// HTTP verb semantic mapping utilities.
//
// Provides the canonical mapping from HTTP methods to semantic verbs used
// by scanner implementations when generating user-facing command aliases.
// All functions are pure and infallible.

use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Canonical HTTP method to semantic verb mapping.
///
/// Keys are uppercase HTTP methods. `GET_ID` is a synthetic key used when
/// GET routes have path parameters (single-resource access). Values are
/// lowercase semantic verbs used by CLI and MCP surfaces. The mapping is
/// considered immutable by convention.
pub static SCANNER_VERB_MAP: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("GET", "list");
    m.insert("GET_ID", "get");
    m.insert("POST", "create");
    m.insert("PUT", "update");
    m.insert("PATCH", "patch");
    m.insert("DELETE", "delete");
    m.insert("HEAD", "head");
    m.insert("OPTIONS", "options");
    m
});

/// Regex covering path parameter syntax across major frameworks:
///   FastAPI / Django / OpenAPI: `{param}`
///   Express / NestJS / Gin / Axum: `:param`
static PATH_PARAM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{[^}]+\}|:[a-zA-Z_]\w*").expect("valid regex"));

/// Anchored variant for whole-segment match testing.
static PATH_PARAM_FULL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:\{[^}]+\}|:[a-zA-Z_]\w*)$").expect("valid regex"));

/// Check if a URL path contains path parameter placeholders.
///
/// Detects both brace-style (`{param}`) and colon-style (`:param`) parameters,
/// covering all major web frameworks.
///
/// # Examples
///
/// ```
/// use apcore_toolkit::http_verb_map::has_path_params;
///
/// assert_eq!(has_path_params("/tasks"), false);
/// assert_eq!(has_path_params("/tasks/{id}"), true);
/// assert_eq!(has_path_params("/users/:userId"), true);
/// ```
pub fn has_path_params(path: &str) -> bool {
    PATH_PARAM_RE.is_match(path)
}

/// Map an HTTP method to its semantic verb.
///
/// GET is contextual: collection routes (no path params) map to `"list"`,
/// single-resource routes (with path params) map to `"get"`. All other
/// methods have a static mapping. Unknown methods fall through to
/// the lowercase form of the input.
///
/// # Arguments
///
/// * `method` - HTTP method string (case-insensitive).
/// * `path_has_params` - True if the corresponding route has path parameters.
///
/// # Examples
///
/// ```
/// use apcore_toolkit::http_verb_map::resolve_http_verb;
///
/// assert_eq!(resolve_http_verb("POST", false), "create");
/// assert_eq!(resolve_http_verb("GET", false), "list");
/// assert_eq!(resolve_http_verb("GET", true), "get");
/// ```
pub fn resolve_http_verb(method: &str, path_has_params: bool) -> String {
    let method_upper = method.to_uppercase();
    if method_upper == "GET" {
        let key = if path_has_params { "GET_ID" } else { "GET" };
        return SCANNER_VERB_MAP.get(key).copied().unwrap_or("").to_string();
    }
    SCANNER_VERB_MAP
        .get(method_upper.as_str())
        .copied()
        .map(|s| s.to_string())
        .unwrap_or_else(|| method.to_lowercase())
}

/// Generate a dot-separated suggested alias from HTTP route info.
///
/// The alias is built from non-parameter path segments joined with the
/// resolved semantic verb. The output uses snake_case preserved from the
/// path; surface adapters apply their own naming conventions (e.g., CLI
/// converts underscores to hyphens).
///
/// The GET-vs-list disambiguation checks whether the LAST path segment
/// is a path parameter (single-resource access) rather than whether the
/// path contains any parameters anywhere. This correctly treats nested
/// collection endpoints like `/orgs/{org_id}/members` as `"list"`.
///
/// # Examples
///
/// ```
/// use apcore_toolkit::http_verb_map::generate_suggested_alias;
///
/// assert_eq!(
///     generate_suggested_alias("/tasks/user_data", "POST"),
///     "tasks.user_data.create"
/// );
/// assert_eq!(
///     generate_suggested_alias("/tasks/user_data", "GET"),
///     "tasks.user_data.list"
/// );
/// assert_eq!(
///     generate_suggested_alias("/tasks/user_data/{id}", "GET"),
///     "tasks.user_data.get"
/// );
/// assert_eq!(
///     generate_suggested_alias("/orgs/{org_id}/members", "GET"),
///     "orgs.members.list"
/// );
/// ```
pub fn generate_suggested_alias(path: &str, method: &str) -> String {
    let trimmed = path.trim_matches('/');
    let raw_segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    let segments: Vec<&str> = raw_segments
        .iter()
        .copied()
        .filter(|s| !PATH_PARAM_FULL_RE.is_match(s))
        .collect();
    let is_single_resource = raw_segments
        .last()
        .map(|s| PATH_PARAM_FULL_RE.is_match(s))
        .unwrap_or(false);
    let verb = resolve_http_verb(method, is_single_resource);
    let mut parts: Vec<String> = segments.iter().map(|s| s.to_string()).collect();
    parts.push(verb);
    parts.join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- has_path_params ----

    #[test]
    fn test_has_path_params_empty_string() {
        assert!(!has_path_params(""));
    }

    #[test]
    fn test_has_path_params_root_path() {
        assert!(!has_path_params("/"));
    }

    #[test]
    fn test_has_path_params_static_path() {
        assert!(!has_path_params("/tasks"));
    }

    #[test]
    fn test_has_path_params_brace_style() {
        assert!(has_path_params("/tasks/{id}"));
    }

    #[test]
    fn test_has_path_params_colon_style() {
        assert!(has_path_params("/tasks/:id"));
    }

    #[test]
    fn test_has_path_params_mixed_styles() {
        assert!(has_path_params("/{id}/:name"));
    }

    #[test]
    fn test_has_path_params_multi_segment_static() {
        assert!(!has_path_params("/a/b/c"));
    }

    #[test]
    fn test_has_path_params_empty_brace() {
        assert!(!has_path_params("/tasks/{}"));
    }

    // ---- resolve_http_verb ----

    #[test]
    fn test_resolve_http_verb_get_collection() {
        assert_eq!(resolve_http_verb("GET", false), "list");
    }

    #[test]
    fn test_resolve_http_verb_get_single() {
        assert_eq!(resolve_http_verb("GET", true), "get");
    }

    #[test]
    fn test_resolve_http_verb_get_case_insensitive() {
        assert_eq!(resolve_http_verb("get", false), "list");
    }

    #[test]
    fn test_resolve_http_verb_post_no_params() {
        assert_eq!(resolve_http_verb("POST", false), "create");
    }

    #[test]
    fn test_resolve_http_verb_post_with_params() {
        assert_eq!(resolve_http_verb("POST", true), "create");
    }

    #[test]
    fn test_resolve_http_verb_put() {
        assert_eq!(resolve_http_verb("PUT", true), "update");
    }

    #[test]
    fn test_resolve_http_verb_patch() {
        assert_eq!(resolve_http_verb("PATCH", true), "patch");
    }

    #[test]
    fn test_resolve_http_verb_delete() {
        assert_eq!(resolve_http_verb("DELETE", true), "delete");
    }

    #[test]
    fn test_resolve_http_verb_head() {
        assert_eq!(resolve_http_verb("HEAD", false), "head");
    }

    #[test]
    fn test_resolve_http_verb_options() {
        assert_eq!(resolve_http_verb("OPTIONS", false), "options");
    }

    #[test]
    fn test_resolve_http_verb_unknown_method() {
        assert_eq!(resolve_http_verb("PURGE", false), "purge");
    }

    #[test]
    fn test_resolve_http_verb_empty_method() {
        assert_eq!(resolve_http_verb("", false), "");
    }

    // ---- generate_suggested_alias ----

    #[test]
    fn test_generate_alias_post_collection() {
        assert_eq!(
            generate_suggested_alias("/tasks/user_data", "POST"),
            "tasks.user_data.create"
        );
    }

    #[test]
    fn test_generate_alias_get_collection() {
        assert_eq!(
            generate_suggested_alias("/tasks/user_data", "GET"),
            "tasks.user_data.list"
        );
    }

    #[test]
    fn test_generate_alias_get_single() {
        assert_eq!(
            generate_suggested_alias("/tasks/user_data/{id}", "GET"),
            "tasks.user_data.get"
        );
    }

    #[test]
    fn test_generate_alias_put_single() {
        assert_eq!(
            generate_suggested_alias("/tasks/user_data/{id}", "PUT"),
            "tasks.user_data.update"
        );
    }

    #[test]
    fn test_generate_alias_patch_single() {
        assert_eq!(
            generate_suggested_alias("/tasks/user_data/{id}", "PATCH"),
            "tasks.user_data.patch"
        );
    }

    #[test]
    fn test_generate_alias_delete_single() {
        assert_eq!(
            generate_suggested_alias("/tasks/user_data/{id}", "DELETE"),
            "tasks.user_data.delete"
        );
    }

    #[test]
    fn test_generate_alias_single_segment() {
        assert_eq!(generate_suggested_alias("/health", "GET"), "health.list");
    }

    #[test]
    fn test_generate_alias_root_path() {
        assert_eq!(generate_suggested_alias("/", "GET"), "list");
    }

    #[test]
    fn test_generate_alias_empty_path() {
        assert_eq!(generate_suggested_alias("", "GET"), "list");
    }

    #[test]
    fn test_generate_alias_colon_param() {
        assert_eq!(
            generate_suggested_alias("/users/:user_id", "GET"),
            "users.get"
        );
    }

    #[test]
    fn test_generate_alias_version_prefix() {
        assert_eq!(
            generate_suggested_alias("/api/v2/users", "GET"),
            "api.v2.users.list"
        );
    }

    #[test]
    fn test_generate_alias_nested_params_collection() {
        assert_eq!(
            generate_suggested_alias("/orgs/{org_id}/teams/{team_id}/members", "GET"),
            "orgs.teams.members.list"
        );
    }

    #[test]
    fn test_generate_alias_double_slashes() {
        assert_eq!(
            generate_suggested_alias("//tasks//user_data//", "POST"),
            "tasks.user_data.create"
        );
    }

    #[test]
    fn test_generate_alias_param_only_path() {
        assert_eq!(generate_suggested_alias("/{id}", "GET"), "get");
    }

    // ---- SCANNER_VERB_MAP ----

    #[test]
    fn test_scanner_verb_map_contains_standard_methods() {
        for k in &[
            "GET", "GET_ID", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS",
        ] {
            assert!(SCANNER_VERB_MAP.contains_key(k), "missing key: {}", k);
        }
    }

    #[test]
    fn test_scanner_verb_map_values_lowercase() {
        for v in SCANNER_VERB_MAP.values() {
            assert_eq!(*v, &*v.to_lowercase());
        }
    }

    // ---- Conformance fixture ----

    #[test]
    fn test_conformance_fixture() {
        let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("scanner_verb_map.json");

        let content = std::fs::read_to_string(&fixture_path)
            .unwrap_or_else(|e| panic!("failed to read fixture at {:?}: {}", fixture_path, e));

        let cases: serde_json::Value =
            serde_json::from_str(&content).expect("fixture must be valid JSON");

        let array = cases.as_array().expect("fixture must be a JSON array");
        assert!(!array.is_empty(), "fixture must contain at least one case");

        for case in array {
            let path = case["path"].as_str().unwrap();
            let method = case["method"].as_str().unwrap();
            let expected = case["expected_alias"].as_str().unwrap();

            let result = generate_suggested_alias(path, method);
            assert_eq!(
                result, expected,
                "fixture mismatch for {} {}: got {}, expected {}",
                method, path, result, expected
            );
        }
    }
}
