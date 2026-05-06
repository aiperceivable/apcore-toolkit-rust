// OpenAPI $ref resolution and schema extraction utilities.
//
// Standalone functions for resolving JSON $ref pointers and extracting
// input/output schemas from OpenAPI operation objects.

use serde_json::{json, Value};
use tracing::warn;

/// Decode a JSON Pointer token per RFC 6901.
///
/// `~1` → `/` and `~0` → `~` (order matters — `~1` must be decoded before `~0`
/// to prevent `~01` from becoming `/` instead of `~1`).
fn decode_pointer_token(token: &str) -> std::borrow::Cow<'_, str> {
    if token.contains('~') {
        std::borrow::Cow::Owned(token.replace("~1", "/").replace("~0", "~"))
    } else {
        std::borrow::Cow::Borrowed(token)
    }
}

/// Resolve a JSON `$ref` pointer like `#/components/schemas/Foo`.
///
/// Decodes RFC 6901 escape sequences in path segments (`~1` → `/`, `~0` → `~`).
/// Returns the resolved schema, or an empty object on failure.
pub fn resolve_ref(ref_string: &str, openapi_doc: &Value) -> Value {
    if !ref_string.starts_with("#/") {
        warn!(
            ref_string,
            "resolve_ref: ignoring non-local $ref (must start with '#/')"
        );
        return json!({});
    }

    let parts: Vec<&str> = ref_string[2..].split('/').collect();
    let mut current = openapi_doc;

    for part in parts {
        let decoded = decode_pointer_token(part);
        match current.get(decoded.as_ref()) {
            Some(next) => current = next,
            None => {
                warn!(
                    ref_string,
                    part, "resolve_ref: path segment not found in document"
                );
                return json!({});
            }
        }
    }

    if current.is_object() {
        current.clone()
    } else {
        warn!(
            ref_string,
            "resolve_ref: resolved value is not an object — returning empty schema"
        );
        json!({})
    }
}

/// If `schema` contains a `$ref`, resolve it; otherwise return as-is.
pub fn resolve_schema(schema: &Value, openapi_doc: Option<&Value>) -> Value {
    if let (Some(doc), Some(ref_str)) = (openapi_doc, schema.get("$ref").and_then(|v| v.as_str())) {
        resolve_ref(ref_str, doc)
    } else {
        schema.clone()
    }
}

/// Recursively resolve all `$ref` pointers in a schema.
///
/// Handles `$ref`, `allOf`, `anyOf`, `oneOf`, `items`, `prefixItems`,
/// `properties`, `patternProperties`, `additionalProperties`, `not`,
/// and `if`/`then`/`else`.
/// Depth-limited: resolves through depth 16 (cuts off at depth > 16),
/// matching the Python and TypeScript implementations.
pub fn deep_resolve_refs(schema: &Value, openapi_doc: &Value, depth: usize) -> Value {
    if depth > 16 {
        warn!(depth, "deep_resolve_refs: depth limit reached — returning schema as-is to prevent infinite recursion");
        return schema.clone();
    }

    // Direct $ref resolution
    if let Some(ref_str) = schema.get("$ref").and_then(|v| v.as_str()) {
        let resolved = resolve_ref(ref_str, openapi_doc);
        return deep_resolve_refs(&resolved, openapi_doc, depth + 1);
    }

    let mut result = schema.clone();

    if let Some(obj) = result.as_object_mut() {
        // Resolve inside allOf/anyOf/oneOf
        for key in &["allOf", "anyOf", "oneOf"] {
            if let Some(Value::Array(items)) = obj.get(*key).cloned() {
                let resolved: Vec<Value> = items
                    .iter()
                    .map(|item| deep_resolve_refs(item, openapi_doc, depth + 1))
                    .collect();
                obj.insert(key.to_string(), Value::Array(resolved));
            }
        }

        // Resolve array items (single schema) and tuple items (array of schemas)
        if let Some(items) = obj.get("items").cloned() {
            if items.is_object() {
                obj.insert(
                    "items".to_string(),
                    deep_resolve_refs(&items, openapi_doc, depth + 1),
                );
            } else if let Value::Array(arr) = items {
                let resolved: Vec<Value> = arr
                    .iter()
                    .map(|item| deep_resolve_refs(item, openapi_doc, depth + 1))
                    .collect();
                obj.insert("items".to_string(), Value::Array(resolved));
            }
        }

        // Resolve prefixItems (JSON Schema 2020-12 tuple items)
        if let Some(Value::Array(prefix)) = obj.get("prefixItems").cloned() {
            let resolved: Vec<Value> = prefix
                .iter()
                .map(|item| deep_resolve_refs(item, openapi_doc, depth + 1))
                .collect();
            obj.insert("prefixItems".to_string(), Value::Array(resolved));
        }

        // Resolve nested properties
        if let Some(Value::Object(props)) = obj.get("properties").cloned() {
            let resolved: serde_json::Map<String, Value> = props
                .into_iter()
                .map(|(k, v)| (k, deep_resolve_refs(&v, openapi_doc, depth + 1)))
                .collect();
            obj.insert("properties".to_string(), Value::Object(resolved));
        }

        // Resolve patternProperties (same shape as properties but keyed by regex)
        if let Some(Value::Object(pat_props)) = obj.get("patternProperties").cloned() {
            let resolved: serde_json::Map<String, Value> = pat_props
                .into_iter()
                .map(|(k, v)| (k, deep_resolve_refs(&v, openapi_doc, depth + 1)))
                .collect();
            obj.insert("patternProperties".to_string(), Value::Object(resolved));
        }

        // Resolve additionalProperties when it is a schema (not a boolean)
        if let Some(add_props) = obj.get("additionalProperties").cloned() {
            if add_props.is_object() {
                obj.insert(
                    "additionalProperties".to_string(),
                    deep_resolve_refs(&add_props, openapi_doc, depth + 1),
                );
            }
        }

        // Resolve not / if / then / else (applicator keywords)
        for key in &["not", "if", "then", "else"] {
            if let Some(sub) = obj.get(*key).cloned() {
                if sub.is_object() {
                    obj.insert(
                        key.to_string(),
                        deep_resolve_refs(&sub, openapi_doc, depth + 1),
                    );
                }
            }
        }
    }

    result
}

