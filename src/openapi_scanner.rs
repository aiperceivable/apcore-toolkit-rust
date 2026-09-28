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
// [`apcore-toolkit-rust#4`](https://github.com/aiperceivable/apcore-toolkit-rust/issues/4)
// anticipated `OpenAPIScanner` as the trait's first non-framework
// implementation, and two things stood in the way. One is fixed:
// `BaseScanner::scan` now returns `Result<Vec<ScannedModule>, Self::Error>`,
// so a scanner no longer has to swallow its errors. The other is kept by
// decision, not by limitation: `OpenAPIScanner`'s input is a document plus
// options (`spec`, `include`, `exclude`, `base_path_prefix`,
// `include_deprecated`, three optional hook closures), not a framework
// application, and no consumer in the ecosystem dispatches over
// `BaseScanner` implementations polymorphically. `axum-apcore`, the one
// Rust adapter that serves a route scanner and an OpenAPI scanner behind a
// single `dyn`, does so through its own scanner trait, binding the document
// into its OpenAPI scanner at construction.
//
// `OpenAPIScanner` therefore exposes an inherent `scan` method, and reuses
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

// `normalize_module_id` regexes. Every class is a literal ASCII range:
// `\d`, `\w` and `(?i)` are Unicode-aware in the `regex` crate (and in at
// least one of the Python / JavaScript engines), so they would let a
// non-ASCII letter or digit through in one SDK only.
static ACRONYM_BOUNDARY_RE: LazyLock<Regex> =
    // `([A-Z])`, not `([A-Z]+)`, to match the spec text. The output is the
    // same either way and the `regex` crate is linear-time for both; the spec
    // pins this form because the `+` form is quadratic in backtracking engines.
    LazyLock::new(|| Regex::new(r"([A-Z])([A-Z][a-z])").expect("valid regex"));
static WORD_BOUNDARY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([a-z0-9])([A-Z])").expect("valid regex"));
// In Unicode mode (the default) a negated class matches one whole code
// point, so each non-ASCII character becomes exactly one `_`.
static OUTSIDE_ID_ALPHABET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_.]").expect("valid regex"));
/// One segment of an apcore Canonical ID (PROTOCOL_SPEC §2.7). `^…$`
/// without `(?m)` is a full match in the `regex` crate — unlike Python's
/// `$`, it does not also accept a trailing newline.
static LEGAL_SEGMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]*$").expect("valid regex"));
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

/// Project `raw` into apcore's Canonical ID alphabet (`[a-z0-9_.]`),
/// converting camelCase / PascalCase words to snake_case.
///
/// The algorithm is pinned byte-for-byte in
/// `apcore-toolkit/docs/features/openapi-scanner.md` § `module_id`
/// Derivation (`normalize_module_id`):
///
/// 1. insert `_` at word boundaries: acronym-before-word
///    (`getHTTPResponse` → `getHTTP_Response`) and lower/digit-before-upper
///    (→ `get_HTTP_Response`) — the two replacements commute;
/// 2. replace every code point outside `[A-Za-z0-9_.]` with one `_`;
/// 3. ASCII-lowercase — only ASCII is left after step 2, which is why
///    lowercasing waits for it: U+212A KELVIN SIGN would otherwise
///    lowercase to ASCII `k` and survive as a letter;
/// 4. split on `.`, strip **leading** `_` from each segment, and drop
///    segments left empty — runs of `_` and trailing `_` are kept;
/// 5. rejoin with `.`.
///
/// A legal apcore module ID is returned unchanged
/// (`read_item_items__item_id__get`, FastAPI's default `operationId`
/// shape, is registrable as-is and stays so), and the function is
/// idempotent on its own output. The result can be empty, and can still be
/// illegal when a segment begins with a digit (`v1.2fa.post`) — repairing
/// that would invent a name, so the scanner warns instead (see
/// [`legality_warning`]).
fn normalize_module_id(raw: &str) -> String {
    // `${1}_${2}`, not `$1_$2`: the `regex` crate reads `$1_` as a group
    // *named* `1_`, which does not exist and expands to the empty string.
    let acronyms_split = ACRONYM_BOUNDARY_RE.replace_all(raw, "${1}_${2}");
    let words_split = WORD_BOUNDARY_RE.replace_all(&acronyms_split, "${1}_${2}");
    let in_alphabet = OUTSIDE_ID_ALPHABET_RE
        .replace_all(&words_split, "_")
        .to_ascii_lowercase();
    in_alphabet
        .split('.')
        .map(|segment| segment.trim_start_matches('_'))
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<&str>>()
        .join(".")
}

