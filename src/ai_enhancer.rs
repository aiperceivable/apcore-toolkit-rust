// AI-driven metadata enhancement using local SLMs.
//
// Uses an OpenAI-compatible local API (e.g., Ollama, vLLM, LM Studio) to fill
// metadata gaps that static analysis cannot resolve.
//
// All AI-generated fields are tagged with `x-generated-by: slm` in the module's
// metadata for auditability.

use std::env;
use std::time::Duration;

use serde_json::{json, Value};
use thiserror::Error;
use tracing::warn;

use apcore::module::ModuleAnnotations;

use crate::types::ScannedModule;

const DEFAULT_ENDPOINT: &str = "http://localhost:11434/v1";
const DEFAULT_MODEL: &str = "qwen:0.6b";
const DEFAULT_THRESHOLD: f64 = 0.7;
const DEFAULT_BATCH_SIZE: usize = 5;
const DEFAULT_TIMEOUT: u64 = 30;

/// All annotation fields that the SLM may assign confidence scores for.
///
/// Keep in sync with `apcore::module::ModuleAnnotations` when upstream adds fields.
const ANNOTATION_FIELDS: &[&str] = &[
    "description",
    "documentation",
    "tags",
    "version",
    "cacheable",
    "readonly",
    "destructive",
    "idempotent",
    "requires_confirmation",
    "long_running",
    "category",
];

/// Errors returned by [`AIEnhancer`] operations.
#[derive(Debug, Error)]
pub enum AIEnhancerError {
    /// Invalid configuration value.
    #[error("invalid config: {0}")]
    Config(String),
    /// Failed to reach the SLM endpoint.
    #[error("connection failed: {0}")]
    Connection(String),
    /// SLM returned an unparseable response.
    #[error("bad response: {0}")]
    Response(String),
}

/// Protocol for pluggable metadata enhancement.
///
/// # Blocking / async compatibility
///
/// `enhance` is a synchronous method. The bundled [`AIEnhancer`] performs
/// blocking HTTP requests via `ureq`, so each call may park the current
/// thread for up to `APCORE_AI_TIMEOUT` seconds (default 30) per module.
/// **Do not call `enhance` directly from an async task** on a Tokio (or
/// other async) runtime — it will block a runtime worker thread and can
/// stall the scheduler under concurrent load.
///
/// From an async context, wrap the call in
/// [`tokio::task::spawn_blocking`]:
///
/// ```ignore
/// let enhanced = tokio::task::spawn_blocking(move || enhancer.enhance(modules)).await?;
/// ```
///
/// Enhancement is a one-shot scanning-phase operation (not per-request),
/// so this is typically invoked once during framework adapter bootstrap.
pub trait Enhancer {
    /// Enhance a list of ScannedModules by filling metadata gaps.
    ///
    /// Synchronous and potentially long-running. See the trait-level doc
    /// comment for guidance on invoking from async contexts.
    fn enhance(&self, modules: Vec<ScannedModule>) -> Vec<ScannedModule>;
}

/// Enhances ScannedModule metadata using a local SLM.
///
/// Configuration is read from environment variables or constructor parameters:
/// - `APCORE_AI_ENABLED`: Enable enhancement (default: false).
/// - `APCORE_AI_ENDPOINT`: OpenAI-compatible API URL.
/// - `APCORE_AI_MODEL`: Model name.
/// - `APCORE_AI_THRESHOLD`: Confidence threshold (0.0–1.0).
/// - `APCORE_AI_BATCH_SIZE`: Modules per API call.
/// - `APCORE_AI_TIMEOUT`: Timeout in seconds per API call.
#[derive(Debug)]
pub struct AIEnhancer {
    pub endpoint: String,
    pub model: String,
    pub threshold: f64,
    pub batch_size: usize,
    pub timeout: u64,
}

