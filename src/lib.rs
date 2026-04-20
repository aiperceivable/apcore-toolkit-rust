//! apcore-toolkit — Shared scanner, schema extraction, and output toolkit.
//!
//! Rust implementation — tri-language parity with Python and TypeScript (v0.5.0).
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

pub mod ai_enhancer;
pub mod binding_loader;
pub mod display;
pub mod formatting;
pub mod http_verb_map;
pub mod openapi;
pub mod output;
pub mod resolve_target;
pub mod scanner;
pub mod schema_utils;
pub mod serializers;
pub mod types;

// Re-export primary types at crate root for convenience.
pub use ai_enhancer::{AIEnhancer, AIEnhancerError, Enhancer};
pub use binding_loader::{BindingLoadError, BindingLoader};
pub use display::{DisplayResolver, DisplayResolverError};
pub use formatting::{to_markdown, MarkdownError, MarkdownOptions};
pub use http_verb_map::{
    generate_suggested_alias, has_path_params, resolve_http_verb, SCANNER_VERB_MAP,
};
pub use openapi::{
    deep_resolve_refs, extract_input_schema, extract_output_schema, resolve_ref, resolve_schema,
};
pub use output::errors::WriteError;
pub use output::registry_writer::{HandlerFactory, HandlerFn, RegistryWriter};
pub use output::types::{Verifier, VerifyResult, WriteResult};
pub use output::verifiers::{
    run_verifier_chain, JSONVerifier, MagicBytesVerifier, RegistryVerifier, SyntaxVerifier,
    YAMLVerifier,
};
pub use output::yaml_writer::YAMLWriter;
pub use output::{get_writer, OutputFormat, OutputFormatError};
pub use resolve_target::{resolve_target, ResolveTargetError, ResolvedTarget};
pub use scanner::{deduplicate_ids, filter_modules, infer_annotations_from_method, BaseScanner};
pub use schema_utils::enrich_schema_descriptions;
pub use serializers::{annotations_to_dict, module_to_dict, modules_to_dicts};
pub use types::ScannedModule;

#[cfg(feature = "http-proxy")]
pub use output::http_proxy_writer::{HTTPProxyRegistryWriter, HTTPProxyWriterError};
