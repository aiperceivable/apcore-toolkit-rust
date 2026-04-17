// Scanner trait and shared utilities for framework scanners.
//
// Provides filtering, deduplication, and annotation inference.
// Framework-specific implementations live in separate crates
// (e.g., axum-apcore, actix-apcore).

use apcore::module::ModuleAnnotations;
use async_trait::async_trait;
use regex::Regex;

use crate::types::ScannedModule;

// Re-export the HTTP verb mapping helpers so downstream scanner crates
// can call them through the familiar `scanner` module path, mirroring the
// relationship between `infer_annotations_from_method` and the scanner
// module in the other language SDKs.
pub use crate::http_verb_map::{
    generate_suggested_alias, has_path_params, resolve_http_verb, SCANNER_VERB_MAP,
};

/// Abstract interface for framework scanners.
///
/// Implementors provide `scan()` for framework-specific endpoint scanning
/// and `source_name()` for identification. The `App` type parameter allows
/// each framework adapter to accept its own application type:
///
/// ```ignore
/// // Example: Axum adapter
/// struct AxumScanner;
///
/// #[async_trait]
/// impl Scanner<axum::Router> for AxumScanner {
///     async fn scan(&self, app: &axum::Router) -> Vec<ScannedModule> { /* ... */ }
///     fn source_name(&self) -> &str { "axum" }
/// }
///
/// // Example: Actix adapter
/// struct ActixScanner;
///
/// #[async_trait]
/// impl Scanner<()> for ActixScanner {
///     async fn scan(&self, _app: &()) -> Vec<ScannedModule> { /* ... */ }
///     fn source_name(&self) -> &str { "actix-web" }
/// }
/// ```
#[async_trait]
pub trait Scanner<App: Send + Sync = ()> {
    /// Scan endpoints and return module definitions.
    ///
    /// The `app` parameter receives framework-specific state (e.g., `axum::Router`,
    /// `actix_web::App`). Use `()` if no app context is needed.
    async fn scan(&self, app: &App) -> Vec<ScannedModule>;

    /// Return human-readable scanner name (e.g., "axum", "actix-web").
    fn source_name(&self) -> &str;
}

/// Apply include/exclude regex filters to scanned modules.
///
/// - `include`: If set, only modules whose `module_id` matches are kept.
/// - `exclude`: If set, modules whose `module_id` matches are removed.
///
/// Returns an error if either pattern is not a valid regex.
pub fn filter_modules(
    modules: &[ScannedModule],
    include: Option<&str>,
    exclude: Option<&str>,
) -> Result<Vec<ScannedModule>, regex::Error> {
    let mut result: Vec<ScannedModule> = modules.to_vec();

    if let Some(pattern) = include {
        let re = Regex::new(pattern)?;
        result.retain(|m| re.is_match(&m.module_id));
    }

    if let Some(pattern) = exclude {
        let re = Regex::new(pattern)?;
        result.retain(|m| !re.is_match(&m.module_id));
    }

    Ok(result)
}

/// Resolve duplicate module IDs by appending `_2`, `_3`, etc.
///
/// A warning is appended to the module's warnings list when a rename occurs.
pub fn deduplicate_ids(modules: Vec<ScannedModule>) -> Vec<ScannedModule> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut result: Vec<ScannedModule> = Vec::with_capacity(modules.len());

    for mut module in modules {
        let mid = module.module_id.clone();
        let count = seen.entry(mid.clone()).or_insert(0);
        *count += 1;

        if *count > 1 {
            let new_id = format!("{}_{}", mid, count);
            module.warnings.push(format!(
                "Module ID renamed from '{}' to '{}' to avoid collision",
                mid, new_id
            ));
            module.module_id = new_id;
        }

        result.push(module);
    }

    result
}

