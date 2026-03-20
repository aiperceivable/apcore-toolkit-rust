// apcore-toolkit — Shared scanner, schema extraction, and output toolkit
// Rust port of apcore-toolkit-python v0.3.0

pub mod ai_enhancer;
pub mod formatting;
pub mod openapi;
pub mod output;
pub mod scanner;
pub mod schema_utils;
pub mod serializers;
pub mod types;

// Re-export primary types at crate root for convenience.
pub use ai_enhancer::{AIEnhancer, AIEnhancerError, Enhancer};
pub use formatting::to_markdown;
pub use openapi::{extract_input_schema, extract_output_schema, resolve_ref, resolve_schema};
pub use output::errors::WriteError;
pub use output::registry_writer::{HandlerFactory, HandlerFn, RegistryWriter};
pub use output::types::{Verifier, VerifyResult, WriteResult};
pub use output::verifiers::{
    run_verifier_chain, JSONVerifier, MagicBytesVerifier, RegistryVerifier, YAMLVerifier,
};
pub use output::yaml_writer::YAMLWriter;
pub use output::{get_writer, OutputFormat};
pub use scanner::{deduplicate_ids, filter_modules, infer_annotations_from_method, Scanner};
pub use schema_utils::enrich_schema_descriptions;
pub use serializers::{annotations_to_value, module_to_value, modules_to_values};
pub use types::ScannedModule;

#[cfg(feature = "http-proxy")]
pub use output::http_proxy_writer::HTTPProxyRegistryWriter;