/// The pinned warning for an emitted `module_id` that is still not a legal
/// apcore ID after normalisation and deduplication, or `None` when it is
/// legal.
///
/// Names the first `.`-segment failing `^[a-z][a-z0-9_]*$`; an empty ID
/// has exactly one segment, the empty string. See "What normalisation will
/// not repair" in `apcore-toolkit/docs/features/openapi-scanner.md`.
fn legality_warning(module_id: &str) -> Option<String> {
    let segment = module_id
        .split('.')
        .find(|segment| !LEGAL_SEGMENT_RE.is_match(segment))?;
    Some(format!(
        "module_id '{module_id}' is not a legal apcore module ID: segment '{segment}' must match ^[a-z][a-z0-9_]*$; name this operation with a derive_module_id or transform_module hook"
    ))
}

/// Strip one enclosing `{`…`}` pair from a path-template segment
/// (`{user_id}` → `user_id`); any other segment is returned unchanged.
fn strip_template_braces(segment: &str) -> &str {
    if segment.len() >= 2 && segment.starts_with('{') && segment.ends_with('}') {
        &segment[1..segment.len() - 1]
    } else {
        segment
    }
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
/// A non-empty `operationId` is converted to snake_case
/// (`getUserById` → `get_user_by_id`) and loses any trailing `_`
/// (`getUser_` → `get_user`, as V1's sanitiser did). That strip happens here
/// only: the scanner's final normalisation keeps a trailing `_`, so a legal
/// ID a hook returns (`abc_`) is never rewritten. Otherwise the ID is built from the
/// path segments, each normalised on its own, plus the lowercased method
/// (`/user-profiles/{userId}` + `get` → `user_profiles.user_id.get`). A
/// path with no segment that survives normalisation (`/`, `/-`, `/{}`)
/// falls back to `"root.<method>"`, so the result is never empty.
///
/// The result is a legal apcore module ID unless a segment begins with a
/// digit (`/v1/2fa` → `v1.2fa.post`). This function does not warn about
/// that; [`OpenAPIScanner::scan`] does.
pub fn derive_module_id(path: &str, method: &str, operation: &Value) -> String {
    if let Some(operation_id) = operation.get("operationId").and_then(Value::as_str) {
        let normalized = normalize_module_id(operation_id);
        let candidate = normalized.trim_end_matches('_');
        if !candidate.is_empty() {
            return candidate.to_string();
        }
    }

    let mut parts: Vec<String> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| normalize_module_id(strip_template_braces(segment)))
        .filter(|part| !part.is_empty())
        .collect();
    let method = method.to_lowercase();
    if parts.is_empty() {
        return format!("root.{method}");
    }
    parts.push(method);
    parts.join(".")
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
///
/// The hook chooses the words; the scanner owns the alphabet. Whatever it
/// returns is normalised like the default derivation
/// (`"Custom-Space.GetThing"` is emitted as `custom_space.get_thing`); an
/// ID that is already legal, trailing `_` included, is emitted unchanged.
pub type DeriveModuleIdHook = Box<dyn Fn(&str, &str, &Value) -> Option<String> + Send + Sync>;
/// A `transform_module` hook: `(module) -> Option<module>`. Returning
/// `None` drops the module from the result.
///
/// Runs before the final `module_id` normalisation, so an ID it sets is
/// normalised too.
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
    /// `"<prefix>.<id>"`; applied before filtering and deduplication. The
    /// prefix is part of the final ID and is normalised with it
    /// (`"Pet-Store"` → `pet_store.users.get`).
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
    /// `scan` is `async fn` for API-shape consistency with the ecosystem's
    /// `async fn scan` convention, even though this implementation performs
    /// no `.await`ing internally — the document is already parsed by the
    /// time it arrives. `OpenAPIScanner` is deliberately not a
    /// [`BaseScanner`](crate::scanner::BaseScanner) impl: that trait's
    /// `scan` is fallible too, but takes a framework application, while
    /// this one takes a document plus options, and nothing dispatches over
    /// `BaseScanner` implementations polymorphically (see the module-level
    /// comment).
    ///
    /// Every emitted `module_id` has passed through the `module_id`
    /// normalisation, applied after the `derive_module_id` hook,
    /// `base_path_prefix` and the `transform_module` hook, and before
    /// filtering and deduplication. An emitted ID that is still not a legal
    /// apcore module ID (a segment beginning with a digit, or an empty ID
    /// from a hook) is emitted anyway, with a warning naming it and its
    /// offending segment; the check runs after deduplication, so it names
    /// the ID actually emitted and follows any rename warning. The
    /// consumer decides what to do with it.
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

                let mut module = match &options.transform_module {
                    Some(hook) => match hook(module) {
                        Some(m) => m,
                        None => continue,
                    },
                    None => module,
                };

                // After the last hook, so the alphabet holds for a prefix or
                // a hook's choice too, and before filtering and
                // deduplication, so both see the ID that is emitted.
                module.module_id = normalize_module_id(&module.module_id);

                modules.push(module);
            }
        }

        let filtered = filter_modules(
            &modules,
            options.include.as_deref(),
            options.exclude.as_deref(),
        )?;
        let mut deduplicated = deduplicate_ids(filtered);
        // Checked last, so the warning names the ID actually emitted —
        // `3ds_2`, not `3ds`, for a renamed duplicate.
        for module in &mut deduplicated {
            if let Some(warning) = legality_warning(&module.module_id) {
                module.warnings.push(warning);
            }
        }
        Ok(deduplicated)
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
    fn test_derive_operation_id_snake_case() {
        assert_eq!(
            derive_module_id("/users/{id}", "get", &json!({"operationId": "getUserById"})),
            "get_user_by_id"
        );
    }

    #[test]
    fn test_derive_illegal_chars_become_underscores() {
        assert_eq!(derive_module_id("/a b/c", "post", &json!({})), "a_b.c.post");
    }

    #[test]
    fn test_derive_path_branch_normalised() {
        assert_eq!(
            derive_module_id("/user-profiles/{userId}", "get", &json!({})),
            "user_profiles.user_id.get"
        );
        assert_eq!(
            derive_module_id("/v1/_debug/", "get", &json!({})),
            "v1.debug.get"
        );
        // Was `userprofiles.get` / `pets.petid.get`: 0.12.0 lowercased the
        // path before any word boundary could be seen.
        assert_eq!(
            derive_module_id("/userProfiles", "get", &json!({})),
            "user_profiles.get"
        );
        assert_eq!(
            derive_module_id("/pets/{petId}", "get", &json!({})),
            "pets.pet_id.get"
        );
        // Each segment is normalised on its own; interior and trailing `_`
        // runs are kept, as they are in an `operationId`.
        assert_eq!(
            derive_module_id("/items__/{item_id}", "get", &json!({})),
            "items__.item_id.get"
        );
    }

    #[test]
    fn test_derive_path_with_no_surviving_segment_is_root() {
        for (path, method, want) in [
            ("/", "get", "root.get"),
            ("/-", "get", "root.get"),
            ("/{}", "post", "root.post"),
            ("/\u{1F600}", "get", "root.get"),
            ("/_/-/{}", "delete", "root.delete"),
        ] {
            assert_eq!(
                derive_module_id(path, method, &json!({})),
                want,
                "{method} {path:?}"
            );
        }
    }

    #[test]
    fn test_derive_uppercase_method_lowercased_in_both_branches() {
        assert_eq!(derive_module_id("/users", "GET", &json!({})), "users.get");
        assert_eq!(derive_module_id("/", "GET", &json!({})), "root.get");
    }

    #[test]
    fn test_derive_digit_leading_segment_not_repaired() {
        // Repairing it would invent a name; the scanner warns instead.
        assert_eq!(
            derive_module_id("/v1/2fa", "post", &json!({})),
            "v1.2fa.post"
        );
        assert_eq!(
            derive_module_id("/verify", "get", &json!({"operationId": "3ds"})),
            "3ds"
        );
    }

    #[test]
    fn test_derive_operation_id_normalising_to_empty_falls_back_to_path() {
        for operation_id in ["", "---", "__", "\u{212A}", "\u{1F600}"] {
            assert_eq!(
                derive_module_id("/users", "get", &json!({"operationId": operation_id})),
                "users.get",
                "operationId {operation_id:?}"
            );
        }
    }

    #[test]
    fn test_derive_operation_id_trailing_underscore_stripped() {
        for (operation_id, want) in [
            ("getUser_", "get_user"),
            ("list-", "list"),
            ("get_user__", "get_user"),
            // The strip applies to the end of the ID, not to every segment.
            ("Users_.Get_", "users_.get"),
        ] {
            assert_eq!(
                derive_module_id("/x", "get", &json!({"operationId": operation_id})),
                want,
                "operationId {operation_id:?}"
            );
        }
    }

    #[test]
    fn test_derive_path_branch_keeps_trailing_underscore() {
        assert_eq!(derive_module_id("/users_", "get", &json!({})), "users_.get");
    }

    #[test]
    fn test_derive_non_string_operation_id_ignored() {
        assert_eq!(
            derive_module_id("/users", "get", &json!({"operationId": 42})),
            "users.get"
        );
    }

    // ---- normalize_module_id ----

    #[test]
    fn test_normalize_module_id_spec_worked_examples() {
        for (raw, want) in [
            ("getUserById", "get_user_by_id"),
            ("listPets", "list_pets"),
            ("createPets", "create_pets"),
            ("showPetById", "show_pet_by_id"),
            ("getHTTPResponse", "get_http_response"),
            ("OAuth2Token", "o_auth2_token"),
            ("list-pets", "list_pets"),
        ] {
            assert_eq!(normalize_module_id(raw), want, "input {raw:?}");
        }
    }

    #[test]
    fn test_normalize_module_id_legal_id_unchanged() {
        // FastAPI's default operationIds are legal and registrable as-is;
        // normalisation must not rename them.
        for id in [
            "get_product_product__product_id__get",
            "read_item_items__item_id__get",
            "users.get",
            "a_",
            "a__b.c_",
            "v1.pets.get",
            "x9_.y",
        ] {
            assert_eq!(legality_warning(id), None, "fixture id {id:?} not legal");
            assert_eq!(normalize_module_id(id), id, "legal id {id:?} changed");
        }
    }

    #[test]
    fn test_normalize_module_id_long_capital_run() {
        // The spec pins `([A-Z])([A-Z][a-z])` because the `([A-Z]+)` form is
        // quadratic in backtracking engines; the `regex` crate is linear for
        // both, so this checks only that the `_` lands where the spec says.
        let crafted = format!("{}b", "A".repeat(200_000));
        let expected = format!("{}_ab", "a".repeat(199_999));
        assert_eq!(normalize_module_id(&crafted), expected);
    }

    #[test]
    fn test_normalize_module_id_acronym_boundaries() {
        for (raw, want) in [
            ("getUserID", "get_user_id"),
            // A known ugly output, pinned rather than special-cased.
            ("getIDs", "get_i_ds"),
            ("ABCDef", "abc_def"),
            ("HTTPSProxyURLParser", "https_proxy_url_parser"),
            ("XMLHttpRequest", "xml_http_request"),
            ("ID", "id"),
            ("Users.GetUser", "users.get_user"),
        ] {
            assert_eq!(normalize_module_id(raw), want, "input {raw:?}");
        }
    }

    #[test]
    fn test_normalize_module_id_digits() {
        for (raw, want) in [
            ("v2Items", "v2_items"),
            ("item2", "item2"),
            ("get2FA", "get2_fa"),
            ("2fa", "2fa"),
            ("v1.2fa.post", "v1.2fa.post"),
        ] {
            assert_eq!(normalize_module_id(raw), want, "input {raw:?}");
        }
    }

    #[test]
    fn test_normalize_module_id_underscores_and_dots() {
        for (raw, want) in [
            // Only leading `_` is stripped; runs and trailing `_` stay.
            ("__private__", "private__"),
            ("a__b", "a__b"),
            ("a  b", "a__b"),
            ("getUser_", "get_user_"),
            ("get_User", "get_user"),
            ("_a_b__.c_", "a_b__.c_"),
            // Empty segments are dropped.
            ("x..y", "x.y"),
            ("._a_._", "a_"),
            ("..", ""),
            ("", ""),
            ("---", ""),
        ] {
            assert_eq!(normalize_module_id(raw), want, "input {raw:?}");
        }
    }

    #[test]
    fn test_normalize_module_id_non_ascii_never_survives() {
        for (raw, want) in [
            // U+212A KELVIN SIGN lowercases to ASCII `k` under Unicode rules;
            // lowercasing only after the alphabet step keeps it out.
            ("get\u{212A}elvin", "get_elvin"),
            ("\u{212A}", ""),
            // U+017F LATIN SMALL LETTER LONG S: a case-insensitive `[a-z]`
            // would accept it.
            ("\u{17F}ort", "ort"),
            // U+0130 LATIN CAPITAL LETTER I WITH DOT ABOVE.
            ("\u{130}stanbul", "stanbul"),
            // U+0663 ARABIC-INDIC DIGIT THREE: `\d` would accept it.
            ("v\u{663}", "v_"),
            ("caf\u{E9}", "caf_"),
            // Astral: one code point, so exactly one `_` (two UTF-16 code
            // units, which a non-Unicode JavaScript regex would turn into
            // `__`).
            ("\u{1F600}emoji", "emoji"),
            ("a\u{1F600}b", "a_b"),
        ] {
            assert_eq!(normalize_module_id(raw), want, "input {raw:?}");
        }
    }

    #[test]
    fn test_normalize_module_id_output_alphabet_and_idempotence() {
        let inputs = [
            "getUserById",
            "getHTTPResponse",
            "OAuth2Token",
            "getIDs",
            "list-pets",
            "Custom-Space.GetThing",
            "Pet-Store.users.get",
            "__private__",
            "x..y",
            "a b/c{d}",
            "get\u{212A}elvin",
            "\u{1F600}emoji",
            "v1.2fa.post",
            "already_snake.case",
            "getUser_",
            "__a__.__b__",
            "caf\u{E9}",
            "",
        ];
        for raw in inputs {
            let once = normalize_module_id(raw);
            assert_eq!(normalize_module_id(&once), once, "not idempotent: {raw:?}");
            assert!(
                once.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.'),
                "{raw:?} -> {once:?} left the alphabet"
            );
            if !once.is_empty() {
                assert!(
                    once.split('.')
                        .all(|segment| !segment.is_empty() && !segment.starts_with('_')),
                    "{raw:?} -> {once:?} has an empty or `_`-leading segment"
                );
            }
        }
    }

    // ---- legality_warning ----

    #[test]
    fn test_legality_warning_none_for_legal_ids() {
        for id in [
            "users.get",
            "get_user_by_id",
            "v1.pets.get",
            "a",
            "a1_b2.c3",
        ] {
            assert_eq!(legality_warning(id), None, "id {id:?}");
        }
    }

    #[test]
    fn test_legality_warning_names_first_illegal_segment() {
        assert_eq!(
            legality_warning("v1.2fa.post").as_deref(),
            Some(
                "module_id 'v1.2fa.post' is not a legal apcore module ID: segment '2fa' must match ^[a-z][a-z0-9_]*$; name this operation with a derive_module_id or transform_module hook"
            )
        );
        assert_eq!(
            legality_warning("3ds.4ds").as_deref(),
            Some(
                "module_id '3ds.4ds' is not a legal apcore module ID: segment '3ds' must match ^[a-z][a-z0-9_]*$; name this operation with a derive_module_id or transform_module hook"
            )
        );
    }

    #[test]
    fn test_legality_warning_empty_id_names_empty_segment() {
        assert_eq!(
            legality_warning("").as_deref(),
            Some(
                "module_id '' is not a legal apcore module ID: segment '' must match ^[a-z][a-z0-9_]*$; name this operation with a derive_module_id or transform_module hook"
            )
        );
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

    fn spec_with_paths(paths: Value) -> Value {
        json!({
            "openapi": "3.0.3",
            "info": {"title": "t", "version": "1.0.0"},
            "paths": paths
        })
    }

    fn ok_response() -> Value {
        json!({"200": {"description": "ok"}})
    }

    fn scan_ids(spec: &Value, options: &ScanOptions) -> Vec<String> {
        block_on(OpenAPIScanner::new().scan(spec, options))
            .unwrap()
            .into_iter()
            .map(|m| m.module_id)
            .collect()
    }

    #[test]
    fn test_scan_operation_id_normalised_raw_value_kept_in_metadata() {
        let spec = spec_with_paths(json!({
            "/pets": {"get": {"operationId": "listPets", "responses": ok_response()}}
        }));
        let modules = block_on(OpenAPIScanner::new().scan(&spec, &ScanOptions::new())).unwrap();
        assert_eq!(modules[0].module_id, "list_pets");
        assert!(modules[0].warnings.is_empty());
        assert_eq!(
            modules[0].metadata["openapi"]["operation_id"],
            json!("listPets")
        );
    }

    #[test]
    fn test_scan_prefix_normalised_with_id() {
        let spec = spec_with_paths(json!({"/users": {"get": {"responses": ok_response()}}}));
        let options = ScanOptions {
            base_path_prefix: Some("Pet-Store".to_string()),
            ..ScanOptions::new()
        };
        assert_eq!(scan_ids(&spec, &options), vec!["pet_store.users.get"]);
    }

    #[test]
    fn test_scan_derive_hook_output_normalised() {
        let spec = spec_with_paths(json!({"/widgets": {"get": {"responses": ok_response()}}}));
        let options = ScanOptions {
            derive_module_id: Some(Box::new(|_, _, _| {
                Some("Custom-Space.GetThing".to_string())
            })),
            ..ScanOptions::new()
        };
        assert_eq!(scan_ids(&spec, &options), vec!["custom_space.get_thing"]);
    }

    #[test]
    fn test_scan_legal_hook_id_never_rewritten() {
        // A trailing `_` is stripped only from an `operationId`; the final
        // normalisation leaves a legal ID a hook chose exactly as it is.
        let spec = spec_with_paths(json!({"/widgets": {"get": {"responses": ok_response()}}}));
        let options = ScanOptions {
            derive_module_id: Some(Box::new(|_, _, _| Some("abc_.x__y_".to_string()))),
            ..ScanOptions::new()
        };
        let modules = block_on(OpenAPIScanner::new().scan(&spec, &options)).unwrap();
        assert_eq!(modules[0].module_id, "abc_.x__y_");
        assert!(modules[0].warnings.is_empty());
    }

    #[test]
    fn test_scan_transform_module_output_normalised() {
        let spec = spec_with_paths(json!({"/widgets": {"get": {"responses": ok_response()}}}));
        let options = ScanOptions {
            transform_module: Some(Box::new(|mut module| {
                module.module_id = format!("Renamed-By.{}", module.module_id);
                Some(module)
            })),
            ..ScanOptions::new()
        };
        assert_eq!(scan_ids(&spec, &options), vec!["renamed_by.widgets.get"]);
    }

    #[test]
    fn test_scan_include_matches_normalised_id() {
        let spec = spec_with_paths(json!({
            "/users/{id}": {"get": {"operationId": "getUserById", "responses": ok_response()}},
            "/pets": {"get": {"operationId": "listPets", "responses": ok_response()}}
        }));
        let options = ScanOptions {
            include: Some("^get_user_by_id$".to_string()),
            ..ScanOptions::new()
        };
        assert_eq!(scan_ids(&spec, &options), vec!["get_user_by_id"]);
    }

    #[test]
    fn test_scan_collision_created_by_normalisation_resolved_by_dedup() {
        let spec = spec_with_paths(json!({
            "/a": {"get": {"operationId": "listPets", "responses": ok_response()}},
            "/b": {"get": {"operationId": "list-pets", "responses": ok_response()}}
        }));
        assert_eq!(
            scan_ids(&spec, &ScanOptions::new()),
            vec!["list_pets", "list_pets_2"]
        );
    }

    #[test]
    fn test_scan_illegal_id_emitted_with_legality_warning() {
        let spec = spec_with_paths(json!({"/v1/2fa": {"post": {"responses": ok_response()}}}));
        let modules = block_on(OpenAPIScanner::new().scan(&spec, &ScanOptions::new())).unwrap();
        assert_eq!(modules.len(), 1, "an illegal ID is still emitted");
        assert_eq!(modules[0].module_id, "v1.2fa.post");
        assert_eq!(
            modules[0].warnings,
            vec![
                "module_id 'v1.2fa.post' is not a legal apcore module ID: segment '2fa' must match ^[a-z][a-z0-9_]*$; name this operation with a derive_module_id or transform_module hook"
            ]
        );
    }

    #[test]
    fn test_scan_hook_empty_id_emitted_with_legality_warning() {
        let spec = spec_with_paths(json!({"/widgets": {"get": {"responses": ok_response()}}}));
        let options = ScanOptions {
            derive_module_id: Some(Box::new(|_, _, _| Some("--".to_string()))),
            ..ScanOptions::new()
        };
        let modules = block_on(OpenAPIScanner::new().scan(&spec, &options)).unwrap();
        assert_eq!(modules[0].module_id, "");
        assert_eq!(
            modules[0].warnings,
            vec![
                "module_id '' is not a legal apcore module ID: segment '' must match ^[a-z][a-z0-9_]*$; name this operation with a derive_module_id or transform_module hook"
            ]
        );
    }

    #[test]
    fn test_scan_legality_warning_names_deduplicated_id_after_rename_warning() {
        let spec = spec_with_paths(json!({
            "/a": {"get": {"operationId": "3ds", "responses": ok_response()}},
            "/b": {"get": {"operationId": "3ds", "responses": ok_response()}}
        }));
        let modules = block_on(OpenAPIScanner::new().scan(&spec, &ScanOptions::new())).unwrap();
        assert_eq!(modules[0].module_id, "3ds");
        assert_eq!(
            modules[0].warnings,
            vec![
                "module_id '3ds' is not a legal apcore module ID: segment '3ds' must match ^[a-z][a-z0-9_]*$; name this operation with a derive_module_id or transform_module hook"
            ]
        );
        assert_eq!(modules[1].module_id, "3ds_2");
        assert_eq!(
            modules[1].warnings,
            vec![
                "Module ID renamed from '3ds' to '3ds_2' to avoid collision",
                "module_id '3ds_2' is not a legal apcore module ID: segment '3ds_2' must match ^[a-z][a-z0-9_]*$; name this operation with a derive_module_id or transform_module hook",
            ]
        );
    }

    #[test]
    fn test_scan_legality_not_checked_on_filtered_out_modules() {
        let spec = spec_with_paths(json!({
            "/v1/2fa": {"post": {"responses": ok_response()}},
            "/users": {"get": {"responses": ok_response()}}
        }));
        let options = ScanOptions {
            exclude: Some("2fa".to_string()),
            ..ScanOptions::new()
        };
        let modules = block_on(OpenAPIScanner::new().scan(&spec, &options)).unwrap();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].module_id, "users.get");
        assert!(modules[0].warnings.is_empty());
    }

    #[test]
    fn test_scan_fastapi_operation_id_passes_through_unchanged() {
        let spec = spec_with_paths(json!({
            "/items/{item_id}": {"get": {
                "operationId": "read_item_items__item_id__get",
                "responses": ok_response()
            }}
        }));
        let modules = block_on(OpenAPIScanner::new().scan(&spec, &ScanOptions::new())).unwrap();
        assert_eq!(modules[0].module_id, "read_item_items__item_id__get");
        assert!(modules[0].warnings.is_empty());
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
