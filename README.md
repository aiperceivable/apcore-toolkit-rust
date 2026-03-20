<div align="center">
  <img src="https://raw.githubusercontent.com/aipartnerup/apcore-toolkit/main/apcore-toolkit-logo.svg" alt="apcore-toolkit logo" width="200"/>
</div>

# apcore-toolkit-rust

Rust implementation of the [apcore-toolkit](https://github.com/aipartnerup/apcore-toolkit).

Shared scanner, schema extraction, and output toolkit for apcore framework adapters. Ported from [apcore-toolkit-python](https://github.com/aipartnerup/apcore-toolkit-python) v0.3.0.

## Installation

```toml
[dependencies]
apcore-toolkit = { git = "https://github.com/aipartnerup/apcore-toolkit-rust" }

# Optional: HTTP proxy writer
apcore-toolkit = { git = "https://github.com/aipartnerup/apcore-toolkit-rust", features = ["http-proxy"] }
```

## Core Modules

| Module | Description |
|--------|-------------|
| `ScannedModule` | Canonical struct representing a scanned endpoint |
| `Scanner` | Async trait for framework scanners, generic over `App` type (e.g., `Scanner<axum::Router>`) |
| `filter_modules` | Regex-based include/exclude filtering for scanned modules |
| `deduplicate_ids` | Resolves duplicate module IDs by appending `_2`, `_3`, etc. |
| `infer_annotations_from_method` | Maps HTTP methods to behavioral `ModuleAnnotations` |
| `YAMLWriter` | Generates `.binding.yaml` files for `apcore::BindingLoader` |
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
| `enrich_schema_descriptions` | Merges descriptions into JSON Schema properties |
| `get_writer` | Factory function mapping format strings to `OutputFormat` variants |

## Usage

### Scanning and Writing

```rust
use async_trait::async_trait;
use apcore_toolkit::{Scanner, ScannedModule, YAMLWriter, filter_modules, deduplicate_ids};
use serde_json::json;

struct MyScanner;

#[async_trait]
impl Scanner<()> for MyScanner {
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
use apcore_toolkit::{Scanner, ScannedModule};
use async_trait::async_trait;

struct AxumScanner;

#[async_trait]
impl Scanner<axum::Router> for AxumScanner {
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

## Requirements

- Rust edition 2021
- apcore >= 0.13.0
- See `Cargo.toml` for full dependency list

## Features

| Feature | Description |
|---------|-------------|
| `http-proxy` | Enables `HTTPProxyRegistryWriter` (adds `reqwest` dependency) |

## Documentation

Full documentation is available at [https://github.com/aipartnerup/apcore-toolkit](https://github.com/aipartnerup/apcore-toolkit).

## License

Apache-2.0
