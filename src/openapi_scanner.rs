// OpenAPIScanner — turn an OpenAPI 3.x document into a `ScannedModule` list.
//
// Document-level traversal layered on top of the shipped operation-level
// primitives in `crate::openapi` (`extract_input_schema`,
// `extract_output_schema`, `resolve_ref`) and
// `crate::scanner::infer_annotations_from_method`.
//
// See `apcore-toolkit/docs/features/openapi-scanner.md` for the full V1
// specification, worked examples, and conformance corpus.
//
// # Why this is not a `BaseScanner` impl
//
// `BaseScanner::scan` is `async fn scan(&self, app: &App) -> Vec<ScannedModule>`
// — a fixed single-argument, infallible-return shape designed for
// framework-introspection scanners. `OpenAPIScanner` needs multiple named
// parameters (`spec`, `include`, `exclude`, `base_path_prefix`,
// `include_deprecated`, three optional hook closures) *and* a fallible
// return (`Err` for a non-OpenAPI-3.x document). Forcing that into
// `BaseScanner` would require an awkward options-struct-as-`App` hack and
// silently swallowing the one real error case.
//
// This is a deliberate, documented gap — see
// [`apcore-toolkit-rust#4`](https://github.com/aiperceivable/apcore-toolkit-rust/issues/4),
// the tracked issue for evolving the trait to support this shape.
// `OpenAPIScanner` instead exposes an inherent `scan` method, and reuses
// the trait's free functions (`filter_modules`, `deduplicate_ids`,
// `infer_annotations_from_method`) directly, exactly as they are designed
// to be reused outside a `BaseScanner` impl.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::openapi::{extract_input_schema, extract_output_schema, resolve_ref};
use crate::scanner::{deduplicate_ids, filter_modules, infer_annotations_from_method};
use crate::types::ScannedModule;

/// Only these path-item keys are treated as HTTP operations (OpenAPI 3.x
/// Path Item Object). Everything else (`summary`, `parameters`, `servers`,
/// `$ref`, vendor `x-*` extensions, ...) is skipped.
const RECOGNIZED_METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

static SANITIZE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_.\-]").expect("valid regex"));
static DOT_RUN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.+").expect("valid regex"));
static ABS_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9+.\-]*://").expect("valid regex"));
static TEMPLATE_VAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{([^}]+)\}").expect("valid regex"));

/// Errors returned by [`OpenAPIScanner::scan`].
#[derive(Debug, thiserror::Error)]
pub enum ScannerError {
    /// The document is not OpenAPI 3.0.x / 3.1.x (missing `openapi` key,
    /// or `swagger: "2.0"`). This is a caller error, not spec noise —
    /// every other malformed-operation case degrades to a warning instead
    /// (see the Error Model in the spec doc).
    #[error("{0}")]
    InvalidSpec(String),
    /// An invalid `include`/`exclude` regex, inherited verbatim from
    /// [`filter_modules`]'s error contract.
    #[error(transparent)]
    Pattern(#[from] regex::Error),
}

fn sanitize(candidate: &str) -> String {
    let step1 = SANITIZE_RE.replace_all(candidate, "_");
    let step2 = DOT_RUN_RE.replace_all(&step1, ".");
    step2.trim_matches(|c| c == '.' || c == '_').to_string()
}

/// Derive a stable, byte-identical `module_id` for an OpenAPI operation.
///
/// See `apcore-toolkit/docs/features/openapi-scanner.md` § `module_id`
/// Derivation for the algorithm and worked examples. This function is the
/// primary subject of the cross-SDK conformance corpus — implementations
/// MUST match it byte-for-byte.
///
/// - `path`: the OpenAPI path template (e.g. `"/users/{user_id}"`).
/// - `method`: the HTTP method key as written in the document (e.g. `"get"`).
/// - `operation`: the operation object, consulted only for `operationId`.
///
/// Never empty — falls back to `"root.<method>"`.
pub fn derive_module_id(path: &str, method: &str, operation: &Value) -> String {
    if let Some(operation_id) = operation.get("operationId").and_then(Value::as_str) {
        if !operation_id.is_empty() {
            let candidate = sanitize(operation_id);
            if !candidate.is_empty() {
                return candidate;
            }
        }
    }

    let raw_segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if raw_segments.is_empty() {
        return format!("root.{}", method.to_lowercase());
    }

    let segments: Vec<String> = raw_segments
        .iter()
        .map(|seg| {
            if seg.len() >= 2 && seg.starts_with('{') && seg.ends_with('}') {
                seg[1..seg.len() - 1].to_string()
            } else {
                (*seg).to_string()
            }
        })
        .collect();

    let mut candidate = segments.join(".");
    candidate.push('.');
    candidate.push_str(method);
    let candidate = sanitize(&candidate.to_lowercase());
    if candidate.is_empty() {
        return format!("root.{}", method.to_lowercase());
    }
    candidate
}

fn collect_refs(node: &Value, out: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get("$ref") {
                out.push(r.clone());
            }
            for value in map.values() {
                collect_refs(value, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_refs(item, out);
            }
        }
        _ => {}
    }
}

