// All-caps acronyms (AIEnhancer, HTTPProxy*, YAML*, JSON*) are intentional:
// these names match the Python and TypeScript SDK public surfaces for cross-language
// parity. Clippy's C-CASE recommendation is waived for this crate.
#![allow(clippy::upper_case_acronyms)]

//! apcore-toolkit — Shared scanner, schema extraction, and output toolkit.
//!
//! Rust implementation — tri-language parity with Python and TypeScript.
//!
//! The crate version is exported as [`VERSION`] to match the `__version__`
//! / `VERSION` symbols in the Python and TypeScript SDKs.
//!
//! # Language-writer parity
//!
//! Python ships `PythonWriter`, TypeScript ships `TypeScriptWriter`, and the
//! Rust SDK ships [`RustWriter`] for generating language-native handler stubs.
//! The generated files are starting-point stubs; Rust consumers may also work
//! directly with the strongly-typed `ScannedModule` / registry APIs.
//!
//! [`RustWriter`]: crate::output::rust_writer::RustWriter
//!
//! # Crate-root re-exports
//!
//! The HTTP verb helpers and `SCANNER_VERB_MAP` are exported directly at
//! the crate root. As of v0.5.0 they are no longer re-exported through the
//! `scanner` module path — import from the crate root or `http_verb_map`:
//!
//! ```
//! use apcore_toolkit::{generate_suggested_alias, has_path_params, resolve_http_verb, SCANNER_VERB_MAP};
//!
//! assert_eq!(resolve_http_verb("POST", false), "create");
//! assert!(has_path_params("/tasks/{id}"));
//! assert_eq!(SCANNER_VERB_MAP.get("POST").copied(), Some("create"));
//! assert_eq!(
//!     generate_suggested_alias("/tasks/user_data", "POST"),
//!     "tasks.user_data.create"
//! );
//! ```

/// Crate version, read from `Cargo.toml` at compile time.
///
/// Mirrors Python's `apcore_toolkit.__version__` and TypeScript's
/// `VERSION` export so all three SDKs expose the current toolkit version
/// via a public symbol.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod ai_enhancer;
#[cfg(feature = "device-auth")]
pub mod auth;
pub mod binding_loader;
pub mod conformance;
pub mod display;
pub mod formatting;
pub mod http_verb_map;
pub mod openapi;
pub mod openapi_scanner;
pub mod output;
pub mod resolve_target;
pub mod scanner;
pub mod schema_utils;
pub mod serializers;
pub mod tui_view_model;
pub mod types;

// Re-export primary types at crate root for convenience.
pub use ai_enhancer::{AIEnhancer, AIEnhancerError, Enhancer};
pub use binding_loader::{
    match_binding_pattern, validate_binding_pattern, BindingLoadError, BindingLoader,
    DEFAULT_BINDING_PATTERN,
};
pub use conformance::assert_annotations_preserved;
pub use display::{DisplayResolver, DisplayResolverError};
pub use formatting::{
    format_csv, format_jsonl, format_module, format_modules, format_schema, to_markdown,
    FormatError, FormatOutput, GroupBy, MarkdownError, MarkdownOptions, ModuleStyle, SchemaStyle,
};
pub use http_verb_map::{
    extract_path_param_names, generate_suggested_alias, has_path_params, resolve_http_verb,
    substitute_path_params, SCANNER_VERB_MAP,
};
pub use openapi::{
    deep_resolve_refs, extract_input_schema, extract_output_schema, resolve_ref, resolve_schema,
};
pub use openapi_scanner::{
    derive_module_id, DeriveModuleIdHook, OpenAPIScanner, ScanOptions, ScannerError,
    TransformModuleHook, TransformOperationHook,
};
pub use output::errors::WriteError;
pub use output::registry_writer::{HandlerFactory, HandlerFn, RegistryWriter};
pub use output::rust_writer::RustWriter;
pub use output::types::{Verifier, VerifyResult, WriteResult};
pub use output::verifiers::{
    run_verifier_chain, JSONVerifier, MagicBytesVerifier, RegistryVerifier, SyntaxVerifier,
    YAMLVerifier,
};
pub use output::yaml_writer::YAMLWriter;
pub use output::{get_writer, InvalidFormatError, OutputFormat};
pub use resolve_target::{resolve_target, ResolveTargetError, ResolvedTarget};
pub use scanner::{deduplicate_ids, filter_modules, infer_annotations_from_method, BaseScanner};
pub use schema_utils::enrich_schema_descriptions;
pub use serializers::{annotations_to_dict, module_to_dict, modules_to_dicts};
pub use tui_view_model::{
    format_view_model, modules_to_view_model, Cell, Column, Direction, Exposure, Filter, Group,
    Justify, ModulesToViewModelOptions, Sort, Tone, TonePalette, ToneRule, TuiViewModel, View,
    ViewGroupBy,
};
pub use types::{clone_module, create_scanned_module, ScannedModule};

#[cfg(feature = "http-proxy")]
pub use openapi_scanner::{load_spec, load_spec_with_options, LoadSpecError, LoadSpecOptions};
#[cfg(feature = "http-proxy")]
pub use output::http_proxy_writer::{HTTPProxyRegistryWriter, HTTPProxyRegistryWriterError};

// RFC 8628 device authorization flow. Only the names a consumer needs to build
// and drive a flow are lifted to the crate root; the rest stay reachable under
// `apcore_toolkit::auth::*`, matching how `output::*` is exported.
#[cfg(feature = "device-auth")]
pub use auth::{
    DeviceAuthClient, DeviceAuthConfig, DeviceAuthError, DeviceCodeGrant, FileTokenStore, Grant,
    LoginCallbacks, PollEvent, TokenSet, TokenStore, TokenStoreError, UserCodeEvent,
};
