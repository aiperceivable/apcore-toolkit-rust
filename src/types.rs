// ScannedModule — canonical representation of a scanned endpoint.
//
// Unified superset of framework-specific module definitions.
// Web-specific fields (http_method, url_rule) are stored in the `metadata`
// map rather than as top-level fields, keeping the struct domain-agnostic.

use std::collections::HashMap;

use apcore::module::{ModuleAnnotations, ModuleExample};
use serde::{Deserialize, Serialize};

/// Result of scanning a single endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannedModule {
    /// Unique module identifier (e.g., "users.get_user.get").
    pub module_id: String,
    /// Human-readable description for MCP tool listing.
    pub description: String,
    /// JSON Schema dict for module input.
    pub input_schema: serde_json::Value,
    /// JSON Schema dict for module output.
    pub output_schema: serde_json::Value,
    /// Categorization tags.
    pub tags: Vec<String>,
    /// Callable reference in "module.path:callable" format.
    pub target: String,
    /// Module version string.
    #[serde(default = "default_version")]
    pub version: String,
    /// Behavioral annotations (readonly, destructive, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ModuleAnnotations>,
    /// Full docstring text for rich descriptions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    /// Example invocations for documentation and testing.
    #[serde(default)]
    pub examples: Vec<ModuleExample>,
    /// Arbitrary key-value data (e.g., http_method, url_rule).
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
    /// Non-fatal issues encountered during scanning.
    #[serde(default)]
    pub warnings: Vec<String>,
}

fn default_version() -> String {
    "1.0.0".to_string()
}

impl ScannedModule {
    /// Create a new ScannedModule with required fields and sensible defaults.
    pub fn new(
        module_id: String,
        description: String,
        input_schema: serde_json::Value,
        output_schema: serde_json::Value,
        tags: Vec<String>,
        target: String,
    ) -> Self {
        Self {
            module_id,
            description,
            input_schema,
            output_schema,
            tags,
            target,
            version: "1.0.0".to_string(),
            annotations: None,
            documentation: None,
            examples: Vec::new(),
            metadata: HashMap::new(),
            warnings: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_scanned_module_new_defaults() {
        let m = ScannedModule::new(
            "users.get_user".into(),
            "Get a user by ID".into(),
            json!({"type": "object", "properties": {"user_id": {"type": "integer"}}}),
            json!({"type": "object", "properties": {"name": {"type": "string"}}}),
            vec!["users".into()],
            "myapp.views:get_user".into(),
        );
        assert_eq!(m.version, "1.0.0");
        assert!(m.annotations.is_none());
        assert!(m.documentation.is_none());
        assert!(m.examples.is_empty());
        assert!(m.metadata.is_empty());
        assert!(m.warnings.is_empty());
    }

    #[test]
    fn test_scanned_module_serde_roundtrip() {
        let m = ScannedModule::new(
            "tasks.create".into(),
            "Create task".into(),
            json!({"type": "object"}),
            json!({"type": "object"}),
            vec!["tasks".into()],
            "myapp:create_task".into(),
        );
        let json_str = serde_json::to_string(&m).unwrap();
        let deserialized: ScannedModule = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deserialized.module_id, "tasks.create");
        assert_eq!(deserialized.version, "1.0.0");
    }

    #[test]
    fn test_scanned_module_with_annotations() {
        let mut m = ScannedModule::new(
            "items.delete".into(),
            "Delete item".into(),
            json!({}),
            json!({}),
            vec![],
            "app:delete_item".into(),
        );
        m.annotations = Some(ModuleAnnotations {
            destructive: true,
            ..Default::default()
        });
        assert!(m.annotations.as_ref().unwrap().destructive);
        assert!(!m.annotations.as_ref().unwrap().readonly);
    }

    #[test]
    fn test_mutable_defaults_independent() {
        let mut a = ScannedModule::new(
            "a".into(),
            "A".into(),
            json!({}),
            json!({}),
            vec![],
            "app:a".into(),
        );
        let b = ScannedModule::new(
            "b".into(),
            "B".into(),
            json!({}),
            json!({}),
            vec![],
            "app:b".into(),
        );
        a.tags.push("modified".into());
        a.metadata.insert("key".into(), json!("value"));

        assert!(b.tags.is_empty());
        assert!(b.metadata.is_empty());
    }

    #[test]
    fn test_with_all_fields() {
        use apcore::module::ModuleExample;

        let m = ScannedModule {
            module_id: "full.module".into(),
            description: "Fully populated module".into(),
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            tags: vec!["tag1".into(), "tag2".into()],
            target: "app:full".into(),
            version: "2.0.0".into(),
            annotations: Some(ModuleAnnotations {
                readonly: true,
                ..Default::default()
            }),
            documentation: Some("Full documentation string".into()),
            examples: vec![ModuleExample {
                title: "Example 1".into(),
                description: Some("An example".into()),
                inputs: json!({"x": 1}),
                output: json!({"y": 2}),
            }],
            metadata: {
                let mut map = HashMap::new();
                map.insert("http_method".into(), json!("GET"));
                map
            },
            warnings: vec!["a warning".into()],
        };

        assert_eq!(m.module_id, "full.module");
        assert_eq!(m.version, "2.0.0");
        assert!(m.annotations.is_some());
        assert!(m.documentation.is_some());
        assert_eq!(m.examples.len(), 1);
        assert_eq!(m.metadata.len(), 1);
        assert_eq!(m.warnings.len(), 1);
    }

    #[test]
    fn test_field_count() {
        let mut m = ScannedModule::new(
            "count.check".into(),
            "Check field count".into(),
            json!({}),
            json!({}),
            vec![],
            "app:count".into(),
        );
        // Populate optional fields so they are included in serialization.
        m.annotations = Some(ModuleAnnotations::default());
        m.documentation = Some("doc".into());

        let val = serde_json::to_value(&m).unwrap();
        let obj = val.as_object().unwrap();
        assert_eq!(
            obj.len(),
            12,
            "ScannedModule should have exactly 12 fields, got {}",
            obj.len()
        );
    }

    #[test]
    fn test_default_version() {
        let json_str = r#"{
            "module_id": "test",
            "description": "test",
            "input_schema": {},
            "output_schema": {},
            "tags": [],
            "target": "app:test"
        }"#;
        let m: ScannedModule = serde_json::from_str(json_str).unwrap();
        assert_eq!(m.version, "1.0.0");
    }
}
