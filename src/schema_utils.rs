// JSON Schema enrichment utilities.
//
// Helpers for merging docstring-extracted parameter descriptions into
// JSON Schema `properties`.

use std::collections::HashMap;

use serde_json::Value;

/// Merge parameter descriptions into JSON Schema properties.
///
/// By default, only fills in *missing* descriptions — existing ones are
/// preserved. Set `overwrite` to `true` to replace existing descriptions.
///
/// Returns a **new** Value with descriptions merged in. The original
/// schema is not mutated.
pub fn enrich_schema_descriptions(
    schema: &Value,
    param_descriptions: &HashMap<String, String>,
    overwrite: bool,
) -> Value {
    if param_descriptions.is_empty() {
        return schema.clone();
    }

    match schema.get("properties") {
        Some(p) if p.is_object() => p,
        _ => return schema.clone(),
    };

    let mut result = schema.clone();

    if let Some(result_props) = result.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for (name, desc) in param_descriptions {
            if let Some(prop) = result_props.get_mut(name) {
                if let Some(prop_obj) = prop.as_object_mut() {
                    if overwrite || !prop_obj.contains_key("description") {
                        prop_obj.insert("description".to_string(), Value::String(desc.clone()));
                    }
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_enrich_adds_missing_descriptions() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "age": {"type": "integer"}
            }
        });
        let mut descs = HashMap::new();
        descs.insert("name".into(), "The user's name".into());
        descs.insert("age".into(), "The user's age".into());

        let result = enrich_schema_descriptions(&schema, &descs, false);
        assert_eq!(
            result["properties"]["name"]["description"],
            "The user's name"
        );
        assert_eq!(result["properties"]["age"]["description"], "The user's age");
    }

    #[test]
    fn test_enrich_preserves_existing_descriptions() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Original"}
            }
        });
        let mut descs = HashMap::new();
        descs.insert("name".into(), "New description".into());

        let result = enrich_schema_descriptions(&schema, &descs, false);
        assert_eq!(result["properties"]["name"]["description"], "Original");
    }

    #[test]
    fn test_enrich_overwrites_when_flag_set() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Original"}
            }
        });
        let mut descs = HashMap::new();
        descs.insert("name".into(), "Overwritten".into());

        let result = enrich_schema_descriptions(&schema, &descs, true);
        assert_eq!(result["properties"]["name"]["description"], "Overwritten");
    }

    #[test]
    fn test_enrich_empty_descriptions_returns_original() {
        let schema = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        let descs = HashMap::new();
        let result = enrich_schema_descriptions(&schema, &descs, false);
        assert_eq!(result, schema);
    }

    #[test]
    fn test_enrich_no_properties_returns_original() {
        let schema = json!({"type": "string"});
        let mut descs = HashMap::new();
        descs.insert("x".into(), "desc".into());
        let result = enrich_schema_descriptions(&schema, &descs, false);
        assert_eq!(result, schema);
    }

    #[test]
    fn test_enrich_ignores_unknown_params() {
        let schema = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}}
        });
        let mut descs = HashMap::new();
        descs.insert("unknown_field".into(), "desc".into());
        let result = enrich_schema_descriptions(&schema, &descs, false);
        assert!(!result["properties"]
            .as_object()
            .unwrap()
            .contains_key("unknown_field"));
    }

    #[test]
    fn test_enrich_does_not_mutate_original() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "age": {"type": "integer"}
            }
        });
        let original = schema.clone();
        let mut descs = HashMap::new();
        descs.insert("name".into(), "A name".into());
        descs.insert("age".into(), "An age".into());

        let result = enrich_schema_descriptions(&schema, &descs, false);
        // Result should have descriptions
        assert_eq!(result["properties"]["name"]["description"], "A name");
        // Original must be unchanged
        assert_eq!(schema, original, "original schema must not be mutated");
        assert!(
            schema["properties"]["name"]
                .as_object()
                .unwrap()
                .get("description")
                .is_none(),
            "original should not have description added"
        );
    }

    #[test]
    fn test_enrich_partial_match() {
        // Only some params match schema properties; others are ignored
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "email": {"type": "string"}
            }
        });
        let mut descs = HashMap::new();
        descs.insert("name".into(), "User name".into());
        descs.insert("nonexistent".into(), "Should be ignored".into());

        let result = enrich_schema_descriptions(&schema, &descs, false);
        // Matching property gets description
        assert_eq!(result["properties"]["name"]["description"], "User name");
        // Non-matching property stays untouched (no description added)
        assert!(
            result["properties"]["email"]
                .as_object()
                .unwrap()
                .get("description")
                .is_none(),
            "email should not get a description"
        );
        // Nonexistent param should not create a new property
        assert!(
            !result["properties"]
                .as_object()
                .unwrap()
                .contains_key("nonexistent"),
            "nonexistent param should not appear in properties"
        );
    }
}
