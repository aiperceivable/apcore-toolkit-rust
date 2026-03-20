// Registry writer for direct module registration.
//
// Converts ScannedModule instances into apcore Module implementations
// and registers them directly into an apcore Registry.
//
// Framework adapters provide a `HandlerFactory` to resolve targets to real
// async handlers. Without a factory, modules are registered with a passthrough
// handler that echoes inputs (useful for schema-only registration).

use std::pin::Pin;
use std::sync::Arc;

use tracing::debug;

use apcore::context::Context;
use apcore::errors::ModuleError;
use apcore::Registry;

use crate::output::types::{Verifier, WriteResult};
use crate::output::verifiers::{run_verifier_chain, RegistryVerifier};
use crate::types::ScannedModule;

/// Async handler function type for registered modules.
pub type HandlerFn = Arc<
    dyn for<'a> Fn(
            serde_json::Value,
            &'a Context<serde_json::Value>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<serde_json::Value, ModuleError>>
                    + Send
                    + 'a,
            >,
        > + Send
        + Sync,
>;

/// Factory that resolves a `target` string to an async handler.
///
/// Framework adapters implement this to map target strings (e.g., `"myapp:get_user"`)
/// to actual handler functions. For example, an Axum adapter might look up the
/// handler in a route table; a generic adapter might use a dynamic dispatch map.
///
/// ```ignore
/// let factory: HandlerFactory = Arc::new(|target: &str| {
///     let handler = lookup_handler(target);
///     Some(Arc::new(move |inputs, _ctx| {
///         let h = handler.clone();
///         Box::pin(async move { h.call(inputs).await })
///     }))
/// });
/// let writer = RegistryWriter::with_handler_factory(factory);
/// ```
pub type HandlerFactory = Arc<dyn Fn(&str) -> Option<HandlerFn> + Send + Sync>;

/// Registers ScannedModule instances directly into an apcore Registry.
///
/// This is the default writer used when no output_format is specified.
/// Instead of writing files, it registers modules directly for immediate use.
///
/// ## Handler Resolution
///
/// By default (`RegistryWriter::new()`), modules are registered with a passthrough
/// handler that returns inputs unchanged — useful for schema-only registration
/// where execution is handled elsewhere.
///
/// For executable modules, use `RegistryWriter::with_handler_factory(factory)` to
/// provide a [`HandlerFactory`] that resolves target strings to real handlers.
pub struct RegistryWriter {
    handler_factory: Option<HandlerFactory>,
}

impl Default for RegistryWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryWriter {
    /// Create a RegistryWriter with passthrough handlers (schema-only registration).
    pub fn new() -> Self {
        Self {
            handler_factory: None,
        }
    }

    /// Create a RegistryWriter with a custom handler factory for target resolution.
    pub fn with_handler_factory(factory: HandlerFactory) -> Self {
        Self {
            handler_factory: Some(factory),
        }
    }
}

impl RegistryWriter {
    /// Register scanned modules into the registry.
    ///
    /// - `registry`: The apcore Registry to register modules into.
    /// - `dry_run`: If true, skip registration and return results only.
    /// - `verify`: If true, verify modules are retrievable after registration.
    /// - `verifiers`: Optional custom verifiers run after the built-in check.
    pub fn write(
        &self,
        modules: &[ScannedModule],
        registry: &mut Registry,
        dry_run: bool,
        verify: bool,
        verifiers: Option<&[&dyn Verifier]>,
    ) -> Vec<WriteResult> {
        let mut results: Vec<WriteResult> = Vec::new();

        for module in modules {
            if dry_run {
                results.push(WriteResult::new(module.module_id.clone()));
                continue;
            }

            let fm = self.to_function_module(module);
            // Register with a descriptor
            let descriptor = apcore::registry::registry::ModuleDescriptor {
                name: module.module_id.clone(),
                annotations: module.annotations.clone().unwrap_or_default(),
                input_schema: module.input_schema.clone(),
                output_schema: module.output_schema.clone(),
                enabled: true,
                tags: module.tags.clone(),
                dependencies: vec![],
            };
            if let Err(e) = registry.register(&module.module_id, Box::new(fm), descriptor) {
                results.push(WriteResult::failed(
                    module.module_id.clone(),
                    None,
                    format!("Registration failed: {e}"),
                ));
                continue;
            }
            debug!("Registered module: {}", module.module_id);

            let mut result = WriteResult::new(module.module_id.clone());
            if verify {
                result = verify_registry(&result, &module.module_id, registry);
            }
            if result.verified {
                if let Some(vs) = verifiers {
                    let chain_result = run_verifier_chain(vs, "", &module.module_id);
                    if !chain_result.ok {
                        result = WriteResult::failed(
                            result.module_id,
                            result.path,
                            chain_result.error.unwrap_or_default(),
                        );
                    }
                }
            }
            results.push(result);
        }

        results
    }
}

