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
/// `scan` is fallible. Each implementation names its own error type through
/// the associated [`BaseScanner::Error`]; one that cannot fail uses
/// [`std::convert::Infallible`]. Before 0.13.0 `scan` returned a bare
/// `Vec<ScannedModule>`, which left a Rust scanner no way to report the
/// failures the Python and TypeScript scanners raise — it had to panic or
/// return an empty list
/// ([apcore-toolkit-rust#4](https://github.com/aiperceivable/apcore-toolkit-rust/issues/4)).
///
/// # Language-specific API shape
///
/// The Rust `BaseScanner` trait intentionally contains only the primitive `scan()` and
/// `source_name()` operations, keeping it object-safe: with the error type named, it is
/// usable as `Box<dyn BaseScanner<App, Error = E>>`.
///
/// Where Python and TypeScript let each subclass define `scan`'s signature, Rust fixes
/// it to one argument, `app: &App`; an implementation needing several inputs bundles
/// them into its `App` type.
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
/// # Examples
///
/// A scanner that can fail names its error type, and `?` propagates the
/// shared helpers' errors into it:
///
/// ```
/// use apcore_toolkit::{deduplicate_ids, filter_modules, BaseScanner, ScannedModule};
/// use async_trait::async_trait;
/// use serde_json::json;
///
/// /// A route table standing in for a framework's application type.
/// struct Routes(Vec<&'static str>);
///
/// #[derive(Debug, thiserror::Error)]
/// enum RoutesScanError {
///     #[error("invalid include pattern: {0}")]
///     Pattern(#[from] regex::Error),
/// }
///
/// struct RoutesScanner {
///     include: Option<String>,
/// }
///
/// #[async_trait]
/// impl BaseScanner<Routes> for RoutesScanner {
///     type Error = RoutesScanError;
///
///     async fn scan(&self, app: &Routes) -> Result<Vec<ScannedModule>, Self::Error> {
///         let modules: Vec<ScannedModule> = app
///             .0
///             .iter()
///             .map(|id| {
///                 ScannedModule::new(
///                     (*id).to_string(),
///                     format!("Handler for {id}"),
///                     json!({"type": "object"}),
///                     json!({"type": "object"}),
///                     vec![],
///                     format!("routes:{id}"),
///                 )
///             })
///             .collect();
///         let filtered = filter_modules(&modules, self.include.as_deref(), None)?;
///         Ok(deduplicate_ids(filtered))
///     }
///
///     fn source_name(&self) -> &str {
///         "routes"
///     }
/// }
///
/// let app = Routes(vec!["users.get", "users.get", "tasks.list"]);
/// let scanner = RoutesScanner { include: Some(r"^users\.".to_string()) };
/// let modules = futures::executor::block_on(scanner.scan(&app))?;
/// assert_eq!(modules.len(), 2);
/// assert_eq!(modules[1].module_id, "users.get_2");
///
/// let broken = RoutesScanner { include: Some("[unclosed".to_string()) };
/// assert!(futures::executor::block_on(broken.scan(&app)).is_err());
/// # Ok::<(), RoutesScanError>(())
/// ```
///
/// Framework adapters have the same shape (not compiled here: they need the
/// framework crates):
///
/// ```ignore
/// // Example: Axum adapter
/// struct AxumScanner;
///
/// #[async_trait]
/// impl BaseScanner<axum::Router> for AxumScanner {
///     type Error = AxumScanError;
///     async fn scan(&self, app: &axum::Router) -> Result<Vec<ScannedModule>, Self::Error> { /* ... */ }
///     fn source_name(&self) -> &str { "axum" }
/// }
///
/// // Example: Actix adapter that cannot fail
/// struct ActixScanner;
///
/// #[async_trait]
/// impl BaseScanner<()> for ActixScanner {
///     type Error = std::convert::Infallible;
///     async fn scan(&self, _app: &()) -> Result<Vec<ScannedModule>, Self::Error> { /* ... */ }
///     fn source_name(&self) -> &str { "actix-web" }
/// }
/// ```
#[async_trait]
pub trait BaseScanner<App: Send + Sync = ()> {
    /// Error returned by `scan`; use `std::convert::Infallible` for a scanner that cannot fail.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Scan endpoints and return module definitions.
    ///
    /// The `app` parameter receives framework-specific state (e.g., `axum::Router`,
    /// `actix_web::App`). Use `()` if no app context is needed.
    ///
    /// # Errors
    ///
    /// Returns `Err(Self::Error)` when the scan cannot produce a module list —
    /// for example an invalid `include`/`exclude` pattern forwarded to
    /// [`filter_modules`]. What counts as a failure is the implementation's
    /// to define, as it is in the Python and TypeScript SDKs.
    async fn scan(&self, app: &App) -> Result<Vec<ScannedModule>, Self::Error>;

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
/// Canonical RFC 9110 mapping (aligned with Python and TypeScript SDKs and
/// the spec at apcore-toolkit/docs/features/scanning.md):
/// - GET            -> readonly=true, cacheable=true
/// - HEAD           -> readonly=true (no cacheable)
/// - OPTIONS        -> readonly=true (no cacheable)
/// - DELETE         -> destructive=true
/// - PUT            -> idempotent=true
/// - POST           -> default (all false; creates resources, not idempotent by spec)
/// - PATCH          -> default (partial update, not standardly idempotent)
/// - unknown method -> default (all false)
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

    // ---- BaseScanner ----