/// Extract input schema from an OpenAPI operation.
///
/// Combines query/path parameters and request body properties into a
/// single `{"type": "object", "properties": ..., "required": ...}` schema.
pub fn extract_input_schema(operation: &Value, openapi_doc: Option<&Value>) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();

    // Query/path parameters
    if let Some(Value::Array(params)) = operation.get("parameters") {
        for param in params {
            let in_value = param.get("in").and_then(|v| v.as_str()).unwrap_or("");
            if in_value == "query" || in_value == "path" {
                if let Some(name) = param.get("name").and_then(|v| v.as_str()) {
                    let param_schema = param
                        .get("schema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "string"}));
                    let resolved = resolve_schema(&param_schema, openapi_doc);
                    properties.insert(name.to_string(), resolved);

                    if param
                        .get("required")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                    {
                        required.push(Value::String(name.to_string()));
                    }
                }
            }
        }
    }

    // Request body — try "application/json" first, then "application/vnd.api+json"
    // as a fallback. This matches the Python implementation which iterates all
    // content-type keys and accepts both.
    let body_content = operation
        .get("requestBody")
        .and_then(|rb| rb.get("content"));
    let body_schema_opt = body_content
        .and_then(|c| c.get("application/json"))
        .and_then(|jc| jc.get("schema"))
        .or_else(|| {
            body_content
                .and_then(|c| c.get("application/vnd.api+json"))
                .and_then(|jc| jc.get("schema"))
        });
    if let Some(body_schema) = body_schema_opt {
        let resolved = resolve_schema(body_schema, openapi_doc);
        if let Some(props) = resolved.get("properties").and_then(|p| p.as_object()) {
            for (k, v) in props {
                properties.insert(k.clone(), v.clone());
            }
        }
        if let Some(req) = resolved.get("required").and_then(|r| r.as_array()) {
            required.extend(req.iter().cloned());
        }
    }

    // Recursively resolve $ref inside individual properties
    if let Some(doc) = openapi_doc {
        let resolved_props: serde_json::Map<String, Value> = properties
            .into_iter()
            .map(|(k, v)| (k, deep_resolve_refs(&v, doc, 0)))
            .collect();
        properties = resolved_props;
    }

    // Deduplicate required list while preserving order (params + body can overlap)
    let mut seen = std::collections::HashSet::new();
    required.retain(|v| {
        let key = v.as_str().unwrap_or("").to_string();
        seen.insert(key)
    });

    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": Value::Array(required),
    })
}

