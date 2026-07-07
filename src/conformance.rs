//! Cross-adapter conformance checks for registry writers.
//!
//! Call from an adapter's tests to assert that registering a scanned module
//! **preserves its behavioral annotations**. Approval and ACL gating key on
//! `requires_approval` (and read `destructive`); a writer that drops
//! annotations during registration silently disables that gate — no error, no
//! warning. This helper turns that otherwise-invisible regression into a
//! failing test.

use apcore::Registry;

use crate::types::ScannedModule;
use crate::RegistryWriter;

/// Register `module` via `writer` into `registry` and assert its governance
/// annotations (`requires_approval`, `destructive`) survive `get_definition`.
///
/// Panics on a dropped or changed field, so it reads naturally inside a
/// `#[test]`. `module.annotations` must be `Some(..)` — that is what the
/// round-trip verifies.
pub fn assert_annotations_preserved(
    writer: &RegistryWriter,
    module: &ScannedModule,
    registry: &Registry,
) {
    let source = module
        .annotations
        .as_ref()
        .expect("assert_annotations_preserved expects module.annotations to be set");

    writer.write(std::slice::from_ref(module), registry, false, false, None);

    let descriptor = registry
        .get_definition(&module.module_id)
        .expect("get_definition failed")
        .unwrap_or_else(|| {
            panic!(
                "conformance: module '{}' was not registered",
                module.module_id
            )
        });

    let registered = descriptor.annotations.as_ref().unwrap_or_else(|| {
        panic!(
            "conformance: module '{}' lost its annotations during registration — \
             approval/ACL gating that keys on requires_approval will silently never fire",
            module.module_id
        )
    });

    assert_eq!(
        registered.requires_approval, source.requires_approval,
        "conformance: module '{}' annotation 'requires_approval' changed during registration",
        module.module_id
    );
    assert_eq!(
        registered.destructive, source.destructive,
        "conformance: module '{}' annotation 'destructive' changed during registration",
        module.module_id
    );
}