/// Warn on unresolvable internal refs and refuse external refs.
///
/// Internal refs (`#/...`) that resolve successfully are silent — this
/// only flags the failure cases enumerated in the Error Model:
/// unresolvable internal `$ref` and external `$ref` (never fetched).
fn ref_warnings(operation: &Value, spec: &Value) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    if let Some(request_body) = operation.get("requestBody") {
        collect_refs(request_body, &mut refs);
    }
    if let Some(responses) = operation.get("responses") {
        collect_refs(responses, &mut refs);
    }

    let mut warnings = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in refs {
        if !seen.insert(r.clone()) {
            continue;
        }
        if !r.starts_with("#/") {
            warnings.push(format!("external $ref not fetched: {r}"));
        } else {
            let resolved = resolve_ref(&r, spec);
            let is_empty = resolved.as_object().is_none_or(Map::is_empty);
            if is_empty {
                warnings.push(format!("unresolvable $ref: {r}"));
            }
        }
    }
    warnings
}

fn first_line(text: Option<&str>) -> Option<String> {
    let text = text?;
    if text.is_empty() {
        return None;
    }
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn has_2xx_response(operation: &Value) -> bool {
    operation
        .get("responses")
        .and_then(Value::as_object)
        .is_some_and(|responses| {
            responses.keys().any(|k| {
                k.len() == 3 && k.starts_with('2') && k.chars().skip(1).all(|c| c.is_ascii_digit())
            })
        })
}

/// Best-effort resolution of `servers[0].url`.
///
/// Absolute URLs are used verbatim. Templated URLs are substituted from
/// `servers[0].variables[*].default` when every variable has one;
/// otherwise the URL is unusable and omitted. Relative URLs require the
/// spec's *source* URL to resolve against, which `scan()` — pure and
/// I/O-free — does not have; they are omitted here (advisory only; the
/// caller supplies `base_url` to the writer regardless).
fn resolve_server_url(spec: &Value) -> Option<String> {
    let servers = spec.get("servers")?.as_array()?;
    let first = servers.first()?.as_object()?;
    let url = first.get("url")?.as_str()?;
    if url.is_empty() || !ABS_URL_RE.is_match(url) {
        return None;
    }
    let mut url = url.to_string();

    if let Some(variables) = first.get("variables").and_then(Value::as_object) {
        if !variables.is_empty() {
            let mut substitutions: HashMap<String, String> = HashMap::new();
            for (name, var) in variables {
                let default = var.as_object().and_then(|o| o.get("default"))?;
                let default_str = match default {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                substitutions.insert(name.clone(), default_str);
            }
            url = TEMPLATE_VAR_RE
                .replace_all(&url, |caps: &regex::Captures<'_>| {
                    substitutions
                        .get(&caps[1])
                        .cloned()
                        .unwrap_or_else(|| caps[0].to_string())
                })
                .to_string();
            if url.contains('{') || url.contains('}') {
                return None;
            }
        }
    }

    Some(url)
}

/// A `transform_operation` hook: `(path, method, operation) -> Option<operation>`.
/// Returning `None` skips the operation entirely.
pub type TransformOperationHook = Box<dyn Fn(&str, &str, &Value) -> Option<Value> + Send + Sync>;
/// A `derive_module_id` override hook: `(path, method, operation) -> Option<module_id>`.
/// Returning `None` falls back to the default derivation.
pub type DeriveModuleIdHook = Box<dyn Fn(&str, &str, &Value) -> Option<String> + Send + Sync>;
/// A `transform_module` hook: `(module) -> Option<module>`. Returning
/// `None` drops the module from the result.
pub type TransformModuleHook = Box<dyn Fn(ScannedModule) -> Option<ScannedModule> + Send + Sync>;

/// Options for [`OpenAPIScanner::scan`].
///
/// See `Contract: OpenAPIScanner.scan` in
/// `apcore-toolkit/docs/features/openapi-scanner.md`.
pub struct ScanOptions {
    /// Forwarded to [`filter_modules`].
    pub include: Option<String>,
    /// Forwarded to [`filter_modules`].
    pub exclude: Option<String>,
    /// When set, prepended to every derived `module_id` as
    /// `"<prefix>.<id>"`; applied before filtering and deduplication.
    pub base_path_prefix: Option<String>,
    /// When `false`, operations with `deprecated: true` are omitted
    /// entirely rather than annotated. Defaults to `true`.
    pub include_deprecated: bool,
    /// See [`TransformOperationHook`].
    pub transform_operation: Option<TransformOperationHook>,
    /// See [`DeriveModuleIdHook`].
    pub derive_module_id: Option<DeriveModuleIdHook>,
    /// See [`TransformModuleHook`].
    pub transform_module: Option<TransformModuleHook>,
}

impl ScanOptions {
    /// `ScanOptions` with every field at its documented default
    /// (`include_deprecated: true`, everything else unset).
    pub fn new() -> Self {
        Self {
            include: None,
            exclude: None,
            base_path_prefix: None,
            include_deprecated: true,
            transform_operation: None,
            derive_module_id: None,
            transform_module: None,
        }
    }
}

// `#[derive(Default)]` would set `include_deprecated` to `false` (bool's
// zero value); the documented default is `true`, so `Default` delegates
// to the hand-written `ScanOptions::new()` instead.
impl Default for ScanOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Turn an OpenAPI 3.0/3.1 document into a list of `ScannedModule`.
///
/// Pure and (per the `async` note on [`OpenAPIScanner::scan`]) awaits
/// nothing internally: it accepts an already-parsed document and performs
/// no I/O. Use [`crate::openapi_scanner::load_spec`] (behind the
/// `http-proxy` feature) to fetch/parse a document first.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAPIScanner;

impl OpenAPIScanner {
    pub fn new() -> Self {
        Self
    }

    /// The literal string `"openapi"`, matching the doc's Contract block
    /// naming across all three SDKs.
    pub fn get_source_name(&self) -> &str {
        "openapi"
    }

    /// Scan an OpenAPI document, returning one `ScannedModule` per
    /// recognised operation.
    ///
    /// `scan` is `async fn` for API-shape consistency with the ecosystem
    /// `Scanner` trait convention (see the module-level doc comment for
    /// why `OpenAPIScanner` does not literally implement `BaseScanner`),
    /// even though this implementation performs no `.await`ing
    /// internally — the document is already parsed by the time it
    /// arrives.
    ///
    /// See `Contract: OpenAPIScanner.scan` in
    /// `apcore-toolkit/docs/features/openapi-scanner.md`.
    ///
    /// # Errors
    ///
    /// Returns [`ScannerError::InvalidSpec`] when `spec` is not OpenAPI
    /// 3.0.x/3.1.x. Returns [`ScannerError::Pattern`] when `include` or
    /// `exclude` is not a valid regex.
    #[allow(clippy::too_many_lines)]
    pub async fn scan(
        &self,
        spec: &Value,
        options: &ScanOptions,
    ) -> Result<Vec<ScannedModule>, ScannerError> {
        Self::validate_spec(spec)?;

        let empty_paths = Map::new();
        let paths = spec
            .get("paths")
            .and_then(Value::as_object)
            .unwrap_or(&empty_paths);

        let openapi_version = spec
            .get("openapi")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let doc_version = spec
            .get("info")
            .and_then(|info| info.get("version"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("1.0.0")
            .to_string();
        let server_url = resolve_server_url(spec);

        let mut modules: Vec<ScannedModule> = Vec::new();

        for (path, path_item) in paths {
            let Some(path_item_obj) = path_item.as_object() else {
                continue;
            };
            for (key, raw_operation) in path_item_obj {
                let method = key.to_lowercase();
                if !RECOGNIZED_METHODS.contains(&method.as_str()) || !raw_operation.is_object() {
                    continue;
                }

                let operation: Value = match &options.transform_operation {
                    Some(hook) => match hook(path, &method, raw_operation) {
                        Some(v) => v,
                        None => continue,
                    },
                    None => raw_operation.clone(),
                };

                let deprecated = operation
                    .get("deprecated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if deprecated && !options.include_deprecated {
                    continue;
                }

                let mut module_id = match &options.derive_module_id {
                    Some(hook) => hook(path, &method, &operation)
                        .unwrap_or_else(|| derive_module_id(path, &method, &operation)),
                    None => derive_module_id(path, &method, &operation),
                };
                if let Some(prefix) = options.base_path_prefix.as_deref() {
                    if !prefix.is_empty() {
                        module_id = format!("{prefix}.{module_id}");
                    }
                }

                let mut warnings = ref_warnings(&operation, spec);

                let input_schema = extract_input_schema(&operation, Some(spec));
                let output_schema = extract_output_schema(&operation, Some(spec));
                if !has_2xx_response(&operation) {
                    warnings.push("no 2xx response defined; output_schema is empty".to_string());
                }

                let mut annotations = infer_annotations_from_method(&method);
                if deprecated {
                    // `ModuleAnnotations` has no first-class `deprecated`
                    // field; the toolkit convention (matching
                    // `tui_view_model::Filter`) is
                    // `annotations.extra["deprecated"]`.
                    annotations
                        .extra
                        .insert("deprecated".to_string(), Value::Bool(true));
                }

                let description = operation
                    .get("summary")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .or_else(|| first_line(operation.get("description").and_then(Value::as_str)))
                    .unwrap_or_default();

                let mut openapi_meta = Map::new();
                openapi_meta.insert(
                    "spec_version".to_string(),
                    Value::String(openapi_version.clone()),
                );
                if let Some(operation_id) = operation
                    .get("operationId")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    openapi_meta.insert(
                        "operation_id".to_string(),
                        Value::String(operation_id.to_string()),
                    );
                }
                if let Some(su) = &server_url {
                    openapi_meta.insert("server_url".to_string(), Value::String(su.clone()));
                }
                if let Some(summary) = operation
                    .get("summary")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    openapi_meta.insert("summary".to_string(), Value::String(summary.to_string()));
                }

                let mut metadata: HashMap<String, Value> = HashMap::new();
                metadata.insert(
                    "http_method".to_string(),
                    Value::String(method.to_uppercase()),
                );
                metadata.insert("url_path".to_string(), Value::String(path.clone()));
                metadata.insert("openapi".to_string(), Value::Object(openapi_meta));

                let tags: Vec<String> = operation
                    .get("tags")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();

                let mut module = ScannedModule::new(
                    module_id,
                    description,
                    input_schema,
                    output_schema,
                    tags,
                    format!("{} {}", method.to_uppercase(), path),
                );
                module.version = doc_version.clone();
                module.annotations = Some(annotations);
                module.documentation = operation
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                module.metadata = metadata;
                module.warnings = warnings;

                let module = match &options.transform_module {
                    Some(hook) => match hook(module) {
                        Some(m) => m,
                        None => continue,
                    },
                    None => module,
                };

                modules.push(module);
            }
        }

        let filtered = filter_modules(
            &modules,
            options.include.as_deref(),
            options.exclude.as_deref(),
        )?;
        Ok(deduplicate_ids(filtered))
    }

    fn validate_spec(spec: &Value) -> Result<(), ScannerError> {
        let openapi_version = spec
            .as_object()
            .and_then(|o| o.get("openapi"))
            .and_then(Value::as_str);
        match openapi_version {
            Some(v) if v.starts_with("3.0") || v.starts_with("3.1") => Ok(()),
            _ => Err(ScannerError::InvalidSpec(format!(
                "OpenAPIScanner.scan: unsupported spec — expected OpenAPI 3.0.x or 3.1.x, got 'openapi': {openapi_version:?} (swagger 2.0 is not supported in V1)"
            ))),
        }
    }
}

/// Options for [`load_spec_with_options`].
///
/// `load_spec` (the zero-config entry point matching the doc's minimal
/// Rust example, `load_spec(url).await?`) uses
/// `LoadSpecOptions::default()`.
#[cfg(feature = "http-proxy")]
pub struct LoadSpecOptions {
    /// Extra request headers (ignored for local files).
    pub headers: Option<HashMap<String, String>>,
    /// Invoked once per fetch, for specs behind authentication (ignored
    /// for local files).
    pub auth_header_factory: Option<Box<dyn Fn() -> HashMap<String, String> + Send + Sync>>,
    /// Request timeout in seconds (ignored for local files).
    pub timeout_secs: f64,
}

#[cfg(feature = "http-proxy")]
impl Default for LoadSpecOptions {
    fn default() -> Self {
        Self {
            headers: None,
            auth_header_factory: None,
            timeout_secs: 30.0,
        }
    }
}

/// Errors returned by [`load_spec`] / [`load_spec_with_options`].
///
/// The doc's Contract lists three distinct bare error types per language
/// (`io::Error` / `reqwest::Error` / `serde_json::Error` for Rust); Rust
/// requires a single return type, so — matching the
/// `HTTPProxyRegistryWriterError` / `ScannerError` convention already used
/// elsewhere in this crate — they are wrapped in one `thiserror` enum
/// instead. A malformed **YAML** document (not mentioned in the doc, which
/// only anticipates JSON) gets its own variant rather than being forced
/// into `Json`.
#[cfg(feature = "http-proxy")]
#[derive(Debug, thiserror::Error)]
pub enum LoadSpecError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("malformed YAML spec at '{path}': {error}")]
    Yaml {
        path: String,
        error: serde_yaml_ng::Error,
    },
}