impl RegistryWriter {
    /// Convert a ScannedModule to an apcore FunctionModule.
    ///
    /// If a handler factory is configured and resolves the target, uses the
    /// resolved handler. Otherwise falls back to a passthrough handler that
    /// returns inputs unchanged.
    fn to_function_module(&self, module: &ScannedModule) -> apcore::decorator::FunctionModule {
        let annotations = module.annotations.clone().unwrap_or_default();
        let input_schema = module.input_schema.clone();
        let output_schema = module.output_schema.clone();

        // Try to resolve the target via the handler factory
        if let Some(factory) = &self.handler_factory {
            if let Some(handler) = factory(&module.target) {
                return apcore::decorator::FunctionModule::new::<_, ()>(
                    annotations,
                    input_schema,
                    output_schema,
                    move |inputs: serde_json::Value,
                          ctx: &Context<serde_json::Value>|
                          -> Pin<
                        Box<
                            dyn std::future::Future<Output = Result<serde_json::Value, ModuleError>>
                                + Send
                                + '_,
                        >,
                    > { handler(inputs, ctx) },
                );
            }
        }

        // Fallback: passthrough handler (schema-only registration)
        fn passthrough<'a>(
            inputs: serde_json::Value,
            _ctx: &'a Context<serde_json::Value>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<serde_json::Value, ModuleError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(inputs) })
        }

        apcore::decorator::FunctionModule::new::<_, ()>(
            annotations,
            input_schema,
            output_schema,
            passthrough,
        )
    }
}

/// Verify that a module was successfully registered and is retrievable.
fn verify_registry(result: &WriteResult, module_id: &str, registry: &Registry) -> WriteResult {
    let verifier = RegistryVerifier::new(registry);
    let vr = verifier.verify("", module_id);
    if vr.ok {
        result.clone()
    } else {
        WriteResult::failed(module_id.into(), None, vr.error.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_module() -> ScannedModule {
        ScannedModule::new(
            "users.get".into(),
            "Get user".into(),
            json!({"type": "object"}),
            json!({"type": "object"}),
            vec!["users".into()],
            "app:get_user".into(),
        )
    }

    #[test]
    fn test_write_dry_run() {
        let writer = RegistryWriter::new();
        let mut registry = Registry::new();
        let modules = vec![sample_module()];
        let results = writer.write(&modules, &mut registry, true, false, None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].module_id, "users.get");
        assert!(!registry.has("users.get"));
    }

    #[test]
    fn test_write_registers_module() {
        let writer = RegistryWriter::new();
        let mut registry = Registry::new();
        let modules = vec![sample_module()];
        let results = writer.write(&modules, &mut registry, false, false, None);
        assert_eq!(results.len(), 1);
        assert!(registry.has("users.get"));
    }

    #[test]
    fn test_write_with_verify() {
        let writer = RegistryWriter::new();
        let mut registry = Registry::new();
        let modules = vec![sample_module()];
        let results = writer.write(&modules, &mut registry, false, true, None);
        assert_eq!(results.len(), 1);
        assert!(results[0].verified);
    }

    #[test]
    fn test_write_empty_list() {
        let writer = RegistryWriter::new();
        let mut registry = Registry::new();
        let results = writer.write(&[], &mut registry, false, false, None);
        assert!(results.is_empty());
    }

    #[test]
    fn test_write_multiple_modules() {
        let writer = RegistryWriter::new();
        let mut registry = Registry::new();
        let modules = vec![
            ScannedModule::new(
                "mod.a".into(),
                "A".into(),
                json!({"type": "object"}),
                json!({"type": "object"}),
                vec![],
                "app:a".into(),
            ),
            ScannedModule::new(
                "mod.b".into(),
                "B".into(),
                json!({"type": "object"}),
                json!({"type": "object"}),
                vec![],
                "app:b".into(),
            ),
        ];
        let results = writer.write(&modules, &mut registry, false, false, None);
        assert_eq!(results.len(), 2);
        assert!(registry.has("mod.a"));
        assert!(registry.has("mod.b"));
        assert!(results[0].verified);
        assert!(results[1].verified);
    }
}