    #[derive(Debug, thiserror::Error)]
    #[error("scan failed: {0}")]
    struct TestScanError(String);

    struct FixedScanner(Vec<&'static str>);

    #[async_trait]
    impl BaseScanner<()> for FixedScanner {
        type Error = TestScanError;

        async fn scan(&self, _app: &()) -> Result<Vec<ScannedModule>, Self::Error> {
            Ok(self.0.iter().map(|id| make_module(id)).collect())
        }

        fn source_name(&self) -> &str {
            "fixed"
        }
    }

    struct FailingScanner;

    #[async_trait]
    impl BaseScanner<()> for FailingScanner {
        type Error = TestScanError;

        async fn scan(&self, _app: &()) -> Result<Vec<ScannedModule>, Self::Error> {
            Err(TestScanError("app not ready".into()))
        }

        fn source_name(&self) -> &str {
            "failing"
        }
    }

    struct InfallibleScanner;

    #[async_trait]
    impl BaseScanner<()> for InfallibleScanner {
        type Error = std::convert::Infallible;

        async fn scan(&self, _app: &()) -> Result<Vec<ScannedModule>, Self::Error> {
            Ok(vec![make_module("always.ok")])
        }

        fn source_name(&self) -> &str {
            "infallible"
        }
    }

    /// Scans the ids listed in the app value itself: a non-`()` `App`.
    struct AppDrivenScanner;

    #[async_trait]
    impl BaseScanner<Vec<String>> for AppDrivenScanner {
        type Error = TestScanError;

        async fn scan(&self, app: &Vec<String>) -> Result<Vec<ScannedModule>, Self::Error> {
            if app.is_empty() {
                return Err(TestScanError("no routes".into()));
            }
            Ok(app.iter().map(|id| make_module(id)).collect())
        }

        fn source_name(&self) -> &str {
            "app-driven"
        }
    }

    #[test]
    fn test_base_scanner_boxed_trait_object_with_named_error() {
        let scanners: Vec<Box<dyn BaseScanner<(), Error = TestScanError>>> = vec![
            Box::new(FixedScanner(vec!["users.get", "tasks.list"])),
            Box::new(FailingScanner),
        ];

        let first = futures::executor::block_on(scanners[0].scan(&())).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(scanners[0].source_name(), "fixed");

        let second = futures::executor::block_on(scanners[1].scan(&()));
        assert_eq!(
            second.unwrap_err().to_string(),
            "scan failed: app not ready"
        );
        assert_eq!(scanners[1].source_name(), "failing");
    }

    #[test]
    fn test_base_scanner_boxed_trait_object_infallible() {
        let scanner: Box<dyn BaseScanner<(), Error = std::convert::Infallible> + Send + Sync> =
            Box::new(InfallibleScanner);
        let Ok(modules) = futures::executor::block_on(scanner.scan(&()));
        assert_eq!(modules[0].module_id, "always.ok");
    }

    #[test]
    fn test_base_scanner_boxed_trait_object_with_app_type() {
        let scanner: Box<dyn BaseScanner<Vec<String>, Error = TestScanError>> =
            Box::new(AppDrivenScanner);
        let app = vec!["a.get".to_string()];
        let modules = futures::executor::block_on(scanner.scan(&app)).unwrap();
        assert_eq!(modules[0].module_id, "a.get");
        assert!(futures::executor::block_on(scanner.scan(&Vec::new())).is_err());
    }

    #[test]
    fn test_base_scanner_error_propagates_into_boxed_error() {
        // The `std::error::Error + Send + Sync + 'static` bound is what lets
        // a caller holding any scanner use `?` into a boxed error.
        fn count_modules<S: BaseScanner<()>>(
            scanner: &S,
        ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
            Ok(futures::executor::block_on(scanner.scan(&()))?.len())
        }

        assert_eq!(count_modules(&FixedScanner(vec!["a"])).unwrap(), 1);
        assert_eq!(count_modules(&InfallibleScanner).unwrap(), 1);
        let err = count_modules(&FailingScanner).unwrap_err();
        assert_eq!(err.to_string(), "scan failed: app not ready");
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
        // Canonical RFC 9110 mapping: HEAD must return readonly=true (without
        // cacheable=true), matching Python and TypeScript implementations and
        // the spec in apcore-toolkit/docs/features/scanning.md.
        let ann = infer_annotations_from_method("HEAD");
        assert!(ann.readonly, "HEAD must be readonly (matches Python/TS)");
        assert!(!ann.cacheable, "HEAD must NOT be cacheable");
        assert!(!ann.destructive);
        assert!(!ann.idempotent);
    }

    #[test]
    fn test_infer_annotations_options() {
        // Canonical RFC 9110 mapping: OPTIONS must return readonly=true (without
        // cacheable=true), matching Python and TypeScript implementations and
        // the spec in apcore-toolkit/docs/features/scanning.md.
        let ann = infer_annotations_from_method("OPTIONS");
        assert!(ann.readonly, "OPTIONS must be readonly (matches Python/TS)");
        assert!(!ann.cacheable, "OPTIONS must NOT be cacheable");
        assert!(!ann.destructive);
        assert!(!ann.idempotent);
    }

    #[test]
    fn test_infer_annotations_head_case_insensitive() {
        let ann = infer_annotations_from_method("head");
        assert!(
            ann.readonly,
            "HEAD (lowercase) should also return readonly=true"
        );
        assert!(!ann.cacheable);
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
