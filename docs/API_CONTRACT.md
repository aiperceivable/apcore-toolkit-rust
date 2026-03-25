# API Contract — apcore-toolkit-rust

Ported from `apcore-toolkit-python` v0.3.0. This document defines the complete public API surface.

---

## 1. Core Types (`src/types.rs`)

### ScannedModule

Canonical representation of a scanned endpoint.

```rust
pub struct ScannedModule {
    pub module_id: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub tags: Vec<String>,
    pub target: String,
    pub version: String,                           // default: "1.0.0"
    pub annotations: Option<ModuleAnnotations>,     // from apcore crate
    pub documentation: Option<String>,
    pub examples: Vec<ModuleExample>,               // from apcore crate
    pub metadata: HashMap<String, serde_json::Value>,
    pub warnings: Vec<String>,
}
```

- Derives: `Debug, Clone, Serialize, Deserialize`
- Must have a builder or `new()` with required fields + defaults for optional ones

---

## 2. Scanner Trait (`src/scanner.rs`)

### Scanner (trait)

Abstract interface for framework scanners. Equivalent to Python's `BaseScanner` ABC.

```rust
#[async_trait]
pub trait Scanner<App: Send + Sync = ()> {
    async fn scan(&self, app: &App) -> Vec<ScannedModule>;
    fn source_name(&self) -> &str;
}
```

### Free functions on scanner module

```rust
pub fn filter_modules(modules: &[ScannedModule], include: Option<&str>, exclude: Option<&str>) -> Result<Vec<ScannedModule>, regex::Error>;
pub fn deduplicate_ids(modules: Vec<ScannedModule>) -> Vec<ScannedModule>;
pub fn infer_annotations_from_method(method: &str) -> ModuleAnnotations;
```

---

## 3. Output Types (`src/output/types.rs`)

### VerifyResult

```rust
pub struct VerifyResult {
    pub ok: bool,
    pub error: Option<String>,
}
```

### Verifier (trait)

```rust
pub trait Verifier {
    fn verify(&self, path: &str, module_id: &str) -> VerifyResult;
}
```

### WriteResult

```rust
pub struct WriteResult {
    pub module_id: String,
    pub path: Option<String>,
    pub verified: bool,                // default: true
    pub verification_error: Option<String>,
}
```

---

## 4. Output Errors (`src/output/errors.rs`)

### WriteError

```rust
#[derive(Debug, thiserror::Error)]
pub struct WriteError {
    pub path: String,
    pub cause: String,
}
```

---

## 5. Verifiers (`src/output/verifiers.rs`)

### YAMLVerifier

Validates YAML syntax and required binding fields (`bindings` list, `module_id`, `target`).

### JSONVerifier

Validates JSON syntax. Optional schema validation (feature-gated or param-based).

### MagicBytesVerifier

Validates file starts with expected byte sequence.

### RegistryVerifier

Validates module is retrievable from a registry.

### run_verifier_chain

```rust
pub fn run_verifier_chain(verifiers: &[&dyn Verifier], path: &str, module_id: &str) -> VerifyResult;
```

---

## 6. YAML Writer (`src/output/yaml_writer.rs`)

### YAMLWriter

```rust
pub struct YAMLWriter;

impl YAMLWriter {
    pub fn write(
        &self,
        modules: &[ScannedModule],
        output_dir: &str,
        dry_run: bool,
        verify: bool,
        verifiers: Option<&[&dyn Verifier]>,
    ) -> Result<Vec<WriteResult>, WriteError>;
}
```

- Sanitizes filenames (replace non-alphanumeric with `_`, collapse `..` to `_`)
- Path traversal protection
- Auto-creates output directory
- Generates `.binding.yaml` files with header comment

---

## 7. Registry Writer (`src/output/registry_writer.rs`)

### RegistryWriter

```rust
pub struct RegistryWriter { /* private fields */ }

impl RegistryWriter {
    pub fn new() -> Self;
    pub fn with_handler_factory(factory: HandlerFactory) -> Self;
    pub fn write(
        &self,
        modules: &[ScannedModule],
        registry: &mut Registry,
        dry_run: bool,
        verify: bool,
        verifiers: Option<&[&dyn Verifier]>,
    ) -> Vec<WriteResult>;
}

impl Default for RegistryWriter { /* ... */ }
```

---

## 8. HTTP Proxy Writer (`src/output/http_proxy_writer.rs`)

Feature-gated behind `http-proxy`.

### HTTPProxyRegistryWriter

```rust
pub struct HTTPProxyRegistryWriter {
    base_url: String,
    auth_header_factory: Option<Arc<dyn Fn() -> HashMap<String, String> + Send + Sync>>,
    timeout_secs: f64,
}

impl HTTPProxyRegistryWriter {
    pub fn new(
        base_url: String,
        auth_header_factory: Option<Box<dyn Fn() -> HashMap<String, String> + Send + Sync>>,
        timeout_secs: f64,
    ) -> Self;
    pub fn write(&self, modules: &[ScannedModule], registry: &mut Registry) -> Vec<WriteResult>;
}
```

---