/// Infer behavioral annotations from an HTTP method.
///
/// Mapping:
/// - GET    -> readonly=true, cacheable=true
/// - DELETE -> destructive=true
/// - PUT    -> idempotent=true
/// - Others -> default (all false)
pub fn infer_annotations_from_method(method: &str) -> ModuleAnnotations {
    match method.to_uppercase().as_str() {
        "GET" => ModuleAnnotations {
            readonly: true,
            cacheable: true,
            ..Default::default()
        },
        "DELETE" => ModuleAnnotations {
            destructive: true,
            ..Default::default()
        },
        "PUT" => ModuleAnnotations {
            idempotent: true,
            ..Default::default()
        },
        _ => ModuleAnnotations::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_module(id: &str) -> ScannedModule {
        ScannedModule::new(
            id.into(),
            "test".into(),
            json!({}),
            json!({}),
            vec![],
            "app:func".into(),
        )
    }

    #[test]
    fn test_filter_modules_include() {
        let modules = vec![
            make_module("users.get"),
            make_module("users.create"),
            make_module("tasks.list"),
        ];
        let filtered = filter_modules(&modules, Some("users"), None).unwrap();
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|m| m.module_id.starts_with("users")));
    }

    #[test]
    fn test_filter_modules_exclude() {
        let modules = vec![
            make_module("users.get"),
            make_module("users.create"),
            make_module("tasks.list"),
        ];
        let filtered = filter_modules(&modules, None, Some("users")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].module_id, "tasks.list");
    }

    #[test]
    fn test_filter_modules_both() {
        let modules = vec![
            make_module("users.get"),
            make_module("users.admin.create"),
            make_module("tasks.list"),
        ];
        let filtered = filter_modules(&modules, Some("users"), Some("admin")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].module_id, "users.get");
    }

    #[test]
    fn test_deduplicate_ids_no_duplicates() {
        let modules = vec![make_module("a"), make_module("b")];
        let result = deduplicate_ids(modules);
        assert_eq!(result[0].module_id, "a");
        assert_eq!(result[1].module_id, "b");
        assert!(result[0].warnings.is_empty());
    }

    #[test]
    fn test_deduplicate_ids_with_duplicates() {
        let modules = vec![make_module("a"), make_module("a"), make_module("a")];
        let result = deduplicate_ids(modules);
        assert_eq!(result[0].module_id, "a");
        assert_eq!(result[1].module_id, "a_2");
        assert_eq!(result[2].module_id, "a_3");
        assert!(result[1].warnings[0].contains("renamed"));
    }

    #[test]
    fn test_infer_annotations_get() {
        let ann = infer_annotations_from_method("GET");
        assert!(ann.readonly);
        assert!(ann.cacheable);
        assert!(!ann.destructive);
    }

    #[test]
    fn test_infer_annotations_delete() {
        let ann = infer_annotations_from_method("DELETE");
        assert!(ann.destructive);
        assert!(!ann.readonly);
    }

    #[test]
    fn test_infer_annotations_put() {
        let ann = infer_annotations_from_method("PUT");
        assert!(ann.idempotent);
        assert!(!ann.readonly);
    }

    #[test]
    fn test_infer_annotations_post() {
        let ann = infer_annotations_from_method("POST");
        assert!(!ann.readonly);
        assert!(!ann.destructive);
        assert!(!ann.idempotent);
    }

    #[test]
    fn test_infer_annotations_case_insensitive() {
        let ann = infer_annotations_from_method("get");
        assert!(ann.readonly);
    }

    #[test]
    fn test_filter_modules_no_filters() {
        let modules = vec![make_module("users.get"), make_module("tasks.list")];
        let filtered = filter_modules(&modules, None, None).unwrap();
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_filter_modules_include_matches_none() {
        let modules = vec![make_module("users.get"), make_module("tasks.list")];
        let filtered = filter_modules(&modules, Some("^zzz$"), None).unwrap();
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_modules_exclude_matches_all() {
        let modules = vec![make_module("users.get"), make_module("users.create")];
        let filtered = filter_modules(&modules, None, Some("users")).unwrap();
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_modules_invalid_include_regex() {
        let modules = vec![make_module("a")];
        let result = filter_modules(&modules, Some("[invalid"), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_filter_modules_invalid_exclude_regex() {
        let modules = vec![make_module("a")];
        let result = filter_modules(&modules, None, Some("[invalid"));
        assert!(result.is_err());
    }

    #[test]
    fn test_deduplicate_ids_empty_list() {
        let result = deduplicate_ids(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_deduplicate_ids_original_unchanged() {
        let original = vec![make_module("a"), make_module("a")];
        let cloned = original.clone();
        let result = deduplicate_ids(original);

        // The original Vec is consumed by deduplicate_ids (ownership).
        // Verify the clone is independent and unmodified.
        assert_eq!(cloned[0].module_id, "a");
        assert_eq!(cloned[1].module_id, "a");
        assert!(cloned[0].warnings.is_empty());
        assert!(cloned[1].warnings.is_empty());

        // The result has been deduplicated.
        assert_eq!(result[1].module_id, "a_2");
    }

    #[test]
    fn test_deduplicate_ids_mixed() {
        let modules = vec![
            make_module("a"),
            make_module("b"),
            make_module("a"),
            make_module("c"),
            make_module("b"),
        ];
        let result = deduplicate_ids(modules);
        assert_eq!(result[0].module_id, "a");
        assert_eq!(result[1].module_id, "b");
        assert_eq!(result[2].module_id, "a_2");
        assert_eq!(result[3].module_id, "c");
        assert_eq!(result[4].module_id, "b_2");
    }

    #[test]
    fn test_deduplicate_warnings_first_no_warning() {
        let modules = vec![make_module("x"), make_module("x")];
        let result = deduplicate_ids(modules);
        assert!(
            result[0].warnings.is_empty(),
            "First occurrence should have no warning"
        );
        assert!(
            !result[1].warnings.is_empty(),
            "Duplicate should have a warning"
        );
    }

    #[test]
    fn test_deduplicate_warnings_preserved() {
        let mut m = make_module("dup");
        m.warnings.push("existing warning".into());
        let modules = vec![make_module("dup"), m];
        let result = deduplicate_ids(modules);

        // Second module had an existing warning; it should still be there
        // along with the new rename warning.
        assert_eq!(result[1].warnings.len(), 2);
        assert_eq!(result[1].warnings[0], "existing warning");
        assert!(result[1].warnings[1].contains("renamed"));
    }

    #[test]
    fn test_infer_annotations_patch() {
        let ann = infer_annotations_from_method("PATCH");
        assert!(!ann.readonly);
        assert!(!ann.destructive);
        assert!(!ann.idempotent);
        assert!(!ann.cacheable);
    }

    // ---- Integration: re-exported http_verb_map helpers ----

    #[test]
    fn test_reexport_generate_suggested_alias() {
        // Callable through the scanner module path.
        assert_eq!(
            generate_suggested_alias("/tasks/user_data", "POST"),
            "tasks.user_data.create"
        );
    }

    #[test]
    fn test_reexport_resolve_http_verb() {
        assert_eq!(resolve_http_verb("POST", false), "create");
    }

    #[test]
    fn test_reexport_has_path_params() {
        assert!(has_path_params("/tasks/{id}"));
    }

    #[test]
    fn test_reexport_scanner_verb_map() {
        assert_eq!(SCANNER_VERB_MAP.get("POST").copied(), Some("create"));
    }
}
