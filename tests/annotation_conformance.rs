//! Conformance: the toolkit `RegistryWriter` preserves behavioral annotations
//! through scan -> register -> get_definition. Locks the governance-critical
//! round-trip that approval/ACL gating (which keys on `requires_approval`)
//! depends on — a real `Registry`, not a mock.

use apcore::module::ModuleAnnotations;
use apcore::Registry;
use apcore_toolkit::assert_annotations_preserved;
use apcore_toolkit::types::ScannedModule;
use apcore_toolkit::RegistryWriter;
use serde_json::json;

#[test]
fn base_writer_preserves_annotations() {
    let mut module = ScannedModule::new(
        "orders.delete_order".into(),
        "Delete an order".into(),
        json!({"type": "object"}),
        json!({"type": "object"}),
        vec!["orders".into()],
        "app:delete_order".into(),
    );
    module.annotations = Some(ModuleAnnotations {
        destructive: true,
        requires_approval: true,
        ..Default::default()
    });

    let registry = Registry::new();
    assert_annotations_preserved(&RegistryWriter::new(), &module, &registry);
}
