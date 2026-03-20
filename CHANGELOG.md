# Changelog

All notable changes to this project will be documented in this file.

## [0.3.0] - 2026-03-20

Initial release. Rust port of [apcore-toolkit-python](https://github.com/aipartnerup/apcore-toolkit-python) v0.3.0.

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