/// Load and parse an OpenAPI document from a local path or `http(s)://`
/// URL, using the default [`LoadSpecOptions`] (no extra headers, no auth,
/// a 30-second timeout).
///
/// Convenience helper, explicitly outside the conformance corpus (I/O
/// behaviour is deliberately not byte-specified). The URL/path is taken
/// verbatim — no candidate paths are probed. See `Contract: load_spec` in
/// `apcore-toolkit/docs/features/openapi-scanner.md`.
///
/// Security: `source` is trusted input. Callers taking a URL from an
/// untrusted source are responsible for their own allowlisting (SSRF).
///
/// For headers, an `auth_header_factory`, or a non-default timeout, use
/// [`load_spec_with_options`] — Rust has no default-parameter-value
/// syntax, so the richer contract (`headers`, `auth_header_factory`,
/// `timeout`) is exposed as a second entry point rather than forcing every
/// caller to spell out three rarely-needed arguments.
#[cfg(feature = "http-proxy")]
pub async fn load_spec(source: &str) -> Result<Value, LoadSpecError> {
    load_spec_with_options(source, &LoadSpecOptions::default()).await
}

/// Full-contract variant of [`load_spec`] taking [`LoadSpecOptions`]
/// (`headers`, `auth_header_factory`, `timeout_secs`).
#[cfg(feature = "http-proxy")]
pub async fn load_spec_with_options(
    source: &str,
    options: &LoadSpecOptions,
) -> Result<Value, LoadSpecError> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let mut request_headers = options.headers.clone().unwrap_or_default();
        if let Some(factory) = &options.auth_header_factory {
            request_headers.extend(factory());
        }
        let timeout = std::time::Duration::from_secs_f64(options.timeout_secs.max(0.0));
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        let mut request = client.get(source);
        for (name, value) in &request_headers {
            request = request.header(name, value);
        }
        let response = request.send().await?.error_for_status()?;
        let text = response.text().await?;
        parse_document(&text, source)
    } else {
        // Reads the file synchronously with `std::fs`. This crate does
        // not otherwise depend on `tokio`, so avoiding `tokio::fs` here
        // keeps `load_spec` from pulling in a new runtime dependency just
        // for a convenience wrapper that is explicitly outside the
        // conformance corpus.
        let text = std::fs::read_to_string(source)?;
        parse_document(&text, source)
    }
}

