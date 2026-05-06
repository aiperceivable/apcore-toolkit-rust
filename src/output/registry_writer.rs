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

use tracing::{debug, warn};

use apcore::context::Context;
use apcore::errors::ModuleError;
use apcore::Registry;

use crate::output::types::{Verifier, WriteResult};
use crate::output::verifiers::{run_verifier_chain, RegistryVerifier};
use crate::types::ScannedModule;

// TODO(release-gate): deep-chain parity with Python/TypeScript RegistryWriter — manual
// review required. RegistryWriter is the primary candidate for missing-registration bugs
// (audit D11 was inconclusive). Verify that all three SDKs perform equivalent registry
// mutations and handle the same error paths before tagging 0.5.0.

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
    /// Optional allow-list of `target` prefixes. When set, any module whose
    /// `target` does not start with one of these prefixes is rejected with a
    /// failed `WriteResult` before any handler factory is invoked. Mirrors the
    /// `allowed_prefixes` parameter on the Python and TypeScript SDKs and
    /// provides a defence-in-depth boundary on dynamically-supplied targets.
    allowed_prefixes: Option<Vec<String>>,
}

impl Default for RegistryWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryWriter {
    /// Create a RegistryWriter with passthrough handlers (schema-only registration).
    ///
    /// # Handler resolution
    ///
    /// Unlike the Python and TypeScript implementations which dynamically import
    /// the target function at write time (`resolve_target`), the Rust implementation
    /// registers a passthrough handler that echoes its inputs when no HandlerFactory
    /// is configured. This means calling a module registered by this writer will
    /// succeed but will not execute real business logic. To register real handlers,
    /// use the HandlerFactory integration.
    ///
    /// # Panics
    ///
    /// This constructor does not panic. However, note that without a `HandlerFactory`,
    /// all registered modules will use a passthrough handler that echoes inputs unchanged.
    /// This is suitable for schema-only registration. For real execution, use
    /// [`RegistryWriter::with_handler_factory`] to supply a factory that resolves targets
    /// to actual async handlers.
    pub fn new() -> Self {
        Self {
            handler_factory: None,
            allowed_prefixes: None,
        }
    }

    /// Create a RegistryWriter with a custom handler factory for target resolution.
    pub fn with_handler_factory(factory: HandlerFactory) -> Self {
        Self {
            handler_factory: Some(factory),
            allowed_prefixes: None,
        }
    }

    /// Restrict registration to modules whose `target` starts with one of the
    /// supplied prefixes. Modules with a non-matching target are rejected with
    /// a failed `WriteResult` and never reach the handler factory.
    ///
    /// Matches the `allowed_prefixes` parameter on the Python `RegistryWriter`
    /// and the TypeScript `allowedPrefixes` option. Use it to bound the set of
    /// callable Python/Rust paths a binding YAML may resolve to (defence in
    /// depth against forged or attacker-controlled `target` strings).
    pub fn with_allowed_prefixes(mut self, prefixes: Vec<String>) -> Self {
        self.allowed_prefixes = Some(prefixes);
        self
    }