impl AIEnhancer {
    /// Create a new AIEnhancer with optional overrides.
    ///
    /// Falls back to environment variables, then defaults.
    pub fn new(
        endpoint: Option<String>,
        model: Option<String>,
        threshold: Option<f64>,
        batch_size: Option<usize>,
        timeout: Option<u64>,
    ) -> Result<Self, AIEnhancerError> {
        let endpoint = endpoint.unwrap_or_else(|| {
            env::var("APCORE_AI_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.into())
        });
        let model = model.unwrap_or_else(|| {
            env::var("APCORE_AI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into())
        });
        let threshold =
            threshold.unwrap_or_else(|| parse_float_env("APCORE_AI_THRESHOLD", DEFAULT_THRESHOLD));
        let batch_size = batch_size
            .unwrap_or_else(|| parse_usize_env("APCORE_AI_BATCH_SIZE", DEFAULT_BATCH_SIZE));
        let timeout =
            timeout.unwrap_or_else(|| parse_u64_env("APCORE_AI_TIMEOUT", DEFAULT_TIMEOUT));

        if !(0.0..=1.0).contains(&threshold) {
            return Err(AIEnhancerError::Config(
                "APCORE_AI_THRESHOLD must be between 0.0 and 1.0".into(),
            ));
        }
        if batch_size == 0 {
            return Err(AIEnhancerError::Config(
                "APCORE_AI_BATCH_SIZE must be a positive integer".into(),
            ));
        }
        if timeout == 0 {
            return Err(AIEnhancerError::Config(
                "APCORE_AI_TIMEOUT must be a positive integer".into(),
            ));
        }

        Ok(Self {
            endpoint,
            model,
            threshold,
            batch_size,
            timeout,
        })
    }

    /// Check whether AI enhancement is enabled via environment.
    pub fn is_enabled() -> bool {
        env::var("APCORE_AI_ENABLED")
            .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false)
    }

    /// Identify which metadata fields are missing or at defaults.
    fn identify_gaps(&self, module: &ScannedModule) -> Vec<String> {
        let mut gaps: Vec<String> = Vec::new();

        if module.description.is_empty() || module.description == module.module_id {
            gaps.push("description".into());
        }
        if module.documentation.is_none() {
            gaps.push("documentation".into());
        }
        if module.annotations.is_none()
            || module
                .annotations
                .as_ref()
                .is_some_and(is_default_annotations)
        {
            gaps.push("annotations".into());
        }
        if module
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|o| o.is_empty())
            .unwrap_or(true)
        {
            gaps.push("input_schema".into());
        }

        gaps
    }

