// HTTP proxy registry writer.
//
// Registers scanned modules as HTTP proxy implementations that forward
// requests to a running web API. Feature-gated behind `http-proxy`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use regex::Regex;
use tracing::debug;

use apcore::context::Context;
use apcore::errors::ModuleError;
use apcore::module::{Module, ModuleAnnotations};
use apcore::Registry;

use crate::output::types::WriteResult;
use crate::types::ScannedModule;

/// Register scanned modules as HTTP proxy modules in the registry.
///
/// Each module's `execute()` sends an HTTP request to the target API
/// instead of calling the handler directly.
pub struct HTTPProxyRegistryWriter {
    base_url: String,
    auth_header_factory: Option<Arc<dyn Fn() -> HashMap<String, String> + Send + Sync>>,
    timeout_secs: f64,
}

impl HTTPProxyRegistryWriter {
    /// Create a new HTTP proxy writer.
    ///
    /// - `base_url`: Base URL of the target API.
    /// - `auth_header_factory`: Optional callable returning HTTP headers for auth.
    /// - `timeout_secs`: HTTP request timeout in seconds.
    pub fn new(
        base_url: String,
        auth_header_factory: Option<Box<dyn Fn() -> HashMap<String, String> + Send + Sync>>,
        timeout_secs: f64,
    ) -> Self {
        Self {
            base_url,
            auth_header_factory: auth_header_factory.map(Arc::from),
            timeout_secs,
        }
    }

    /// Register each ScannedModule as an HTTP proxy module.
    pub fn write(&self, modules: &[ScannedModule], registry: &mut Registry) -> Vec<WriteResult> {
        let mut results: Vec<WriteResult> = Vec::new();

        for module in modules {
            let (http_method, url_path) = get_http_fields(module);
            let path_params = extract_path_params(&url_path);
            let proxy = ProxyModule {
                base_url: self.base_url.clone(),
                http_method,
                url_path,
                path_params,
                input_schema: module.input_schema.clone(),
                output_schema: module.output_schema.clone(),
                description: module.description.clone(),
                annotations: module.annotations.clone().unwrap_or_default(),
                timeout_secs: self.timeout_secs,
                auth_header_factory: self.auth_header_factory.clone(),
            };

            let descriptor = apcore::registry::registry::ModuleDescriptor {
                module_id: module.module_id.clone(),
                name: Some(module.module_id.clone()),
                description: module.description.clone(),
                documentation: module.documentation.clone(),
                input_schema: module.input_schema.clone(),
                output_schema: module.output_schema.clone(),
                version: module.version.clone(),
                tags: module.tags.clone(),
                annotations: Some(proxy.annotations.clone()),
                examples: module.examples.clone(),
                metadata: module.metadata.clone(),
                display: module.display.clone(),
                sunset_date: None,
                dependencies: vec![],
                enabled: true,
            };

            match registry.register(&module.module_id, Box::new(proxy), descriptor) {
                Ok(()) => {
                    debug!("Registered HTTP proxy: {}", module.module_id);
                    results.push(WriteResult::new(module.module_id.clone()));
                }
                Err(e) => {
                    debug!("Skipped {}: {}", module.module_id, e);
                    results.push(WriteResult::failed(
                        module.module_id.clone(),
                        None,
                        e.to_string(),
                    ));
                }
            }
        }

        results
    }
}

/// Extract http_method and url_path from a ScannedModule's metadata.
fn get_http_fields(module: &ScannedModule) -> (String, String) {
    let http_method = module
        .metadata
        .get("http_method")
        .and_then(|v| v.as_str())
        .unwrap_or("GET")
        .to_string();
    let url_path = module
        .metadata
        .get("url_path")
        .and_then(|v| v.as_str())
        .unwrap_or("/")
        .to_string();
    (http_method, url_path)
}

/// Regex matching URL path parameters like `{user_id}`.
static PATH_PARAM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{(\w+)\}").expect("static regex"));

/// Extract path parameter names from a URL pattern like `/users/{user_id}`.
fn extract_path_params(url_path: &str) -> HashSet<String> {
    PATH_PARAM_RE
        .captures_iter(url_path)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

/// Extract a human-readable error message from an HTTP error response body.
///
/// Tries to parse the body as JSON and looks for common error fields
/// (`error_message`, `detail`, `error`, `message`) before falling back
/// to a safely-truncated version of the raw text.
fn extract_error_message(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) {
        for key in &["error_message", "detail", "error", "message"] {
            if let Some(val) = parsed.get(key) {
                let msg = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if !msg.is_empty() {
                    return msg;
                }
            }
        }
    }

    safe_truncate(body, 200)
}

/// Truncate a string to at most `max_chars` characters without panicking
/// on multi-byte UTF-8 boundaries.
fn safe_truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect()
    }
}

/// A module that proxies requests to an HTTP API.
struct ProxyModule {
    base_url: String,
    http_method: String,
    url_path: String,
    path_params: HashSet<String>,
    input_schema: serde_json::Value,
    output_schema: serde_json::Value,
    description: String,
    annotations: ModuleAnnotations,
    timeout_secs: f64,
    auth_header_factory: Option<Arc<dyn Fn() -> HashMap<String, String> + Send + Sync>>,
}

#[async_trait]
impl Module for ProxyModule {
    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    fn output_schema(&self) -> serde_json::Value {
        self.output_schema.clone()
    }

    fn description(&self) -> &str {
        &self.description
    }