#[cfg(feature = "http-proxy")]
fn parse_document(text: &str, source: &str) -> Result<Value, LoadSpecError> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        Ok(serde_json::from_str(text)?)
    } else {
        serde_yaml_ng::from_str::<Value>(text).map_err(|error| LoadSpecError::Yaml {
            path: source.to_string(),
            error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        futures::executor::block_on(fut)
    }

    // ---- derive_module_id ----

    #[test]
    fn test_derive_get_users() {
        assert_eq!(derive_module_id("/users", "get", &json!({})), "users.get");
    }

    #[test]
    fn test_derive_post_users() {
        assert_eq!(derive_module_id("/users", "post", &json!({})), "users.post");
    }

    #[test]
    fn test_derive_get_users_id() {
        assert_eq!(
            derive_module_id("/users/{user_id}", "get", &json!({})),
            "users.user_id.get"
        );
    }

    #[test]
    fn test_derive_delete_nested_params() {
        assert_eq!(
            derive_module_id("/users/{user_id}/orders/{order_id}", "delete", &json!({})),
            "users.user_id.orders.order_id.delete"
        );
    }

    #[test]
    fn test_derive_root() {
        assert_eq!(derive_module_id("/", "get", &json!({})), "root.get");
    }

    #[test]
    fn test_derive_v1_pets() {
        assert_eq!(
            derive_module_id("/v1/pets", "get", &json!({})),
            "v1.pets.get"
        );
    }

    #[test]
    fn test_derive_operation_id_preserved() {
        assert_eq!(
            derive_module_id("/users/{id}", "get", &json!({"operationId": "getUserById"})),
            "getUserById"
        );
    }

    #[test]
    fn test_derive_sanitize_illegal_chars() {
        assert_eq!(derive_module_id("/a b/c", "post", &json!({})), "a_b.c.post");
    }

    // ---- OpenAPIScanner::scan ----

    #[test]
    fn test_scan_rejects_swagger_2() {
        let spec =
            json!({"swagger": "2.0", "info": {"title": "t", "version": "1.0.0"}, "paths": {}});
        let scanner = OpenAPIScanner::new();
        let result = block_on(scanner.scan(&spec, &ScanOptions::new()));
        assert!(matches!(result, Err(ScannerError::InvalidSpec(_))));
    }

    #[test]
    fn test_scan_empty_paths() {
        let spec =
            json!({"openapi": "3.0.3", "info": {"title": "t", "version": "1.0.0"}, "paths": {}});
        let scanner = OpenAPIScanner::new();
        let result = block_on(scanner.scan(&spec, &ScanOptions::new())).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_scan_single_get() {
        let spec = json!({
            "openapi": "3.0.3",
            "info": {"title": "t", "version": "1.0.0"},
            "paths": {
                "/users": {"get": {"responses": {"200": {"description": "ok"}}}}
            }
        });
        let scanner = OpenAPIScanner::new();
        let modules = block_on(scanner.scan(&spec, &ScanOptions::new())).unwrap();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].module_id, "users.get");
        assert_eq!(
            modules[0]
                .metadata
                .get("http_method")
                .and_then(Value::as_str),
            Some("GET")
        );
    }

    #[test]
    fn test_scan_invalid_include_regex_is_pattern_error() {
        let spec = json!({
            "openapi": "3.0.3",
            "info": {"title": "t", "version": "1.0.0"},
            "paths": {"/users": {"get": {"responses": {"200": {"description": "ok"}}}}}
        });
        let scanner = OpenAPIScanner::new();
        let options = ScanOptions {
            include: Some("[invalid".to_string()),
            ..ScanOptions::new()
        };
        let result = block_on(scanner.scan(&spec, &options));
        assert!(matches!(result, Err(ScannerError::Pattern(_))));
    }

    #[test]
    fn test_get_source_name() {
        assert_eq!(OpenAPIScanner::new().get_source_name(), "openapi");
    }

    // ---- load_spec ----

    #[cfg(feature = "http-proxy")]
    #[test]
    fn test_load_spec_local_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spec.json");
        std::fs::write(&path, r#"{"openapi": "3.0.3", "paths": {}}"#).unwrap();
        let doc = block_on(load_spec(path.to_str().unwrap())).unwrap();
        assert_eq!(doc["openapi"], "3.0.3");
    }

    #[cfg(feature = "http-proxy")]
    #[test]
    fn test_load_spec_local_yaml_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spec.yaml");
        std::fs::write(&path, "openapi: 3.0.3\npaths: {}\n").unwrap();
        let doc = block_on(load_spec(path.to_str().unwrap())).unwrap();
        assert_eq!(doc["openapi"], "3.0.3");
    }

    #[cfg(feature = "http-proxy")]
    #[test]
    fn test_load_spec_missing_file_is_io_error() {
        let result = block_on(load_spec("/nonexistent/path/spec.json"));
        assert!(matches!(result, Err(LoadSpecError::Io(_))));
    }

    #[cfg(feature = "http-proxy")]
    #[test]
    fn test_load_spec_malformed_json_is_json_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{not valid json").unwrap();
        let result = block_on(load_spec(path.to_str().unwrap()));
        assert!(matches!(result, Err(LoadSpecError::Json(_))));
    }

    // The four tests above only ever exercise the local-file branch of
    // `load_spec_with_options`; none of them ever build a `reqwest::Client`
    // or open a socket. This drives the `http://` branch (source lines
    // ~613-626) against a real loopback HTTP server, following the same
    // hand-rolled-responder pattern used for the W2 regression tests in
    // `src/output/http_proxy_writer.rs`. `futures::executor::block_on` (this
    // module's `block_on` helper) has no I/O reactor, so this needs a real
    // `tokio` runtime -- hence `#[tokio::test]` rather than the shared
    // helper.
    #[cfg(feature = "http-proxy")]
    #[tokio::test]
    async fn test_load_spec_with_options_http_branch_fetches_from_loopback_server() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
        let addr = listener.local_addr().expect("local_addr");
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let body = r#"{"openapi":"3.0.3","paths":{}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });

        let url = format!("http://{addr}/spec.json");
        let doc = load_spec_with_options(&url, &LoadSpecOptions::default())
            .await
            .expect("load_spec_with_options over http");
        assert_eq!(doc["openapi"], "3.0.3");

        handle.join().expect("mock server thread panicked");
    }
}
