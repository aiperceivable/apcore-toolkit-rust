// Scanner trait and shared utilities for framework scanners.
//
// Provides filtering, deduplication, and annotation inference.
// Framework-specific implementations live in separate crates
// (e.g., axum-apcore, actix-apcore).

use apcore::module::ModuleAnnotations;
use async_trait::async_trait;
use regex::Regex;

use crate::types::ScannedModule;

/// Abstract interface for framework scanners.
///
/// Implementors provide `scan()` for framework-specific endpoint scanning
/// and `source_name()` for identification. The `App` type parameter allows
/// each framework adapter to accept its own application type.
///
/// # Language-specific API shape
///
/// The Rust `BaseScanner` trait intentionally contains only the primitive `scan()` and
/// `source_name()` operations, keeping it object-safe (usable as `Box<dyn BaseScanner>`).
///
/// Helper utilities — [`filter_modules`], [`deduplicate_ids`], [`infer_annotations_from_method`] —
/// are free functions in the `scanner` module rather than trait default methods.
/// This differs from Python and TypeScript where these are instance methods on the class.
///
/// Usage:
/// ```ignore
/// let filtered = scanner::filter_modules(&modules, Some("my.*"), None)?;
/// let deduplicated = scanner::deduplicate_ids(filtered);
/// ```
///
/// # Note: ConventionScanner
///
/// `ConventionScanner` (available in Python as `apcore_toolkit.ConventionScanner`) is
/// **Python-only**. It relies on Python's `importlib` module introspection for plain-function
/// discovery, which has no equivalent in Rust. Rust consumers should use `BaseScanner`
/// implementations that work with Rust's type system directly.
///
/// ```ignore
/// // Example: Axum adapter
/// struct AxumScanner;
///
/// #[async_trait]
/// impl BaseScanner<axum::Router> for AxumScanner {
///     async fn scan(&self, app: &axum::Router) -> Vec<ScannedModule> { /* ... */ }
///     fn source_name(&self) -> &str { "axum" }
/// }
///
/// // Example: Actix adapter
/// struct ActixScanner;
///
/// #[async_trait]
/// impl BaseScanner<()> for ActixScanner {
///     async fn scan(&self, _app: &()) -> Vec<ScannedModule> { /* ... */ }
///     fn source_name(&self) -> &str { "actix-web" }
/// }
/// ```
#[async_trait]
pub trait BaseScanner<App: Send + Sync = ()> {
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
///
/// # Rust API note
///
/// In Python and TypeScript, `filter_modules` / `filterModules` is an *instance method*
/// on `BaseScanner`. In Rust it is a free function because the `BaseScanner` trait must
/// remain object-safe — adding `Self`-independent helpers as default methods would prevent
/// trait object usage. Call this function directly with your module slice:
///
/// ```ignore
/// let filtered = scanner::filter_modules(&modules, Some("my_app.*"), None)?;
/// ```
///
/// # Errors
///
/// Returns `Err(regex::Error)` if `include` or `exclude` contain invalid regex patterns.
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
    // Pre-scan to build the full set of original IDs, so that generated suffixed
    // names skip any ID that already exists in the input (prevents forward collisions
    // like `[a, a, a_2]` producing two `a_2` entries).
    let original_ids: std::collections::HashSet<String> =
        modules.iter().map(|m| m.module_id.clone()).collect();
    let mut occurrence_count: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut assigned: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result: Vec<ScannedModule> = Vec::with_capacity(modules.len());

    for mut module in modules {
        let mid = module.module_id.clone();
        let count = occurrence_count.entry(mid.clone()).or_insert(0);
        *count += 1;

        if *count == 1 {
            assigned.insert(mid.clone());
        } else {
            // Find the smallest suffix that doesn't collide with any original or
            // already-assigned ID.
            let mut suffix = *count;
            let mut new_id = format!("{}_{}", mid, suffix);
            while assigned.contains(&new_id) || original_ids.contains(&new_id) {
                suffix += 1;
                new_id = format!("{}_{}", mid, suffix);
            }
            assigned.insert(new_id.clone());
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
/// - GET     -> readonly=true, cacheable=true
/// - HEAD    -> readonly=true (inspection-only, no body)
/// - OPTIONS -> readonly=true (metadata query, no mutation)
/// - DELETE  -> destructive=true
/// - PUT     -> idempotent=true
/// - POST    -> default (all false; creates resources, not idempotent by spec)
/// - PATCH   -> default (partial update, not standardly idempotent)
pub fn infer_annotations_from_method(method: &str) -> ModuleAnnotations {
    match method.to_uppercase().as_str() {
        "GET" => ModuleAnnotations {
            readonly: true,
            cacheable: true,
            ..Default::default()
        },
        "HEAD" | "OPTIONS" => ModuleAnnotations {
            readonly: true,
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

    #[test]
    fn test_infer_annotations_head() {
        let ann = infer_annotations_from_method("HEAD");
        assert!(ann.readonly, "HEAD should be readonly");
        assert!(!ann.cacheable, "HEAD should not be cacheable (no body)");
        assert!(!ann.destructive);
        assert!(!ann.idempotent);
    }

    #[test]
    fn test_infer_annotations_options() {
        let ann = infer_annotations_from_method("OPTIONS");
        assert!(ann.readonly, "OPTIONS should be readonly");
        assert!(!ann.destructive);
        assert!(!ann.idempotent);
    }

    #[test]
    fn test_infer_annotations_head_case_insensitive() {
        let ann = infer_annotations_from_method("head");
        assert!(ann.readonly);
    }

    #[test]
    fn test_deduplicate_ids_no_collision_with_preexisting_suffixed_id() {
        // [a, a, a_2] — the second 'a' must not collide with the pre-existing 'a_2'.
        // Pre-scan sees {"a", "a_2"}, so the second 'a' skips 'a_2' and picks 'a_3'.
        let modules = vec![make_module("a"), make_module("a"), make_module("a_2")];
        let result = deduplicate_ids(modules);
        assert_eq!(result[0].module_id, "a", "first 'a' keeps its ID");
        assert_eq!(
            result[1].module_id, "a_3",
            "second 'a' skips 'a_2' (pre-existing) and picks 'a_3'"
        );
        assert_eq!(result[2].module_id, "a_2", "original 'a_2' keeps its ID");
        // All IDs must be distinct.
        let ids: std::collections::HashSet<_> = result.iter().map(|m| &m.module_id).collect();
        assert_eq!(ids.len(), 3, "all three IDs must be distinct");
    }
}
