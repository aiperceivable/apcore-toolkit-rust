<div align="center">
  <img src="https://raw.githubusercontent.com/aiperceivable/apcore-toolkit/main/apcore-toolkit-logo.svg" alt="apcore-toolkit logo" width="200"/>
</div>

# apcore-toolkit

Rust toolkit for building [APCore](https://github.com/aiperceivable/apcore-rust) framework integrations. Scan web framework endpoints (Axum, Actix, Rocket, etc.), extract JSON Schemas, infer behavioral annotations, and output APCore-compatible module definitions — as YAML binding files, direct registry entries, or HTTP proxy modules.

If you're building an APCore adapter for a Rust web framework, this crate provides all the shared infrastructure so you only need to write the framework-specific scanning logic.

## Installation

```toml
[dependencies]
apcore-toolkit = { git = "https://github.com/aiperceivable/apcore-toolkit-rust" }

# Optional: HTTP proxy writer
apcore-toolkit = { git = "https://github.com/aiperceivable/apcore-toolkit-rust", features = ["http-proxy"] }
```

## Core Modules

| Module | Description |
|--------|-------------|
| `ScannedModule` | Canonical struct representing a scanned endpoint |
| `BaseScanner<App>` | Async trait for framework scanners, generic over the `App` type with default `()` (e.g., `BaseScanner<axum::Router>`) |
| `filter_modules` | Regex-based include/exclude filtering for scanned modules |
| `deduplicate_ids` | Resolves duplicate module IDs by appending `_2`, `_3`, etc. |
| `infer_annotations_from_method` | Maps HTTP methods to behavioral `ModuleAnnotations` |
| `YAMLWriter` | Generates `.binding.yaml` files for `apcore::BindingLoader` |
| `BindingLoader` | Parses `.binding.yaml` files back into `ScannedModule` values (pure-data inverse of `YAMLWriter`, with loose/strict modes) |
| `BindingLoadError` | `thiserror`-derived error enum: `PathNotFound`, `FileRead`, `FileTooLarge`, `TooManyFiles`, `YamlParse`, `MissingFields`, `InvalidStructure` |
| `RegistryWriter` | Registers modules directly into an `apcore::Registry` with pluggable `HandlerFactory` |
| `HTTPProxyRegistryWriter` | Registers HTTP proxy modules that forward requests to a running API (feature: `http-proxy`) |
| `Enhancer` | Pluggable trait for metadata enhancement |
| `AIEnhancer` | SLM-based metadata enhancement for scanned modules |
| `WriteResult` | Structured result type for all writer operations |
| `WriteError` | Typed error (via `thiserror`) for I/O failures during write |
| `Verifier` | Pluggable trait for validating written artifacts |
| `VerifyResult` | Result type for verification operations |
| `YAMLVerifier` | Verifies YAML files parse correctly with required fields |
| `RegistryVerifier` | Verifies modules are registered and retrievable |
| `MagicBytesVerifier` | Verifies file headers match expected magic bytes |
| `JSONVerifier` | Verifies JSON files parse correctly |
| `to_markdown` | Converts JSON objects to Markdown with depth control and table heuristics |
| `format_csv` _(v0.7.0)_ | Byte-equivalent RFC 4180 CSV emitter — header = union of keys; canonical JSON for nested cells; CRLF terminator |
| `format_jsonl` _(v0.7.0)_ | Byte-equivalent JSON Lines emitter — canonical compact JSON per row, LF terminator |
| `enrich_schema_descriptions` | Merges descriptions into JSON Schema properties |
| `DisplayResolver` | Sparse binding.yaml overlay — resolves alias, description, guidance, tags into `metadata["display"]` |
| `SyntaxVerifier` | Verifies Rust source files parse without syntax errors (via `syn`) |
| `deep_resolve_refs` | Recursively resolves all `$ref` pointers in a JSON Schema (depth-limited to 16) |
| `resolve_target` | Validates and parses `module_path:qualname` target strings |
| `get_writer` | Factory function mapping format strings to `OutputFormat` variants |

## Usage

### Scanning and Writing

```rust
use async_trait::async_trait;
use apcore_toolkit::{BaseScanner, ScannedModule, YAMLWriter, filter_modules, deduplicate_ids};
use serde_json::json;

struct MyScanner;

#[async_trait]
impl BaseScanner<()> for MyScanner {
    async fn scan(&self, _app: &()) -> Vec<ScannedModule> {
        vec![
            ScannedModule::new(
                "users.get_user".into(),
                "Get a user by ID".into(),
                json!({"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}),
                json!({"type": "object", "properties": {"name": {"type": "string"}}}),
                vec!["users".into()],
                "myapp:get_user".into(),
            )
        ]
    }

    fn source_name(&self) -> &str {
        "my-framework"
    }
}

#[tokio::main]
async fn main() {
    let scanner = MyScanner;
    let modules = scanner.scan(&()).await;

    // Filter and deduplicate
    let modules = filter_modules(&modules, Some(r"^users\."), None).unwrap();
    let modules = deduplicate_ids(modules);

    // Write YAML binding files
    let writer = YAMLWriter;
    writer.write(&modules, "./bindings", false, false, None).unwrap();
}
```

### Framework-Specific Scanner (Axum Example)

```rust,ignore
use apcore_toolkit::{BaseScanner, ScannedModule};
use async_trait::async_trait;

struct AxumScanner;

#[async_trait]
impl BaseScanner<axum::Router> for AxumScanner {
    async fn scan(&self, app: &axum::Router) -> Vec<ScannedModule> {
        // Extract routes from Axum router and convert to ScannedModule instances
        todo!("Implement route extraction")
    }

    fn source_name(&self) -> &str { "axum" }
}
```

### Direct Registry Registration

```rust
use apcore::Registry;
use apcore_toolkit::RegistryWriter;

let mut registry = Registry::new();
let writer = RegistryWriter::new();
writer.write(&modules, &mut registry, false, false, None);
```

### Registry with Custom Handler Factory

```rust,ignore
use std::sync::Arc;
use apcore_toolkit::{RegistryWriter, HandlerFactory};

let factory: HandlerFactory = Arc::new(|target: &str| {
    let handler = lookup_handler(target)?;
    Some(Arc::new(move |inputs, _ctx| {
        let h = handler.clone();
        Box::pin(async move { h.call(inputs).await })
    }))
});

let writer = RegistryWriter::with_handler_factory(factory);
writer.write(&modules, &mut registry, false, false, None);
```

### Output Format Factory

```rust
use apcore_toolkit::{get_writer, OutputFormat};

let format = get_writer("yaml");       // Some(OutputFormat::Yaml)
let format = get_writer("registry");   // Some(OutputFormat::Registry)
let format = get_writer("http-proxy"); // Some(OutputFormat::HttpProxy)
```

### OpenAPI Schema Extraction

```rust
use apcore_toolkit::{extract_input_schema, extract_output_schema};

let input_schema = extract_input_schema(&operation, Some(&openapi_doc));
let output_schema = extract_output_schema(&operation, Some(&openapi_doc));
```

### Schema Enrichment

```rust
use std::collections::HashMap;
use apcore_toolkit::enrich_schema_descriptions;

let mut descs = HashMap::new();
descs.insert("user_id".into(), "The user ID".into());
let enriched = enrich_schema_descriptions(&schema, &descs, false);
```

### Markdown Formatting

```rust
use apcore_toolkit::formatting::{to_markdown, MarkdownOptions};
use serde_json::json;

let data = json!({"name": "Alice", "role": "admin"});
let opts = MarkdownOptions {
    title: Some("User Info".into()),
    ..Default::default()
};
let md = to_markdown(&data, &opts).unwrap();
```

### Tabular Formats (v0.7.0)

Byte-equivalent CSV / JSONL emitters with a cross-SDK conformance contract — Rust, Python, and TypeScript produce identical bytes for the same input.

```rust
use apcore_toolkit::{format_csv, format_jsonl};
use serde_json::{json, Map, Value};

let rows: Vec<Map<String, Value>> = vec![
    json!({"sn": 1, "title": "First", "score": 78}).as_object().unwrap().clone(),
    json!({"sn": 2, "title": "Second", "score": 82, "description": "later-only field"})
        .as_object().unwrap().clone(),
];

// CSV: header = union of keys across all rows (no silent data loss on
// heterogeneous rows); nested values serialized as canonical compact JSON;
// RFC 4180 CRLF line terminator.
print!("{}", format_csv(&rows, /*bom=*/ false));
// sn,title,score,description\r\n1,First,78,\r\n2,Second,82,later-only field\r\n

// JSONL: canonical compact JSON per row, LF terminator, no trailing blank.
print!("{}", format_jsonl(&rows));

// UTF-8 BOM for Excel locales (default off for pipeline consumers):
print!("{}", format_csv(&rows, true));
```

> **Note** — this crate enables the `serde_json/preserve_order` feature, required for canonical insertion-order key emission. Transitively affects ALL `serde_json::Map` iteration in your dependency tree; downstream code that relied on alphabetical iteration must re-sort explicitly.

See `apcore-toolkit/docs/features/formatting.md` § Tabular Formats for the full contract and `apcore-toolkit/conformance/fixtures/format_csv.json` / `format_jsonl.json` for the shared cross-SDK test corpus.

## Requirements

- See `Cargo.toml` for full dependency list

## Features

| Feature | Description |
|---------|-------------|
| `http-proxy` | Enables `HTTPProxyRegistryWriter` (adds `reqwest` dependency) |

## Documentation

Full documentation is available at [https://github.com/aiperceivable/apcore-toolkit](https://github.com/aiperceivable/apcore-toolkit).

## License

Apache-2.0
