// ScannedModule serialization utilities.
//
// Pure functions with no framework dependency. Convert ScannedModule instances
// to serde_json::Value suitable for JSON/YAML output or API responses.

use serde_json::{json, Value};
use tracing::warn;

use apcore::module::ModuleAnnotations;

use crate::types::ScannedModule;

/// Convert annotations to a JSON Value, handling both present and absent forms.
///
/// Returns `Value::Null` if annotations is `None` or serialization fails.
pub fn annotations_to_value(annotations: Option<&ModuleAnnotations>) -> Value {
    match annotations {
        Some(ann) => serde_json::to_value(ann).unwrap_or_else(|e| {
            warn!("Failed to serialize ModuleAnnotations: {e}");
            Value::Null
        }),
        None => Value::Null,
    }
}

/// Convert a ScannedModule to a JSON Value with all fields.
pub fn module_to_value(module: &ScannedModule) -> Value {
    let examples = serde_json::to_value(&module.examples).unwrap_or_else(|e| {
        warn!(
            module_id = %module.module_id,
            "Failed to serialize examples: {e}"
        );
        json!([])
    });

    json!({
        "module_id": module.module_id,
        "description": module.description,
        "documentation": module.documentation,
        "tags": module.tags,
        "version": module.version,
        "target": module.target,
        "annotations": annotations_to_value(module.annotations.as_ref()),
        "examples": examples,
        "metadata": module.metadata,
        "input_schema": module.input_schema,
        "output_schema": module.output_schema,
        "warnings": module.warnings,
    })
}

/// Batch-convert a list of ScannedModules to Values.
pub fn modules_to_values(modules: &[ScannedModule]) -> Vec<Value> {
    modules.iter().map(module_to_value).collect()
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
    fn test_annotations_to_value_none() {
        assert_eq!(annotations_to_value(None), Value::Null);
    }

    #[test]
    fn test_annotations_to_value_some() {
        let ann = ModuleAnnotations {
            readonly: true,
            ..Default::default()
        };
        let val = annotations_to_value(Some(&ann));
        assert_eq!(val["readonly"], true);
    }

    #[test]
    fn test_module_to_value() {
        let m = sample_module();
        let val = module_to_value(&m);
        assert_eq!(val["module_id"], "users.get");
        assert_eq!(val["description"], "Get user");
        assert_eq!(val["version"], "1.0.0");
        assert_eq!(val["target"], "app:get_user");
        assert!(val["tags"].is_array());
    }

    #[test]
    fn test_modules_to_values() {
        let modules = vec![sample_module(), sample_module()];
        let values = modules_to_values(&modules);
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn test_module_to_value_with_annotations() {
        let mut m = sample_module();
        m.annotations = Some(ModuleAnnotations {
            destructive: true,
            ..Default::default()
        });
        let val = module_to_value(&m);
        assert_eq!(val["annotations"]["destructive"], true);
    }

    #[test]
    fn test_module_to_value_all_keys() {
        let val = module_to_value(&sample_module());
        let obj = val.as_object().unwrap();
        let expected_keys: std::collections::HashSet<&str> = [
            "module_id",
            "description",
            "documentation",
            "tags",
            "version",
            "target",
            "annotations",
            "examples",
            "metadata",
            "input_schema",
            "output_schema",
            "warnings",
        ]
        .into_iter()
        .collect();

        let actual_keys: std::collections::HashSet<&str> = obj.keys().map(|k| k.as_str()).collect();

        assert_eq!(actual_keys, expected_keys);
    }

    #[test]
    fn test_module_to_value_warnings_empty_default() {
        let val = module_to_value(&sample_module());
        let warnings = val["warnings"].as_array().unwrap();
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_module_to_value_with_documentation() {
        let mut m = sample_module();
        m.documentation = Some("Detailed documentation".into());
        let val = module_to_value(&m);
        assert_eq!(val["documentation"], "Detailed documentation");
    }

    #[test]
    fn test_module_to_value_examples_empty_default() {
        let val = module_to_value(&sample_module());
        let examples = val["examples"].as_array().unwrap();
        assert!(examples.is_empty());
    }

    #[test]
    fn test_modules_to_values_empty() {
        let values = modules_to_values(&[]);
        assert!(values.is_empty());
    }
}