/// Extract output schema from OpenAPI operation responses.
///
/// Returns the output JSON Schema, or a default empty object schema.
///
/// Accepts any 2xx status code (200–299), matching the TypeScript implementation
/// which filters on /^2\d\d$/. Codes are checked in lexicographic order so
/// 200 is preferred over 201, 201 over 202, and so on.
pub fn extract_output_schema(operation: &Value, openapi_doc: Option<&Value>) -> Value {
    let responses = match operation.get("responses") {
        Some(r) => r,
        None => return json!({"type": "object", "properties": {}}),
    };

    // Collect all 2xx response keys and sort them so lower codes take priority.
    let responses_obj = match responses.as_object() {
        Some(obj) => obj,
        None => return json!({"type": "object", "properties": {}}),
    };
    let mut success_codes: Vec<&str> = responses_obj
        .keys()
        .filter_map(|k| {
            let k_str = k.as_str();
            if k_str.len() == 3
                && k_str.starts_with('2')
                && k_str.chars().skip(1).all(|c| c.is_ascii_digit())
            {
                Some(k_str)
            } else {
                None
            }
        })
        .collect();
    success_codes.sort();

    for status_code in &success_codes {
        let json_content = responses
            .get(*status_code)
            .and_then(|r| r.get("content"))
            .and_then(|c| {
                c.get("application/json")
                    .or_else(|| c.get("application/vnd.api+json"))
            });
        if let Some(schema) = json_content.and_then(|jc| jc.get("schema")) {
            let mut resolved = resolve_schema(schema, openapi_doc);
            if let Some(doc) = openapi_doc {
                resolved = deep_resolve_refs(&resolved, doc, 0);
            }
            return resolved;
        }
    }

    json!({"type": "object", "properties": {}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_ref_rfc6901_slash_in_key() {
        // Key name "schemas/v2" encoded as "schemas~1v2" in the pointer.
        let doc = json!({
            "schemas/v2": {"type": "string"}
        });
        let result = resolve_ref("#/schemas~1v2", &doc);
        assert_eq!(result["type"], "string");
    }

    #[test]
    fn test_resolve_ref_rfc6901_tilde_in_key() {
        // Key name "a~b" encoded as "a~0b" in the pointer.
        let doc = json!({
            "a~b": {"type": "number"}
        });
        let result = resolve_ref("#/a~0b", &doc);
        assert_eq!(result["type"], "number");
    }

    #[test]
    fn test_resolve_ref_rfc6901_combined_escapes() {
        // "~01" should decode to "~1" (not "/"), per RFC 6901 §3.
        let doc = json!({
            "~1": {"type": "boolean"}
        });
        let result = resolve_ref("#/~01", &doc);
        assert_eq!(result["type"], "boolean");
    }

    #[test]
    fn test_deep_resolve_refs_additional_properties() {
        let doc = json!({
            "components": {
                "schemas": {
                    "Tag": {"type": "string"}
                }
            }
        });
        let schema = json!({
            "type": "object",
            "additionalProperties": {"$ref": "#/components/schemas/Tag"}
        });
        let result = deep_resolve_refs(&schema, &doc, 0);
        assert_eq!(result["additionalProperties"]["type"], "string");
    }

    #[test]
    fn test_deep_resolve_refs_not_keyword() {
        let doc = json!({
            "components": {
                "schemas": {
                    "Forbidden": {"type": "string"}
                }
            }
        });
        let schema = json!({
            "not": {"$ref": "#/components/schemas/Forbidden"}
        });
        let result = deep_resolve_refs(&schema, &doc, 0);
        assert_eq!(result["not"]["type"], "string");
    }

    #[test]
    fn test_deep_resolve_refs_if_then_else() {
        let doc = json!({
            "components": {
                "schemas": {
                    "Condition": {"type": "boolean"},
                    "TrueCase": {"type": "string"},
                    "FalseCase": {"type": "number"}
                }
            }
        });
        let schema = json!({
            "if": {"$ref": "#/components/schemas/Condition"},
            "then": {"$ref": "#/components/schemas/TrueCase"},
            "else": {"$ref": "#/components/schemas/FalseCase"}
        });
        let result = deep_resolve_refs(&schema, &doc, 0);
        assert_eq!(result["if"]["type"], "boolean");
        assert_eq!(result["then"]["type"], "string");
        assert_eq!(result["else"]["type"], "number");
    }

    #[test]
    fn test_extract_input_schema_deduplicates_required() {
        // Both params and body declare the same field as required — should dedup.
        let doc = json!({
            "components": {
                "schemas": {
                    "Body": {
                        "type": "object",
                        "properties": {"id": {"type": "integer"}},
                        "required": ["id"]
                    }
                }
            }
        });
        let op = json!({
            "parameters": [
                {"name": "id", "in": "path", "required": true, "schema": {"type": "integer"}}
            ],
            "requestBody": {
                "content": {
                    "application/json": {
                        "schema": {"$ref": "#/components/schemas/Body"}
                    }
                }
            }
        });
        let result = extract_input_schema(&op, Some(&doc));
        let req = result["required"].as_array().unwrap();
        let id_count = req.iter().filter(|v| v.as_str() == Some("id")).count();
        assert_eq!(id_count, 1, "required list should deduplicate; got {req:?}");
    }

    #[test]
    fn test_resolve_ref_basic() {
        let doc = json!({
            "components": {
                "schemas": {
                    "User": {"type": "object", "properties": {"name": {"type": "string"}}}
                }
            }
        });
        let result = resolve_ref("#/components/schemas/User", &doc);
        assert_eq!(result["type"], "object");
        assert!(result["properties"]["name"].is_object());
    }

    #[test]
    fn test_resolve_ref_not_found() {
        let doc = json!({});
        let result = resolve_ref("#/components/schemas/Missing", &doc);
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_resolve_ref_non_hash() {
        let doc = json!({});
        let result = resolve_ref("external.json#/foo", &doc);
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_resolve_schema_with_ref() {
        let doc = json!({
            "components": {"schemas": {"Foo": {"type": "string"}}}
        });
        let schema = json!({"$ref": "#/components/schemas/Foo"});
        let result = resolve_schema(&schema, Some(&doc));
        assert_eq!(result["type"], "string");
    }

    #[test]
    fn test_resolve_schema_no_ref() {
        let schema = json!({"type": "integer"});
        let result = resolve_schema(&schema, None);
        assert_eq!(result["type"], "integer");
    }

    #[test]
    fn test_extract_input_schema_parameters() {
        let op = json!({
            "parameters": [
                {"name": "user_id", "in": "path", "required": true, "schema": {"type": "integer"}},
                {"name": "limit", "in": "query", "schema": {"type": "integer"}}
            ]
        });
        let result = extract_input_schema(&op, None);
        assert!(result["properties"]["user_id"].is_object());
        assert!(result["properties"]["limit"].is_object());
        let req = result["required"].as_array().unwrap();
        assert!(req.contains(&Value::String("user_id".into())));
        assert!(!req.contains(&Value::String("limit".into())));
    }

    #[test]
    fn test_extract_input_schema_request_body() {
        let op = json!({
            "requestBody": {
                "content": {
                    "application/json": {
                        "schema": {
                            "type": "object",
                            "properties": {"title": {"type": "string"}},
                            "required": ["title"]
                        }
                    }
                }
            }
        });
        let result = extract_input_schema(&op, None);
        assert_eq!(result["properties"]["title"]["type"], "string");
        let req = result["required"].as_array().unwrap();
        assert!(req.contains(&Value::String("title".into())));
    }

    #[test]
    fn test_extract_input_schema_with_ref() {
        let doc = json!({
            "components": {
                "schemas": {
                    "TaskInput": {
                        "type": "object",
                        "properties": {"name": {"type": "string"}},
                        "required": ["name"]
                    }
                }
            }
        });
        let op = json!({
            "requestBody": {
                "content": {
                    "application/json": {
                        "schema": {"$ref": "#/components/schemas/TaskInput"}
                    }
                }
            }
        });
        let result = extract_input_schema(&op, Some(&doc));
        assert_eq!(result["properties"]["name"]["type"], "string");
    }

    #[test]
    fn test_extract_output_schema_200() {
        let op = json!({
            "responses": {
                "200": {
                    "content": {
                        "application/json": {
                            "schema": {"type": "object", "properties": {"id": {"type": "integer"}}}
                        }
                    }
                }
            }
        });
        let result = extract_output_schema(&op, None);
        assert_eq!(result["properties"]["id"]["type"], "integer");
    }

    #[test]
    fn test_extract_output_schema_fallback() {
        let op = json!({"responses": {"404": {}}});
        let result = extract_output_schema(&op, None);
        assert_eq!(result["type"], "object");
    }

    #[test]
    fn test_deep_resolve_nested_ref() {
        let doc = json!({
            "components": {
                "schemas": {
                    "Address": {"type": "object", "properties": {"city": {"type": "string"}}},
                    "User": {
                        "type": "object",
                        "properties": {
                            "address": {"$ref": "#/components/schemas/Address"}
                        }
                    }
                }
            }
        });
        let schema = json!({"$ref": "#/components/schemas/User"});
        let result = deep_resolve_refs(&schema, &doc, 0);
        assert_eq!(
            result["properties"]["address"]["properties"]["city"]["type"],
            "string"
        );
    }

    #[test]
    fn test_deep_resolve_depth_limit() {
        // Self-referencing schema should not cause stack overflow
        let doc = json!({
            "components": {
                "schemas": {
                    "Recursive": {
                        "type": "object",
                        "properties": {
                            "child": {"$ref": "#/components/schemas/Recursive"}
                        }
                    }
                }
            }
        });
        let schema = json!({"$ref": "#/components/schemas/Recursive"});
        // Should terminate without panic
        let _ = deep_resolve_refs(&schema, &doc, 0);
    }

    #[test]
    fn test_resolve_ref_to_non_dict() {
        // $ref pointing to a string value returns {}
        let doc = json!({
            "components": {
                "schemas": {
                    "JustAString": "hello"
                }
            }
        });
        let result = resolve_ref("#/components/schemas/JustAString", &doc);
        assert_eq!(result, json!({}));

        // $ref pointing to a number value returns {}
        let doc2 = json!({
            "components": {
                "schemas": {
                    "JustANumber": 42
                }
            }
        });
        let result2 = resolve_ref("#/components/schemas/JustANumber", &doc2);
        assert_eq!(result2, json!({}));
    }

    #[test]
    fn test_resolve_ref_through_missing_path() {
        // $ref with intermediate missing keys returns {}
        let doc = json!({
            "components": {}
        });
        let result = resolve_ref("#/components/schemas/Missing", &doc);
        assert_eq!(result, json!({}));
    }

    #[test]
    fn test_resolve_schema_no_openapi_doc() {
        // None openapi_doc returns schema as-is even if it has a $ref
        let schema = json!({"$ref": "#/components/schemas/Foo", "type": "string"});
        let result = resolve_schema(&schema, None);
        assert_eq!(result, schema);
    }

    #[test]
    fn test_deep_resolve_refs_in_allof() {
        let doc = json!({
            "components": {
                "schemas": {
                    "Base": {"type": "object", "properties": {"id": {"type": "integer"}}},
                    "Extra": {"type": "object", "properties": {"tag": {"type": "string"}}}
                }
            }
        });
        let schema = json!({
            "allOf": [
                {"$ref": "#/components/schemas/Base"},
                {"$ref": "#/components/schemas/Extra"}
            ]
        });
        let result = deep_resolve_refs(&schema, &doc, 0);
        let all_of = result["allOf"].as_array().unwrap();
        assert_eq!(all_of[0]["properties"]["id"]["type"], "integer");
        assert_eq!(all_of[1]["properties"]["tag"]["type"], "string");
    }

    #[test]
    fn test_deep_resolve_refs_in_anyof() {
        let doc = json!({
            "components": {
                "schemas": {
                    "Cat": {"type": "object", "properties": {"purrs": {"type": "boolean"}}},
                    "Dog": {"type": "object", "properties": {"barks": {"type": "boolean"}}}
                }
            }
        });
        let schema = json!({
            "anyOf": [
                {"$ref": "#/components/schemas/Cat"},
                {"$ref": "#/components/schemas/Dog"}
            ]
        });
        let result = deep_resolve_refs(&schema, &doc, 0);
        let any_of = result["anyOf"].as_array().unwrap();
        assert_eq!(any_of[0]["properties"]["purrs"]["type"], "boolean");
        assert_eq!(any_of[1]["properties"]["barks"]["type"], "boolean");
    }

    #[test]
    fn test_deep_resolve_refs_in_items() {
        let doc = json!({
            "components": {
                "schemas": {
                    "Item": {"type": "object", "properties": {"name": {"type": "string"}}}
                }
            }
        });
        let schema = json!({
            "type": "array",
            "items": {"$ref": "#/components/schemas/Item"}
        });
        let result = deep_resolve_refs(&schema, &doc, 0);
        assert_eq!(result["items"]["properties"]["name"]["type"], "string");
    }

    #[test]
    fn test_deep_resolve_no_mutation() {
        let doc = json!({
            "components": {
                "schemas": {
                    "Addr": {"type": "object", "properties": {"city": {"type": "string"}}}
                }
            }
        });
        let doc_before = doc.clone();
        let schema = json!({
            "type": "object",
            "properties": {
                "address": {"$ref": "#/components/schemas/Addr"}
            }
        });
        let _result = deep_resolve_refs(&schema, &doc, 0);
        assert_eq!(doc, doc_before, "openapi_doc must not be mutated");
    }

    #[test]
    fn test_extract_input_schema_empty_operation() {
        let op = json!({});
        let result = extract_input_schema(&op, None);
        assert_eq!(result["type"], "object");
        assert!(result["properties"].as_object().unwrap().is_empty());
        assert!(result["required"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_extract_input_schema_ref_in_param() {
        let doc = json!({
            "components": {
                "schemas": {
                    "IdType": {"type": "integer", "format": "int64"}
                }
            }
        });
        let op = json!({
            "parameters": [
                {
                    "name": "user_id",
                    "in": "path",
                    "required": true,
                    "schema": {"$ref": "#/components/schemas/IdType"}
                }
            ]
        });
        let result = extract_input_schema(&op, Some(&doc));
        assert_eq!(result["properties"]["user_id"]["type"], "integer");
        assert_eq!(result["properties"]["user_id"]["format"], "int64");
    }

    #[test]
    fn test_extract_input_schema_nested_ref_in_body() {
        let doc = json!({
            "components": {
                "schemas": {
                    "Address": {"type": "object", "properties": {"zip": {"type": "string"}}}
                }
            }
        });
        let op = json!({
            "requestBody": {
                "content": {
                    "application/json": {
                        "schema": {
                            "type": "object",
                            "properties": {
                                "address": {"$ref": "#/components/schemas/Address"}
                            }
                        }
                    }
                }
            }
        });
        let result = extract_input_schema(&op, Some(&doc));
        assert_eq!(
            result["properties"]["address"]["properties"]["zip"]["type"],
            "string"
        );
    }

    #[test]
    fn test_extract_output_schema_201() {
        let op = json!({
            "responses": {
                "201": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "properties": {"id": {"type": "integer"}}
                            }
                        }
                    }
                }
            }
        });
        let result = extract_output_schema(&op, None);
        assert_eq!(result["properties"]["id"]["type"], "integer");
    }

    #[test]
    fn test_extract_output_schema_200_preferred() {
        let op = json!({
            "responses": {
                "200": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "properties": {"from200": {"type": "string"}}
                            }
                        }
                    }
                },
                "201": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "properties": {"from201": {"type": "string"}}
                            }
                        }
                    }
                }
            }
        });
        let result = extract_output_schema(&op, None);
        assert!(
            result["properties"]
                .as_object()
                .unwrap()
                .contains_key("from200"),
            "200 should be preferred over 201"
        );
        assert!(
            !result["properties"]
                .as_object()
                .unwrap()
                .contains_key("from201"),
            "201 should not be used when 200 exists"
        );
    }

    #[test]
    fn test_extract_output_schema_array_with_ref_items() {
        let doc = json!({
            "components": {
                "schemas": {
                    "Item": {"type": "object", "properties": {"name": {"type": "string"}}}
                }
            }
        });
        let op = json!({
            "responses": {
                "200": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "array",
                                "items": {"$ref": "#/components/schemas/Item"}
                            }
                        }
                    }
                }
            }
        });
        let result = extract_output_schema(&op, Some(&doc));
        assert_eq!(result["type"], "array");
        assert_eq!(result["items"]["properties"]["name"]["type"], "string");
    }

    #[test]
    fn test_extract_output_schema_allof() {
        let doc = json!({
            "components": {
                "schemas": {
                    "Base": {"type": "object", "properties": {"id": {"type": "integer"}}},
                    "Meta": {"type": "object", "properties": {"created": {"type": "string"}}}
                }
            }
        });
        let op = json!({
            "responses": {
                "200": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "allOf": [
                                    {"$ref": "#/components/schemas/Base"},
                                    {"$ref": "#/components/schemas/Meta"}
                                ]
                            }
                        }
                    }
                }
            }
        });
        let result = extract_output_schema(&op, Some(&doc));
        let all_of = result["allOf"].as_array().unwrap();
        assert_eq!(all_of[0]["properties"]["id"]["type"], "integer");
        assert_eq!(all_of[1]["properties"]["created"]["type"], "string");
    }

    #[test]
    fn test_extract_output_schema_nested_ref() {
        let doc = json!({
            "components": {
                "schemas": {
                    "Inner": {"type": "object", "properties": {"val": {"type": "number"}}}
                }
            }
        });
        let op = json!({
            "responses": {
                "200": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "properties": {
                                    "nested": {"$ref": "#/components/schemas/Inner"}
                                }
                            }
                        }
                    }
                }
            }
        });
        let result = extract_output_schema(&op, Some(&doc));
        assert_eq!(
            result["properties"]["nested"]["properties"]["val"]["type"],
            "number"
        );
    }

    #[test]
    fn test_extract_output_schema_empty_responses() {
        // No responses key at all returns default schema
        let op = json!({"operationId": "noResponses"});
        let result = extract_output_schema(&op, None);
        assert_eq!(result["type"], "object");
        assert!(result["properties"].as_object().unwrap().is_empty());
    }

    #[test]
    fn test_extract_output_schema_202() {
        // Regression test (D10-002): 202 Accepted must be recognised.
        // Previously only 200/201 were checked; 202/203 were silently ignored.
        let op = json!({
            "responses": {
                "202": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "properties": {"job_id": {"type": "string"}}
                            }
                        }
                    }
                }
            }
        });
        let result = extract_output_schema(&op, None);
        assert_eq!(
            result["properties"]["job_id"]["type"], "string",
            "202 response schema should be extracted; got: {result:?}"
        );
    }

    #[test]
    fn test_extract_output_schema_203() {
        // Regression test (D10-002): 203 Non-Authoritative Information must be recognised.
        let op = json!({
            "responses": {
                "203": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "properties": {"cached": {"type": "boolean"}}
                            }
                        }
                    }
                }
            }
        });
        let result = extract_output_schema(&op, None);
        assert_eq!(
            result["properties"]["cached"]["type"], "boolean",
            "203 response schema should be extracted; got: {result:?}"
        );
    }

    #[test]
    fn test_extract_input_schema_vnd_api_json() {
        // Regression test (Issue #32): application/vnd.api+json must be accepted
        // as a fallback when application/json is absent, matching Python behavior.
        let op = json!({
            "requestBody": {
                "content": {
                    "application/vnd.api+json": {
                        "schema": {
                            "type": "object",
                            "properties": {"data": {"type": "object"}},
                            "required": ["data"]
                        }
                    }
                }
            }
        });
        let result = extract_input_schema(&op, None);
        assert!(
            result["properties"]["data"].is_object(),
            "vnd.api+json schema properties should be extracted; got: {result:?}"
        );
        let req = result["required"].as_array().unwrap();
        assert!(
            req.contains(&Value::String("data".into())),
            "required field from vnd.api+json schema should be present; got: {req:?}"
        );
    }

    #[test]
    fn test_extract_input_schema_json_preferred_over_vnd_api_json() {
        // application/json takes priority over application/vnd.api+json.
        let op = json!({
            "requestBody": {
                "content": {
                    "application/json": {
                        "schema": {
                            "type": "object",
                            "properties": {"from_json": {"type": "string"}}
                        }
                    },
                    "application/vnd.api+json": {
                        "schema": {
                            "type": "object",
                            "properties": {"from_vnd": {"type": "string"}}
                        }
                    }
                }
            }
        });
        let result = extract_input_schema(&op, None);
        assert!(result["properties"]["from_json"].is_object());
        assert!(!result["properties"]
            .as_object()
            .unwrap()
            .contains_key("from_vnd"));
    }

    #[test]
    fn test_extract_output_schema_200_preferred_over_202() {
        // 200 should be picked over 202 when both exist.
        let op = json!({
            "responses": {
                "200": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "properties": {"from200": {"type": "string"}}
                            }
                        }
                    }
                },
                "202": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "properties": {"from202": {"type": "string"}}
                            }
                        }
                    }
                }
            }
        });
        let result = extract_output_schema(&op, None);
        assert!(result["properties"]
            .as_object()
            .unwrap()
            .contains_key("from200"));
        assert!(!result["properties"]
            .as_object()
            .unwrap()
            .contains_key("from202"));
    }

    #[test]
    fn test_deep_resolve_depth_limit_at_exactly_16() {
        // Regression test (D10-004): depth boundary must be > 16 (cut off AT depth 17),
        // not >= 16. Python and TypeScript resolve through depth 16; Rust must match.
        let doc = json!({
            "components": {
                "schemas": {
                    "Leaf": {"type": "string"}
                }
            }
        });
        let schema = json!({"$ref": "#/components/schemas/Leaf"});
        // At depth 15 the ref IS resolved
        let at_15 = deep_resolve_refs(&schema, &doc, 15);
        assert_eq!(at_15["type"], "string", "depth 15 should resolve the $ref");
        // At depth 16 the ref IS ALSO resolved (>16 is the cut-off, not >=16)
        let at_16 = deep_resolve_refs(&schema, &doc, 16);
        assert_eq!(
            at_16["type"], "string",
            "depth 16 should resolve the $ref (boundary fix)"
        );
        // At depth 17 the schema is returned unchanged (cut-off)
        let at_17 = deep_resolve_refs(&schema, &doc, 17);
        assert!(
            at_17.get("$ref").is_some(),
            "depth 17 must return schema unchanged"
        );
    }

    #[test]
    fn test_extract_output_schema_204() {
        // D11-001: any 2xx code should be accepted. 204 No Content (with a schema)
        // must be extracted, not silently ignored.
        let op = json!({
            "responses": {
                "204": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "properties": {"accepted": {"type": "boolean"}}
                            }
                        }
                    }
                }
            }
        });
        let result = extract_output_schema(&op, None);
        assert_eq!(
            result["properties"]["accepted"]["type"], "boolean",
            "204 response schema should be extracted; got: {result:?}"
        );
    }

    #[test]
    fn test_extract_output_schema_vnd_api_json_output() {
        // D11-002: application/vnd.api+json should be accepted as a fallback for
        // output schemas, matching Python behaviour.
        let op = json!({
            "responses": {
                "200": {
                    "content": {
                        "application/vnd.api+json": {
                            "schema": {
                                "type": "object",
                                "properties": {"data": {"type": "object"}}
                            }
                        }
                    }
                }
            }
        });
        let result = extract_output_schema(&op, None);
        assert!(
            result["properties"]["data"].is_object(),
            "vnd.api+json output schema properties should be extracted; got: {result:?}"
        );
    }

    #[test]
    fn test_extract_output_schema_json_preferred_over_vnd_api_json_output() {
        // application/json takes priority over application/vnd.api+json for outputs.
        let op = json!({
            "responses": {
                "200": {
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "properties": {"from_json": {"type": "string"}}
                            }
                        },
                        "application/vnd.api+json": {
                            "schema": {
                                "type": "object",
                                "properties": {"from_vnd": {"type": "string"}}
                            }
                        }
                    }
                }
            }
        });
        let result = extract_output_schema(&op, None);
        assert!(result["properties"]["from_json"].is_object());
        assert!(!result["properties"]
            .as_object()
            .unwrap()
            .contains_key("from_vnd"));
    }

    #[test]
    fn test_deep_resolve_16_levels_of_nesting() {
        // Regression test (D10-004): a chain of exactly 16 $ref levels must
        // be fully resolved. With the old >= 16 boundary, level 16 was cut off.
        //
        // Build: L0 -> L1 -> L2 -> ... -> L15 -> Leaf
        // That is 16 hops (depth 0 enters L0, depth 1 enters L1, ...,
        // depth 15 enters L15, depth 16 resolves the $ref inside L15 to Leaf).
        let mut schemas = serde_json::Map::new();
        schemas.insert("Leaf".into(), json!({"type": "string"}));
        // L15 references Leaf; L14 references L15; ... L0 references L1.
        for i in (0..16usize).rev() {
            let target = if i == 15 {
                "Leaf".to_string()
            } else {
                format!("L{}", i + 1)
            };
            schemas.insert(
                format!("L{i}"),
                json!({"$ref": format!("#/components/schemas/{target}")}),
            );
        }
        let doc = json!({"components": {"schemas": schemas}});
        let schema = json!({"$ref": "#/components/schemas/L0"});
        let result = deep_resolve_refs(&schema, &doc, 0);
        assert_eq!(
            result["type"], "string",
            "16-level deep $ref chain should be fully resolved; got: {result:?}"
        );
    }
}