## 9. Writer Factory (`src/output/mod.rs`)

```rust
pub enum OutputFormat {
    Yaml,
    Registry,
    HttpProxy,
}

pub fn get_writer(format: &str) -> Option<OutputFormat>;
```

Note: Python's `PythonWriter` generates Python code — not applicable in Rust. The Rust port omits it.

---

## 10. OpenAPI Utilities (`src/openapi.rs`)

```rust
pub fn resolve_ref(ref_string: &str, openapi_doc: &serde_json::Value) -> serde_json::Value;
pub fn resolve_schema(schema: &serde_json::Value, openapi_doc: Option<&serde_json::Value>) -> serde_json::Value;
pub fn extract_input_schema(operation: &serde_json::Value, openapi_doc: Option<&serde_json::Value>) -> serde_json::Value;
pub fn extract_output_schema(operation: &serde_json::Value, openapi_doc: Option<&serde_json::Value>) -> serde_json::Value;
```

Public: `deep_resolve_refs(schema, openapi_doc, depth)` — recursively resolves all `$ref` pointers with depth limit of 16.

---

## 11. Schema Utilities (`src/schema_utils.rs`)

```rust
pub fn enrich_schema_descriptions(
    schema: &serde_json::Value,
    param_descriptions: &HashMap<String, String>,
    overwrite: bool,
) -> serde_json::Value;
```

Returns a new Value; does not mutate the original.

---

## 12. Serializers (`src/serializers.rs`)

```rust
pub fn annotations_to_value(annotations: Option<&ModuleAnnotations>) -> serde_json::Value;
pub fn module_to_value(module: &ScannedModule) -> serde_json::Value;
pub fn modules_to_values(modules: &[ScannedModule]) -> Vec<serde_json::Value>;
```

---

## 13. Formatting — Markdown (`src/formatting/markdown.rs`)

```rust
pub struct MarkdownOptions {
    pub fields: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub max_depth: usize,           // default: 3
    pub table_threshold: usize,     // default: 5
    pub title: Option<String>,
}

pub fn to_markdown(data: &serde_json::Value, options: &MarkdownOptions) -> Result<String, ...>;
```

---

## 14. AI Enhancer (`src/ai_enhancer.rs`)

### Enhancer (trait)

```rust
pub trait Enhancer {
    fn enhance(&self, modules: Vec<ScannedModule>) -> Vec<ScannedModule>;
}
```

### AIEnhancer

```rust
pub struct AIEnhancer {
    endpoint: String,
    model: String,
    threshold: f64,
    batch_size: usize,
    timeout: u64,
}

impl AIEnhancer {
    pub fn new(
        endpoint: Option<String>,
        model: Option<String>,
        threshold: Option<f64>,
        batch_size: Option<usize>,
        timeout: Option<u64>,
    ) -> Result<Self, AIEnhancerError>;
    pub fn is_enabled() -> bool;           // reads APCORE_AI_ENABLED env var
}

impl Enhancer for AIEnhancer {
    fn enhance(&self, modules: Vec<ScannedModule>) -> Vec<ScannedModule>;
}
```

Environment variables:
- `APCORE_AI_ENABLED`
- `APCORE_AI_ENDPOINT` (default: `http://localhost:11434/v1`)
- `APCORE_AI_MODEL` (default: `qwen:0.6b`)
- `APCORE_AI_THRESHOLD` (default: `0.7`)
- `APCORE_AI_BATCH_SIZE` (default: `5`)
- `APCORE_AI_TIMEOUT` (default: `30`)

---

## 15. DisplayResolver (`src/display/resolver.rs`)

```rust
pub struct DisplayResolver;

impl DisplayResolver {
    pub fn new() -> Self;
    pub fn resolve(
        &self,
        modules: Vec<ScannedModule>,
        binding_path: Option<&Path>,
        binding_data: Option<&Value>,
    ) -> Result<Vec<ScannedModule>, DisplayResolverError>;
}

pub enum DisplayResolverError {
    Validation(String),
}
```

Applies sparse `binding.yaml` overlays to resolve surface-facing alias, description, guidance, and tags into `metadata["display"]`. Supports MCP alias auto-sanitization and CLI alias validation.

---

## 16. SyntaxVerifier (`src/output/verifiers.rs`)

```rust
pub struct SyntaxVerifier;

impl Verifier for SyntaxVerifier {
    fn verify(&self, path: &str, module_id: &str) -> VerifyResult;
}
```

Verifies Rust source files parse without syntax errors using the `syn` crate.

---

## 17. resolve_target (`src/resolve_target.rs`)

```rust
pub struct ResolvedTarget {
    pub module_path: String,
    pub qualname: String,
}

pub fn resolve_target(target: &str) -> Result<ResolvedTarget, String>;
```

Validates and parses `module_path:qualname` target strings. Uses the last `:` as separator.

---

## 18. Omitted from Rust Port

- **PythonWriter** — generates Python source code; not applicable to Rust consumers
- **flatten_pydantic_params** — Pydantic-specific; not applicable

These are Python-language-specific features that have no Rust equivalent.