    /// Build a structured prompt for the SLM.
    fn build_prompt(&self, module: &ScannedModule, gaps: &[String]) -> String {
        let mut parts = vec![
            "You are analyzing a function to generate metadata for an AI-perceivable module system.".into(),
            String::new(),
            format!("Module ID: {}", module.module_id),
            format!("Target: {}", module.target),
        ];

        if !module.description.is_empty() {
            parts.push(format!("Current description: {}", module.description));
        }

        parts.push(String::new());
        parts.push("Please provide the following missing metadata as JSON:".into());
        parts.push("{".into());

        for gap in gaps {
            match gap.as_str() {
                "description" => {
                    parts.push(
                        r#"  "description": "<≤200 chars, what this function does>","#.into(),
                    );
                }
                "documentation" => {
                    parts.push(r#"  "documentation": "<detailed Markdown explanation>","#.into());
                }
                "annotations" => {
                    parts.push(r#"  "annotations": {"#.into());
                    parts.push(r#"    "readonly": <true if no side effects>,"#.into());
                    parts.push(r#"    "destructive": <true if deletes/overwrites data>,"#.into());
                    parts.push(r#"    "idempotent": <true if safe to retry>,"#.into());
                    parts.push(r#"    "requires_approval": <true if dangerous operation>,"#.into());
                    parts.push(r#"    "open_world": <true if calls external systems>,"#.into());
                    parts
                        .push(r#"    "streaming": <true if yields results incrementally>,"#.into());
                    parts.push(r#"    "cacheable": <true if results can be cached>,"#.into());
                    parts.push(r#"    "cache_ttl": <seconds, 0 for no expiry>,"#.into());
                    parts.push(r#"    "cache_key_fields": <list of input field names for cache key, or null for all>,"#.into());
                    parts.push(r#"    "paginated": <true if supports pagination>,"#.into());
                    parts
                        .push(r#"    "pagination_style": <"cursor" or "offset" or "page">"#.into());
                    parts.push("  },".into());
                }
                "input_schema" => {
                    parts.push(
                        r#"  "input_schema": <JSON Schema object for function parameters>,"#.into(),
                    );
                }
                _ => {}
            }
        }

        let confidence_keys: serde_json::Value = ANNOTATION_FIELDS
            .iter()
            .map(|&field| (field.to_string(), serde_json::json!(0.0)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let confidence_str =
            serde_json::to_string_pretty(&confidence_keys).unwrap_or_else(|_| "{}".into());
        parts.push(format!(r#"  "confidence": {confidence_str}"#));
        parts.push("}".into());
        parts.push(String::new());
        parts.push("Respond with ONLY valid JSON, no markdown fences or explanation.".into());

        parts.join("\n")
    }

    /// Call the OpenAI-compatible API and return the response text.
    fn call_llm(&self, prompt: &str) -> Result<String, AIEnhancerError> {
        let url = format!("{}/chat/completions", self.endpoint.trim_end_matches('/'));
        let payload = json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0.1,
        });

        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(self.timeout)))
            .build()
            .new_agent();

        let body: Value = agent
            .post(&url)
            .header("Content-Type", "application/json")
            .send_json(&payload)
            .map_err(|e| AIEnhancerError::Connection(format!("Failed to reach SLM at {url}: {e}")))?
            .body_mut()
            .read_json()
            .map_err(|e| AIEnhancerError::Response(format!("Failed to parse SLM response: {e}")))?;

        body["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| AIEnhancerError::Response("Unexpected API response structure".into()))
    }

    /// Parse the SLM response as JSON, stripping markdown fences if present.
    fn parse_response(response: &str) -> Result<Value, AIEnhancerError> {
        let mut text = response.trim().to_string();

        // Strip markdown code fences
        if text.starts_with("```") {
            let lines: Vec<&str> = text.split('\n').collect();
            let start = if lines[0].starts_with("```") { 1 } else { 0 };
            let end = if lines.last().map(|l| l.trim()) == Some("```") {
                lines.len() - 1
            } else {
                lines.len()
            };
            text = lines[start..end].join("\n");
        }

        serde_json::from_str(&text)
            .map_err(|e| AIEnhancerError::Response(format!("SLM returned invalid JSON: {e}")))
    }

    /// Enhance a single module by calling the SLM.
    fn enhance_module(
        &self,
        module: &ScannedModule,
        gaps: &[String],
    ) -> Result<ScannedModule, AIEnhancerError> {
        let prompt = self.build_prompt(module, gaps);
        let response = self.call_llm(&prompt)?;
        let parsed = Self::parse_response(&response)?;

        let mut result = module.clone();
        let mut confidence: serde_json::Map<String, Value> = serde_json::Map::new();

        // Apply description
        if gaps.iter().any(|g| g == "description") {
            if let Some(desc) = parsed.get("description").and_then(|v| v.as_str()) {
                let conf = parsed
                    .get("confidence")
                    .and_then(|c| c.get("description"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                confidence.insert("description".into(), json!(conf));
                if conf >= self.threshold {
                    result.description = desc.to_string();
                } else {
                    result.warnings.push(format!(
                        "Low confidence ({conf:.2}) for description — skipped. Review manually."
                    ));
                }
            }
        }

        // Apply documentation
        if gaps.iter().any(|g| g == "documentation") {
            if let Some(doc) = parsed.get("documentation").and_then(|v| v.as_str()) {
                let conf = parsed
                    .get("confidence")
                    .and_then(|c| c.get("documentation"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                confidence.insert("documentation".into(), json!(conf));
                if conf >= self.threshold {
                    result.documentation = Some(doc.to_string());
                } else {
                    result.warnings.push(format!(
                        "Low confidence ({conf:.2}) for documentation — skipped. Review manually."
                    ));
                }
            }
        }

        // Apply annotations if above threshold (per-field confidence)
        if gaps.iter().any(|g| g == "annotations") {
            if let Some(ann_data) = parsed.get("annotations").and_then(|v| v.as_object()) {
                let ann_conf = parsed
                    .get("confidence")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();
                let mut base = module.annotations.clone().unwrap_or_default();
                let mut any_accepted = false;

                // Iterate boolean fields supplied by the SLM directly.
                // `set_bool_annotation` validates each field's existence
                // on `ModuleAnnotations` via a serde round-trip, so the
                // set of known bool fields lives in one place — the
                // upstream struct — and new fields added upstream are
                // picked up automatically.
                for (field, field_val) in ann_data.iter() {
                    let Some(bool_val) = field_val.as_bool() else {
                        continue;
                    };
                    let field_conf = get_annotation_confidence(&ann_conf, field);
                    confidence.insert(format!("annotations.{field}"), json!(field_conf));
                    if field_conf >= self.threshold {
                        if set_bool_annotation(&mut base, field, bool_val) {
                            any_accepted = true;
                        } else {
                            result.warnings.push(format!(
                                "SLM returned unknown bool annotation '{field}' — ignored."
                            ));
                        }
                    } else {
                        result.warnings.push(format!(
                            "Low confidence ({field_conf:.2}) for annotations.{field} — skipped. Review manually."
                        ));
                    }
                }

                // Integer fields: cache_ttl
                if let Some(val) = ann_data.get("cache_ttl").and_then(|v| v.as_u64()) {
                    let field_conf = get_annotation_confidence(&ann_conf, "cache_ttl");
                    confidence.insert("annotations.cache_ttl".into(), json!(field_conf));
                    if field_conf >= self.threshold {
                        base.cache_ttl = val;
                        any_accepted = true;
                    } else {
                        result.warnings.push(format!(
                            "Low confidence ({field_conf:.2}) for annotations.cache_ttl — skipped. Review manually."
                        ));
                    }
                }

                // String fields: pagination_style
                if let Some(val) = ann_data.get("pagination_style").and_then(|v| v.as_str()) {
                    let field_conf = get_annotation_confidence(&ann_conf, "pagination_style");
                    confidence.insert("annotations.pagination_style".into(), json!(field_conf));
                    if field_conf >= self.threshold {
                        base.pagination_style = val.to_string();
                        any_accepted = true;
                    } else {
                        result.warnings.push(format!(
                            "Low confidence ({field_conf:.2}) for annotations.pagination_style — skipped. Review manually."
                        ));
                    }
                }

                // List fields: cache_key_fields
                if let Some(arr) = ann_data.get("cache_key_fields").and_then(|v| v.as_array()) {
                    let field_conf = get_annotation_confidence(&ann_conf, "cache_key_fields");
                    confidence.insert("annotations.cache_key_fields".into(), json!(field_conf));
                    if field_conf >= self.threshold {
                        let keys: Vec<String> = arr
                            .iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect();
                        base.cache_key_fields = Some(keys);
                        any_accepted = true;
                    } else {
                        result.warnings.push(format!(
                            "Low confidence ({field_conf:.2}) for annotations.cache_key_fields — skipped. Review manually."
                        ));
                    }
                }

                if any_accepted {
                    result.annotations = Some(base);
                }
            }
        }

        // Apply input_schema if above threshold
        if gaps.iter().any(|g| g == "input_schema") {
            if let Some(schema) = parsed.get("input_schema") {
                let conf = parsed
                    .get("confidence")
                    .and_then(|c| c.get("input_schema"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                confidence.insert("input_schema".into(), json!(conf));
                if conf >= self.threshold {
                    result.input_schema = schema.clone();
                } else {
                    result.warnings.push(format!(
                        "Low confidence ({conf:.2}) for input_schema — skipped. Review manually."
                    ));
                }
            }
        }

        // Tag AI-generated fields
        if !confidence.is_empty() {
            result
                .metadata
                .insert("x-generated-by".into(), Value::String("slm".into()));
            result
                .metadata
                .insert("x-ai-confidence".into(), Value::Object(confidence));
        }

        Ok(result)
    }
}

impl Enhancer for AIEnhancer {
    fn enhance(&self, modules: Vec<ScannedModule>) -> Vec<ScannedModule> {
        let mut results: Vec<ScannedModule> = Vec::with_capacity(modules.len());

        let mut pending: Vec<(usize, Vec<String>)> = Vec::new();
        for (idx, module) in modules.iter().enumerate() {
            let gaps = self.identify_gaps(module);
            results.push(module.clone());
            if !gaps.is_empty() {
                pending.push((idx, gaps));
            }
        }

        for batch in pending.chunks(self.batch_size) {
            for (idx, gaps) in batch {
                match self.enhance_module(&modules[*idx], gaps) {
                    Ok(enhanced) => results[*idx] = enhanced,
                    Err(e) => {
                        warn!("AI enhancement failed for {}: {e}", modules[*idx].module_id);
                    }
                }
            }
        }

        results
    }
}

/// Check whether annotations are at their default values.
///
/// Uses `serde_json` round-trip equality so the comparison automatically
/// covers any new field added to `apcore::module::ModuleAnnotations` upstream
/// (including the `extra` extension map). `ModuleAnnotations` does not
/// implement `PartialEq`, so direct `==` is unavailable.
fn is_default_annotations(ann: &ModuleAnnotations) -> bool {
    match (
        serde_json::to_value(ann),
        serde_json::to_value(ModuleAnnotations::default()),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Get confidence for an annotation field, checking both `annotations.<field>` and `<field>` keys.
fn get_annotation_confidence(conf: &serde_json::Map<String, Value>, field: &str) -> f64 {
    conf.get(&format!("annotations.{field}"))
        .or_else(|| conf.get(field))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

/// Set a boolean field on `ModuleAnnotations` by name via a serde
/// round-trip. Returns `true` if the field exists on the struct and is a
/// boolean; `false` if the field is unknown, non-boolean, or the
/// round-trip fails. Using serde rather than a hardcoded match removes
/// the two-list drift risk — new bool fields added to
/// `apcore::module::ModuleAnnotations` upstream are picked up
/// automatically.
fn set_bool_annotation(ann: &mut ModuleAnnotations, field: &str, value: bool) -> bool {
    let mut serialized = match serde_json::to_value(&ann) {
        Ok(v) => v,
        Err(e) => {
            warn!("set_bool_annotation: serialize failed: {e}");
            return false;
        }
    };
    let Some(obj) = serialized.as_object_mut() else {
        return false;
    };
    match obj.get(field) {
        Some(Value::Bool(_)) => {
            obj.insert(field.to_string(), Value::Bool(value));
        }
        // Field absent, or present but not a bool — reject rather than
        // fabricate a new key (serde would happily accept unknown keys
        // via `#[serde(extra)]` on ModuleAnnotations, but misclassifying
        // a non-bool field as bool would corrupt the struct).
        _ => return false,
    }
    match serde_json::from_value::<ModuleAnnotations>(serialized) {
        Ok(new_ann) => {
            *ann = new_ann;
            true
        }
        Err(e) => {
            warn!("set_bool_annotation: deserialize failed: {e}");
            false
        }
    }
}

fn parse_float_env(name: &str, default: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn parse_usize_env(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn parse_u64_env(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apcore::module::ModuleAnnotations;
    use serde_json::json;

    #[test]
    fn test_ai_enhancer_new_defaults() {
        let enhancer = AIEnhancer::new(None, None, None, None, None).unwrap();
        assert_eq!(enhancer.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(enhancer.model, DEFAULT_MODEL);
        assert!((enhancer.threshold - DEFAULT_THRESHOLD).abs() < f64::EPSILON);
        assert_eq!(enhancer.batch_size, DEFAULT_BATCH_SIZE);
        assert_eq!(enhancer.timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn test_ai_enhancer_new_with_overrides() {
        let enhancer = AIEnhancer::new(
            Some("http://custom:8080".into()),
            Some("llama3".into()),
            Some(0.5),
            Some(10),
            Some(60),
        )
        .unwrap();
        assert_eq!(enhancer.endpoint, "http://custom:8080");
        assert_eq!(enhancer.model, "llama3");
        assert!((enhancer.threshold - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ai_enhancer_threshold_validation() {
        let result = AIEnhancer::new(None, None, Some(1.5), None, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_ai_enhancer_batch_size_validation() {
        let result = AIEnhancer::new(None, None, None, Some(0), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_identify_gaps_complete_module() {
        let enhancer = AIEnhancer::new(None, None, None, None, None).unwrap();
        let mut module = ScannedModule::new(
            "test".into(),
            "A real description".into(),
            json!({"type": "object", "properties": {"x": {"type": "string"}}}),
            json!({}),
            vec![],
            "app:func".into(),
        );
        module.documentation = Some("Full docs".into());
        module.annotations = Some(ModuleAnnotations {
            readonly: true,
            ..Default::default()
        });
        let gaps = enhancer.identify_gaps(&module);
        assert!(gaps.is_empty());
    }

    #[test]
    fn test_identify_gaps_missing_fields() {
        let enhancer = AIEnhancer::new(None, None, None, None, None).unwrap();
        let module = ScannedModule::new(
            "test".into(),
            String::new(),
            json!({"type": "object"}),
            json!({}),
            vec![],
            "app:func".into(),
        );
        let gaps = enhancer.identify_gaps(&module);
        assert!(gaps.iter().any(|g| g == "description"));
        assert!(gaps.iter().any(|g| g == "documentation"));
        assert!(gaps.iter().any(|g| g == "annotations"));
        assert!(gaps.iter().any(|g| g == "input_schema"));
    }

    #[test]
    fn test_parse_response_valid_json() {
        let response = r#"{"description": "hello", "confidence": {"description": 0.9}}"#;
        let result = AIEnhancer::parse_response(response).unwrap();
        assert_eq!(result["description"], "hello");
    }

    #[test]
    fn test_parse_response_with_fences() {
        let response = "```json\n{\"key\": \"value\"}\n```";
        let result = AIEnhancer::parse_response(response).unwrap();
        assert_eq!(result["key"], "value");
    }

    #[test]
    fn test_parse_response_invalid() {
        let result = AIEnhancer::parse_response("not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_is_enabled_default() {
        // Assuming env var is not set in test environment
        env::remove_var("APCORE_AI_ENABLED");
        assert!(!AIEnhancer::is_enabled());
    }

    #[test]
    fn test_build_prompt_contains_module_info() {
        let enhancer = AIEnhancer::new(None, None, None, None, None).unwrap();
        let module = ScannedModule::new(
            "users.get".into(),
            "Get user".into(),
            json!({}),
            json!({}),
            vec![],
            "app:get_user".into(),
        );
        let prompt = enhancer.build_prompt(&module, &["description".into()]);
        assert!(prompt.contains("users.get"));
        assert!(prompt.contains("app:get_user"));
        assert!(prompt.contains("description"));
    }

    #[test]
    fn test_identify_gaps_description_equals_module_id() {
        let enhancer = AIEnhancer::new(None, None, None, None, None).unwrap();
        let module = ScannedModule::new(
            "my_module".into(),
            "my_module".into(), // description == module_id
            json!({"type": "object", "properties": {"x": {"type": "string"}}}),
            json!({}),
            vec![],
            "app:func".into(),
        );
        let gaps = enhancer.identify_gaps(&module);
        assert!(
            gaps.iter().any(|g| g == "description"),
            "description matching module_id should be identified as a gap"
        );
    }

    #[test]
    fn test_ai_enhancer_timeout_validation() {
        let result = AIEnhancer::new(None, None, None, None, Some(0));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err
            .to_string()
            .contains("APCORE_AI_TIMEOUT must be a positive integer"));
    }

    // All is_enabled tests are combined into one to prevent env var races
    // when tests run in parallel (env vars are process-global).
    #[test]
    fn test_is_enabled_variants() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        // Default (unset) → disabled
        unsafe { env::remove_var("APCORE_AI_ENABLED") };
        assert!(!AIEnhancer::is_enabled(), "should be disabled by default");

        // "true" → enabled
        unsafe { env::set_var("APCORE_AI_ENABLED", "true") };
        assert!(AIEnhancer::is_enabled(), "\"true\" should enable");

        // "yes" → enabled
        unsafe { env::set_var("APCORE_AI_ENABLED", "yes") };
        assert!(AIEnhancer::is_enabled(), "\"yes\" should enable");

        // "1" → enabled
        unsafe { env::set_var("APCORE_AI_ENABLED", "1") };
        assert!(AIEnhancer::is_enabled(), "\"1\" should enable");

        // "false" → disabled
        unsafe { env::set_var("APCORE_AI_ENABLED", "false") };
        assert!(!AIEnhancer::is_enabled(), "\"false\" should disable");

        // Cleanup
        unsafe { env::remove_var("APCORE_AI_ENABLED") };
    }

    #[test]
    fn test_parse_response_strips_json_fence() {
        let response = "```json\n{\"description\": \"hello world\"}\n```";
        let result = AIEnhancer::parse_response(response).unwrap();
        assert_eq!(result["description"], "hello world");
    }

    #[test]
    fn test_build_prompt_requests_annotations() {
        let enhancer = AIEnhancer::new(None, None, None, None, None).unwrap();
        let module = ScannedModule::new(
            "test".into(),
            "desc".into(),
            json!({}),
            json!({}),
            vec![],
            "app:func".into(),
        );
        let prompt = enhancer.build_prompt(&module, &["annotations".into()]);
        assert!(
            prompt.contains("readonly"),
            "prompt should mention annotations fields"
        );
        assert!(prompt.contains("destructive"));
        assert!(prompt.contains("idempotent"));
    }

    #[test]
    fn test_build_prompt_requests_input_schema() {
        let enhancer = AIEnhancer::new(None, None, None, None, None).unwrap();
        let module = ScannedModule::new(
            "test".into(),
            "desc".into(),
            json!({}),
            json!({}),
            vec![],
            "app:func".into(),
        );
        let prompt = enhancer.build_prompt(&module, &["input_schema".into()]);
        assert!(
            prompt.contains("input_schema"),
            "prompt should mention input_schema"
        );
        assert!(prompt.contains("JSON Schema"));
    }

    #[test]
    fn test_build_prompt_requests_documentation() {
        let enhancer = AIEnhancer::new(None, None, None, None, None).unwrap();
        let module = ScannedModule::new(
            "test".into(),
            "desc".into(),
            json!({}),
            json!({}),
            vec![],
            "app:func".into(),
        );
        let prompt = enhancer.build_prompt(&module, &["documentation".into()]);
        assert!(
            prompt.contains("documentation"),
            "prompt should mention documentation"
        );
        assert!(prompt.contains("Markdown"));
    }

    // ---- set_bool_annotation (serde round-trip, D4-1 regression guards) ----

    #[test]
    fn test_set_bool_annotation_readonly() {
        let mut ann = ModuleAnnotations::default();
        assert!(set_bool_annotation(&mut ann, "readonly", true));
        assert!(ann.readonly);
    }

    #[test]
    fn test_set_bool_annotation_destructive() {
        let mut ann = ModuleAnnotations::default();
        assert!(set_bool_annotation(&mut ann, "destructive", true));
        assert!(ann.destructive);
    }

    #[test]
    fn test_set_bool_annotation_unknown_field_rejected() {
        let mut ann = ModuleAnnotations::default();
        assert!(!set_bool_annotation(
            &mut ann,
            "nonexistent_field_xyz",
            true
        ));
        // Annotations unchanged.
        assert!(is_default_annotations(&ann));
    }

    #[test]
    fn test_set_bool_annotation_non_bool_field_rejected() {
        let mut ann = ModuleAnnotations::default();
        // `cache_ttl` is an integer field on ModuleAnnotations.
        // Round-trip rejects setting it to a bool.
        assert!(!set_bool_annotation(&mut ann, "cache_ttl", true));
        assert_eq!(ann.cache_ttl, 0); // unchanged default
    }

    #[test]
    fn test_set_bool_annotation_preserves_other_fields() {
        let mut ann = ModuleAnnotations {
            destructive: true,
            cache_ttl: 99,
            ..Default::default()
        };
        assert!(set_bool_annotation(&mut ann, "readonly", true));
        // Original fields survive the serde round-trip.
        assert!(ann.readonly);
        assert!(ann.destructive);
        assert_eq!(ann.cache_ttl, 99);
    }
}
