# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

- **Annotation-table cross-SDK alignment** — `format_module(.., ModuleStyle::Markdown | ModuleStyle::Skill, ..)` `## Behavior` table now emits only fields that differ from `ModuleAnnotations::default()`, sorts rows alphabetically by snake_case key (already the natural `serde_json::Map` iteration order under default features), and renders bool values as lowercase `true`/`false`. The section is omitted entirely when every annotation field matches its default. Closes the byte-equality gap with the Python and TypeScript SDKs.
- **Surface-aware formatters** (refs aiperceivable/apcore-toolkit#13) — `format_module`, `format_schema`, `format_modules` for rendering `ScannedModule` and JSON Schema for specific consumer surfaces. Four `ModuleStyle` variants: `Markdown` (LLM context), `Skill` (drop-in `.claude/skills/<id>/SKILL.md` or `.gemini/skills/<id>/SKILL.md` body with minimal `name` + `description` frontmatter — no vendor-specific extensions), `TableRow` (CLI listing), `Json` (programmatic). Three `SchemaStyle` variants: `Prose`, `Table`, `Json`. `format_modules` adds optional `Option<GroupBy>` (`Tag` or `Prefix`). `display: bool` (default true upstream) prefers the `ScannedModule.display` overlay over raw fields. Returns are wrapped in a `FormatOutput` enum with `Text(String) | Value(serde_json::Value) | Values(Vec<Value>)` and `as_str` / `as_value` / `as_values` accessors. Lives in `formatting::surface`; re-exported at the crate root.

### Changed

- **`infer_annotations_from_method` HEAD/OPTIONS canonical mapping** (refs aiperceivable/apcore-toolkit#11) — already produced `readonly=true` for `HEAD` and `OPTIONS` (without `cacheable=true`), aligned with the canonical mapping declared in `apcore-toolkit/docs/features/scanning.md` and now also matched by Python and TypeScript. No code change needed in this SDK; an extra smoke test in `formatting::surface::tests::scanner_head_options_canonical_mapping` cross-references the new spec section.

## [0.5.0] - 2026-04-21

Aligned release across Python, TypeScript, and Rust. Tracks apcore 0.19.0 features (expanded `ModuleAnnotations`, `display` field). The prior `0.5.0-rc.1` pre-release tag has been folded into this entry; the final `0.5.0` ships with the full cross-SDK parity audit completed.

### Added

- **`BindingLoader`** / **`BindingLoadError`** (`apcore_toolkit::binding_loader`) — parses `.binding.yaml` files back into `ScannedModule` objects, the inverse of `YAMLWriter`. Pure-data reader: no target import, no Registry mutation. Matches the Python and TypeScript implementations in API shape and behaviour.
  - `load(path, strict, recursive) -> Result<Vec<ScannedModule>, BindingLoadError>` — single file or directory of `*.binding.yaml`; `recursive=true` descends into subdirectories via `walkdir`.
  - `load_data(data, strict)` — pre-parsed `serde_json::Value`.
  - Loose mode (`strict=false`, default): only `module_id + target` required.
  - Strict mode (`strict=true`): additionally requires `input_schema + output_schema`.
  - `spec_version` validated via `tracing::warn`; missing or unsupported values log but do not fail.
  - `annotations` parsed via `serde_json::from_value::<ModuleAnnotations>`; malformed values degrade to `None` with a warning.
  - `BindingLoadError` enum (`thiserror`-derived) with 7 variants: `PathNotFound`, `FileRead`, `YamlParse`, `MissingFields`, `InvalidStructure`, `FileTooLarge`, `TooManyFiles`.
  - **Safety caps**: `FileTooLarge` (16 MiB per-file limit) and `TooManyFiles` (10,000 files-per-directory limit) bound worst-case memory and traversal cost on untrusted input. The Python and TypeScript loaders do not currently enforce these caps — callers there should pre-validate directories loaded from untrusted sources.
  - Re-exported from crate root: `apcore_toolkit::{BindingLoader, BindingLoadError}`.
- **`ScannedModule.display`** — new optional field (`Option<serde_json::Value>`) for the sparse display overlay. `#[serde(skip_serializing_if = "Option::is_none")]` keeps the wire format clean. Constructor `ScannedModule::new()` initializes to `None`.

### Changed

- **`output::yaml_writer::build_binding`** — emits top-level `display:` key only when `module.display.is_some()`. Refactored from `serde_json::json!` macro to `serde_json::Map` construction to support conditional keys.
- **`serializers::module_to_value`** — includes `display` key.
- **`test_field_count`** updated from 13 → 14 to reflect the new field.
- **`output::registry_writer` & `output::http_proxy_writer`** — `ModuleDescriptor` construction updated for apcore 0.19.0 breaking changes: `display: Option<serde_json::Value>` now required; `annotations` is now `Option<ModuleAnnotations>`; `http_proxy_writer` now populates all descriptor fields (previously partial, missed `description`/`documentation`/`version`/`examples`/`metadata`).

### Changed (breaking)

- **`scanner` module no longer re-exports `http_verb_map` helpers.** Previously `apcore_toolkit::scanner::resolve_http_verb`, `::SCANNER_VERB_MAP`, `::has_path_params`, and `::generate_suggested_alias` were reachable via a `pub use crate::http_verb_map::{...}` inside `src/scanner.rs`. Those re-exports were removed to eliminate the double path. Downstream adapter crates must import from the crate root (`apcore_toolkit::{resolve_http_verb, SCANNER_VERB_MAP, has_path_params, generate_suggested_alias}`) or from `apcore_toolkit::http_verb_map::{...}`. The crate-root re-exports are guarded by a doc test in `src/lib.rs`.

### Dependencies

- **`apcore >= 0.19.0`** — picks up the 12-field `ModuleAnnotations`, `FunctionModule.display`, and the expanded `ModuleDescriptor`. Serde handles new annotation fields automatically.

### Tests

- +27 net new tests (+31 added, −4 removed): 18 for `BindingLoader` parsing + strict/loose modes + spec_version + filesystem loading + round-trip with `YAMLWriter`; 3 for `ScannedModule.display` (default, skip-if-none, serde round-trip); 2 for YAML writer display emission; 3 hardening tests (malformed display warn, null display drop, error message readability); 5 cross-SDK regression tests for strict-mode wrong-type / empty-string rejection and recursive `WalkDir` error surfacing. Removed 4 misleading `test_reexport_*` tests from `scanner::tests` that duplicated `http_verb_map::tests` coverage; added one crate-root re-export doc test in `src/lib.rs`. Total suite: 306 unit tests + 6 doctests.

### Hardening (post-review)

- **`BindingLoader::load`**: directory iteration now surfaces per-entry I/O failures via `BindingLoadError::FileRead` instead of silently discarding them via `filter_map(Result::ok)`. The recursive `WalkDir` branch applies the same policy — permission denied, broken symlinks, and I/O errors are reported as `FileRead` rather than producing a partial result set.
- **`BindingLoadError` `Display`**: `MissingFields` and `InvalidStructure` messages no longer leak `Some("…")` / `None` debug wrappers — they render the inner path/module_id directly with readable fallbacks.
- **Malformed `display` in a binding entry** now emits a `tracing::warn` rather than being silently dropped.
- **`BindingLoader::parse_entry`** — wrong-type required fields (e.g. `module_id: 42`, `target: true`, empty-string `module_id`) are now surfaced as `BindingLoadError::MissingFields` instead of silently coerced to `""`. In strict mode, non-object `input_schema`/`output_schema` are likewise rejected. The error display widens from `"missing or null"` to `"missing or invalid required fields"`. This behaviour is the cross-SDK reference — the Python and TypeScript loaders were updated to match in 0.5.0.
- **`BindingLoader` safety caps** — `FileTooLarge` (16 MiB) and `TooManyFiles` (10,000) `BindingLoadError` variants added to bound memory and traversal cost on untrusted input.
- **`output::registry_writer::RegistryWriter::to_function_module`** — passthrough-handler warning migrated from `eprintln!` to `tracing::warn!(module_id = %…, …)`, honouring the crate's `tracing`-only logging rule.

## [0.4.0] - 2026-03-25

### Added

- **`DisplayResolver`** — sparse `binding.yaml` overlay that resolves surface-facing alias, description, guidance, tags, and documentation into `metadata["display"]`. Supports file/directory binding paths, pre-parsed data, MCP alias sanitization, and CLI alias validation.
- **`SyntaxVerifier`** — implements `Verifier` trait, checks `.rs` files parse without syntax errors via the `syn` crate.
- **`deep_resolve_refs()`** — now public API (was internal). Recursively resolves all `$ref` pointers in OpenAPI schemas, depth-limited to 16 levels.
- **`resolve_target()`** — validates and parses `module_path:qualname` target strings. Returns `ResolvedTarget` with `module_path` and `qualname` fields.

### Fixed

- README: apcore dependency version updated from `>= 0.13.0` to `>= 0.14` (matches Cargo.toml).
- `docs/API_CONTRACT.md`: Scanner trait updated to async with generic `App` parameter; RegistryWriter `new()` and `with_handler_factory()` constructors added; AIEnhancer::new return type corrected to `AIEnhancerError`.

## [0.3.1] - 2026-03-22

### Changed
- Rebrand: aipartnerup → aiperceivable

## [0.3.0] - 2026-03-20

Initial release. Rust port of [apcore-toolkit-python](https://github.com/aiperceivable/apcore-toolkit-python) v0.3.0.

### Added

- `ScannedModule` struct — canonical representation of a scanned endpoint,
  with all 12 fields matching the Python dataclass (serde `Serialize`/`Deserialize`)
- `Scanner` async trait — generic over `App` type parameter for framework-specific
  scanning (e.g., `Scanner<axum::Router>`, `Scanner<actix_web::App>`).
  Uses `#[async_trait]` for async `scan()` method
- `filter_modules()` — regex-based include/exclude filtering with `Result` error
  handling for invalid patterns
- `deduplicate_ids()` — resolves duplicate module IDs by appending `_2`, `_3`, etc.
- `infer_annotations_from_method()` — HTTP method to `ModuleAnnotations` mapping
  (GET → readonly+cacheable, DELETE → destructive, PUT → idempotent)
- `YAMLWriter` — generates `.binding.yaml` files for `apcore::BindingLoader` with
  filename sanitization, path traversal protection, and optional verification
- `RegistryWriter` — registers modules directly into `apcore::Registry` with
  pluggable `HandlerFactory` for target resolution. Falls back to passthrough
  handler for schema-only registration
- `HTTPProxyRegistryWriter` — registers scanned modules as HTTP proxy modules
  that forward requests to a running web API. Supports path parameter substitution,
  pluggable auth headers via `Arc`, and JSON error extraction.
  Feature-gated behind `http-proxy` (`reqwest` dependency)
- `OutputFormat` enum + `get_writer()` factory — maps format strings to writer
  variants (`"yaml"`, `"registry"`, `"http-proxy"`)
- Output verification system:
  - `Verifier` trait — pluggable verification protocol
  - `YAMLVerifier` — validates YAML syntax and required binding fields
  - `JSONVerifier` — validates JSON syntax
  - `MagicBytesVerifier` — validates file header magic bytes
  - `RegistryVerifier` — validates module is retrievable from registry
  - `run_verifier_chain()` — sequential verifier execution with `catch_unwind`
    panic safety (matches Python's `try/except` behavior)
- `WriteResult` / `VerifyResult` — structured result types for all writer and
  verifier operations (serde `Serialize`/`Deserialize`)
- `WriteError` — typed error via `thiserror` for file I/O failures
- OpenAPI utilities:
  - `resolve_ref()` — JSON `$ref` pointer resolution
  - `resolve_schema()` — conditional `$ref` resolution
  - `extract_input_schema()` — merges query/path parameters and request body
  - `extract_output_schema()` — extracts 200/201 response schema
  - `deep_resolve_refs()` (internal) — recursive `$ref` resolution for nested
    schemas (`allOf`/`anyOf`/`oneOf`, `items`, `properties`), depth-limited to 16
- `enrich_schema_descriptions()` — merges parameter descriptions into JSON Schema
  properties with optional overwrite mode
- `annotations_to_value()` / `module_to_value()` / `modules_to_values()` —
  serialization utilities with `tracing::warn!` on serialization failures
- `to_markdown()` — generic JSON-to-Markdown conversion with depth control,
  table heuristics, field/exclude filtering, and UTF-8 safe truncation
- `AIEnhancer` — SLM-based metadata enhancement using local OpenAI-compatible
  APIs (Ollama, vLLM, LM Studio) via `ureq`. Fills missing descriptions,
  documentation, behavioral annotations (all 11 fields), and input schemas.
  All AI-generated fields tagged with `x-generated-by: slm` for auditability.
  Configuration via environment variables (`APCORE_AI_*`) or constructor params
- `Enhancer` trait — pluggable interface for metadata enhancement
- `AIEnhancerError` — typed error enum via `thiserror` with `Config`, `Connection`,
  `Response` variants
- `HandlerFactory` / `HandlerFn` type aliases — enable framework adapters to
  provide real async handlers for registered modules

### Intentionally Omitted (Python-specific)

- `PythonWriter` — generates Python source code; not applicable to Rust consumers
- `SyntaxVerifier` — validates Python AST syntax; not applicable
- `flatten_pydantic_params()` — Pydantic-specific; Rust has no equivalent
- `resolve_target()` — Python `importlib` dynamic import; Rust uses `HandlerFactory`

### Dependencies

- apcore (path dep to `../apcore-rust`) >= 0.13.0
- serde + serde_json + serde_yaml — serialization
- regex — pattern matching
- chrono — timestamps
- tracing — structured logging
- thiserror — domain error types
- async-trait — async trait support
- tokio — required transitively by apcore
- ureq — synchronous HTTP for AI enhancer
- reqwest (optional, `http-proxy` feature) — async HTTP for proxy writer

### Tests

- 176 unit tests + 1 doc-test, all passing
- clippy clean (0 warnings with `-D warnings`)
- All quality gates: `cargo fmt`, `cargo clippy`, `cargo build`, `cargo test`