    /// Returns `true` when the module target is permitted by the configured
    /// `allowed_prefixes` (or when no allow-list is configured).
    fn target_allowed(&self, target: &str) -> bool {
        match self.allowed_prefixes.as_ref() {
            None => true,
            Some(prefixes) => prefixes.iter().any(|p| target.starts_with(p.as_str())),
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
    ///
    /// # Verifier contract for registry-based modules
    ///
    /// Registry modules have no output file, so custom verifiers receive
    /// `path = ""`. Built-in file-based verifiers (`YAMLVerifier`, `JSONVerifier`,
    /// etc.) skip gracefully when path is empty. Custom verifiers must also
    /// handle `path = ""` without erroring — use `module_id` for any
    /// registry-based checks.
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

            if !self.target_allowed(&module.target) {
                warn!(
                    module_id = %module.module_id,
                    target = %module.target,
                    "RegistryWriter: target rejected by allowed_prefixes"
                );
                results.push(WriteResult::failed(
                    module.module_id.clone(),
                    None,
                    format!(
                        "target '{}' is not in allowed_prefixes — registration refused",
                        module.target
                    ),
                ));
                continue;
            }

            let fm = self.to_function_module(module);
            // Register with a descriptor
            let descriptor = apcore::registry::registry::ModuleDescriptor {
                module_id: module.module_id.clone(),
                name: Some(module.module_id.clone()),
                description: module.description.clone(),
                documentation: module.documentation.clone(),
                input_schema: module.input_schema.clone(),
                output_schema: module.output_schema.clone(),
                version: module.version.clone(),
                tags: module.tags.clone(),
                annotations: module.annotations.clone(),
                examples: module.examples.clone(),
                metadata: module.metadata.clone(),
                display: module.display.clone(),
                sunset_date: None,
                dependencies: vec![],
                enabled: true,
            };
            // Note: unlike Python/TypeScript, Rust collects per-module registration errors
            // rather than aborting. This is intentional — partial registration is preferred
            // over a hard stop, giving callers the opportunity to inspect and handle each failure.
            if let Err(e) = registry.register(&module.module_id, Box::new(fm), descriptor) {
                warn!(
                    module_id = %module.module_id,
                    error = %e,
                    "RegistryWriter registration failed"
                );
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
        debug!(
            module_id = %module.module_id,
            "RegistryWriter using passthrough handler (no HandlerFactory configured)",
        );
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
    fn test_custom_verifier_runs_even_when_verify_false() {
        // D11-011: verify=false skips the built-in registry check, but custom
        // verifiers must still run. A failing custom verifier with verify=false
        // should produce a result with verified=false.
        use crate::output::types::{Verifier, VerifyResult};

        struct AlwaysFail;
        impl Verifier for AlwaysFail {
            fn verify(&self, _path: &str, _module_id: &str) -> VerifyResult {
                VerifyResult::fail("custom verifier failed".into())
            }
        }

        let writer = RegistryWriter::new();
        let mut registry = Registry::new();
        let modules = vec![sample_module()];
        let failing_verifier = AlwaysFail;
        let verifiers: &[&dyn Verifier] = &[&failing_verifier];
        // verify=false: built-in registry check skipped, but custom verifier runs
        let results = writer.write(&modules, &mut registry, false, false, Some(verifiers));
        assert_eq!(results.len(), 1);
        // Module was registered successfully
        assert!(registry.has("users.get"));
        // But custom verifier ran and failed — verified must be false
        assert!(
            !results[0].verified,
            "custom verifier must run even when verify=false; result: {:?}",
            results[0]
        );
        assert!(
            results[0]
                .verification_error
                .as_deref()
                .unwrap_or("")
                .contains("custom verifier failed"),
            "verification_error should contain the custom verifier message"
        );
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

    // D11-2 regression: allowed_prefixes is a defence-in-depth allow-list on
    // the `target` field. A module whose target does not match any prefix
    // must be rejected with a failed WriteResult and never registered.
    #[test]
    fn test_allowed_prefixes_rejects_non_matching_target() {
        let writer =
            RegistryWriter::new().with_allowed_prefixes(vec!["app:".into(), "myapp:".into()]);
        let mut registry = Registry::new();
        let allowed = sample_module(); // target = "app:get_user"
        let denied = ScannedModule::new(
            "evil.module".into(),
            "Forged target".into(),
            json!({"type": "object"}),
            json!({"type": "object"}),
            vec![],
            "evil:run_attacker_code".into(),
        );
        let results = writer.write(&[allowed, denied], &mut registry, false, false, None);
        assert_eq!(results.len(), 2);
        // app:get_user is in allowed_prefixes — registered.
        assert!(registry.has("users.get"));
        assert!(results[0].verified);
        // evil:* is not — rejected, NOT registered.
        assert!(!registry.has("evil.module"));
        assert!(!results[1].verified);
        let err = results[1].verification_error.as_deref().unwrap_or("");
        assert!(
            err.contains("allowed_prefixes"),
            "rejection message should mention allowed_prefixes: got {err:?}"
        );
    }

    #[test]
    fn test_allowed_prefixes_default_none_admits_everything() {
        // Without allowed_prefixes set, target_allowed must return true for
        // every input — preserves existing behaviour for callers that have
        // not opted in.
        let writer = RegistryWriter::new();
        let mut registry = Registry::new();
        let module = ScannedModule::new(
            "any.module".into(),
            "Any target".into(),
            json!({"type": "object"}),
            json!({"type": "object"}),
            vec![],
            "anything-goes:func".into(),
        );
        let results = writer.write(&[module], &mut registry, false, false, None);
        assert_eq!(results.len(), 1);
        assert!(registry.has("any.module"));
    }
}
