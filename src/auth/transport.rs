// The HTTP seam.
//
// This is the `http_client` extension hook from the spec, expressed as a trait
// so it covers the whole of its stated purpose: "proxies, custom CA bundles,
// mTLS, connection pooling, and test doubles". The polling state machine is
// pure over this trait plus an injected clock, which is why the conformance
// corpus needs no HTTP mocking -- a scripted implementation of
// [`HttpTransport`] IS the scripted response sequence.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

/// A transport failure: a connection reset, a DNS failure, a timeout.
///
/// Distinct from an HTTP error response, because the dispatch table treats the
/// two differently -- a transport failure is retried, an error *body* is
/// dispatched on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    /// Human-readable reason, for logs and for the eventual error message.
    pub message: String,
}

impl TransportError {
    /// Build a transport error from anything renderable.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A raw HTTP response, undecoded.
///
/// The body stays a string: parsing is a separate, hookable step, and the raw
/// text is what a fail-soft protocol error carries back to the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status. Used for exactly one decision: success payload versus
    /// error payload. Never for dispatch.
    pub status: u16,
    /// The `Content-Type` header, when present.
    pub content_type: Option<String>,
    /// The undecoded response body.
    pub body: String,
}

/// How a request body is encoded on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestBody {
    /// `application/x-www-form-urlencoded`.
    Form(String),
    /// `application/json`.
    Json(String),
    /// No body (used by discovery's `GET`).
    Empty,
}

impl RequestBody {
    /// The body text as it goes on the wire.
    pub fn as_str(&self) -> &str {
        match self {
            RequestBody::Form(text) | RequestBody::Json(text) => text,
            RequestBody::Empty => "",
        }
    }

    /// The `Content-Type` this body requires, if any.
    pub fn content_type(&self) -> Option<&'static str> {
        match self {
            RequestBody::Form(_) => Some("application/x-www-form-urlencoded"),
            RequestBody::Json(_) => Some("application/json"),
            RequestBody::Empty => None,
        }
    }
}

/// The HTTP operations the device auth client performs.
///
/// Implement this to inject a configured client, a proxy, or a test double.
/// [`ReqwestTransport`] is the shipped default.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Issue a `POST`. Transport failures are returned as `Err`; any HTTP
    /// status, including 4xx and 5xx, is returned as `Ok`.
    async fn post(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
        body: RequestBody,
    ) -> Result<HttpResponse, TransportError>;

    /// Issue a `GET`. Used by endpoint discovery.
    async fn get(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<HttpResponse, TransportError>;
}

/// The default [`HttpTransport`], backed by `reqwest`.
///
/// Construct it from a caller-supplied `reqwest::Client` to get proxies, a
/// custom CA bundle, or mTLS, matching the precedent `HTTPProxyRegistryWriter`
/// already sets for SDK-native client injection.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl std::fmt::Debug for ReqwestTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReqwestTransport")
            .field("client", &self.client)
            .finish()
    }
}

impl ReqwestTransport {
    /// Wrap an existing `reqwest::Client`.
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Build a transport with a per-request timeout.
    ///
    /// # Errors
    ///
    /// Returns the underlying `reqwest` error when the client cannot be built.
    pub fn with_timeout(timeout: std::time::Duration) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder().timeout(timeout).build()?,
        })
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

/// Read a `reqwest` response into the transport's raw form.
async fn collect(response: reqwest::Response) -> Result<HttpResponse, TransportError> {
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response
        .text()
        .await
        .map_err(|e| TransportError::new(format!("failed to read response body: {e}")))?;
    Ok(HttpResponse {
        status,
        content_type,
        body,
    })
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn post(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
        body: RequestBody,
    ) -> Result<HttpResponse, TransportError> {
        let mut request = self.client.post(url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(content_type) = body.content_type() {
            request = request.header("Content-Type", content_type);
        }
        let response = request
            .body(body.as_str().to_string())
            .send()
            .await
            .map_err(|e| TransportError::new(e.to_string()))?;
        collect(response).await
    }

    async fn get(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<HttpResponse, TransportError> {
        let mut request = self.client.get(url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|e| TransportError::new(e.to_string()))?;
        collect(response).await
    }
}

/// Shared handle to a transport, so a client and its grants use one instance.
pub type SharedTransport = Arc<dyn HttpTransport>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_body_content_types() {
        assert_eq!(
            RequestBody::Form("a=1".into()).content_type(),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(
            RequestBody::Json("{}".into()).content_type(),
            Some("application/json")
        );
        assert_eq!(RequestBody::Empty.content_type(), None);
        assert_eq!(RequestBody::Empty.as_str(), "");
    }

    #[test]
    fn test_transport_error_displays_message() {
        assert_eq!(TransportError::new("reset").to_string(), "reset");
    }
}