    async fn execute(
        &self,
        inputs: serde_json::Value,
        _ctx: &Context<serde_json::Value>,
    ) -> Result<serde_json::Value, ModuleError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs_f64(self.timeout_secs))
            .build()
            .map_err(|e| {
                ModuleError::new(
                    apcore::errors::ErrorCode::ModuleExecuteError,
                    format!("Failed to create HTTP client: {e}"),
                )
            })?;

        let mut actual_path = self.url_path.clone();
        let mut query: HashMap<String, String> = HashMap::new();
        let mut body: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();

        if let Some(obj) = inputs.as_object() {
            for (key, value) in obj {
                if self.path_params.contains(key) {
                    let val_str = match value {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    actual_path = actual_path.replace(&format!("{{{key}}}"), &val_str);
                } else if self.http_method == "GET" {
                    let val_str = match value {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    query.insert(key.clone(), val_str);
                } else {
                    body.insert(key.clone(), value.clone());
                }
            }
        }

        let url = format!("{}{}", self.base_url.trim_end_matches('/'), actual_path);

        let mut request = match self.http_method.as_str() {
            "GET" => client.get(&url),
            "POST" => client.post(&url),
            "PUT" => client.put(&url),
            "PATCH" => client.patch(&url),
            "DELETE" => client.delete(&url),
            other => {
                return Err(ModuleError::new(
                    apcore::errors::ErrorCode::ModuleExecuteError,
                    format!("Unsupported HTTP method: {other}"),
                ))
            }
        };

        // Apply auth headers from the factory, if configured
        if let Some(ref factory) = self.auth_header_factory {
            for (header_name, header_value) in factory() {
                request = request.header(&header_name, &header_value);
            }
        }

        if !query.is_empty() {
            request = request.query(&query.iter().collect::<Vec<_>>());
        }
        if !body.is_empty() && matches!(self.http_method.as_str(), "POST" | "PUT" | "PATCH") {
            request = request.json(&body);
        }

        let resp = request.send().await.map_err(|e| {
            ModuleError::new(
                apcore::errors::ErrorCode::ModuleExecuteError,
                format!("HTTP request failed: {e}"),
            )
        })?;

        let status = resp.status();
        if status.is_success() {
            if status.as_u16() == 204 {
                return Ok(serde_json::json!({}));
            }
            resp.json().await.map_err(|e| {
                ModuleError::new(
                    apcore::errors::ErrorCode::ModuleExecuteError,
                    format!("Failed to parse response JSON: {e}"),
                )
            })
        } else {
            let error_text = resp.text().await.unwrap_or_default();
            let message = extract_error_message(&error_text);
            Err(ModuleError::new(
                apcore::errors::ErrorCode::ModuleExecuteError,
                format!("HTTP {}: {}", status.as_u16(), message),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_get_http_fields_defaults() {
        let module = ScannedModule::new(
            "test".into(),
            "test".into(),
            json!({}),
            json!({}),
            vec![],
            "app:func".into(),
        );
        let (method, path) = get_http_fields(&module);
        assert_eq!(method, "GET");
        assert_eq!(path, "/");
    }

    #[test]
    fn test_get_http_fields_from_metadata() {
        let mut module = ScannedModule::new(
            "test".into(),
            "test".into(),
            json!({}),
            json!({}),
            vec![],
            "app:func".into(),
        );
        module.metadata.insert(
            "http_method".into(),
            serde_json::Value::String("POST".into()),
        );
        module.metadata.insert(
            "url_path".into(),
            serde_json::Value::String("/users".into()),
        );
        let (method, path) = get_http_fields(&module);
        assert_eq!(method, "POST");
        assert_eq!(path, "/users");
    }

    #[test]
    fn test_extract_path_params() {
        let params = extract_path_params("/users/{user_id}/tasks/{task_id}");
        assert!(params.contains("user_id"));
        assert!(params.contains("task_id"));
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn test_extract_path_params_none() {
        let params = extract_path_params("/users");
        assert!(params.is_empty());
    }

    #[test]
    fn test_extract_error_message_json_error_message() {
        let body = r#"{"error_message": "not found"}"#;
        assert_eq!(extract_error_message(body), "not found");
    }

    #[test]
    fn test_extract_error_message_json_detail() {
        let body = r#"{"detail": "unauthorized"}"#;
        assert_eq!(extract_error_message(body), "unauthorized");
    }

    #[test]
    fn test_extract_error_message_json_error() {
        let body = r#"{"error": "bad request"}"#;
        assert_eq!(extract_error_message(body), "bad request");
    }

    #[test]
    fn test_extract_error_message_json_message() {
        let body = r#"{"message": "server error"}"#;
        assert_eq!(extract_error_message(body), "server error");
    }

    #[test]
    fn test_extract_error_message_json_priority() {
        // error_message takes priority over message
        let body = r#"{"error_message": "first", "message": "second"}"#;
        assert_eq!(extract_error_message(body), "first");
    }

    #[test]
    fn test_extract_error_message_plain_text_short() {
        let body = "plain text error";
        assert_eq!(extract_error_message(body), "plain text error");
    }

    #[test]
    fn test_extract_error_message_plain_text_truncated() {
        let body = "x".repeat(300);
        let result = extract_error_message(&body);
        assert_eq!(result.len(), 200);
    }

    #[test]
    fn test_safe_truncate_multibyte() {
        // Each emoji is multiple bytes but one char
        let body = "\u{1F600}".repeat(300);
        let result = safe_truncate(&body, 200);
        assert_eq!(result.chars().count(), 200);
    }
}
