// Provider configuration, extension hooks, and endpoint discovery.
//
// RFC 8628 fixes the shape of the flow, not the URLs, not the extra parameters
// each vendor demands, and not the response encoding. No endpoint is hard-coded
// anywhere in this file, and none may be: per-tenant and per-realm path
// segments make any fixed URL wrong for somebody. Every knob below exists
// because a real, widely-deployed provider requires it -- see "Field Evidence"
// in `apcore-toolkit/docs/features/device-auth.md`.

use std::collections::BTreeMap;
use std::sync::Arc;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::auth::error::DeviceAuthError;
use crate::auth::parse::{decode_body, read_string, ErrorIdentifier, ParsedBody, RequestKind};
use crate::auth::transport::{HttpTransport, ReqwestTransport, SharedTransport};

/// Ordered request parameters or headers. Insertion order is preserved so a
/// request body is reproducible and comparable across the three SDKs.
pub type ParamMap = IndexMap<String, String>;

/// The RFC 8628 grant type, sent verbatim in every token request.
///
/// Some providers advertise the bare `device_code` short name in their
/// metadata while still requiring this full URN on the wire.
pub const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The short name some providers use in `grant_types_supported`.
pub const DEVICE_CODE_GRANT_SHORT_NAME: &str = "device_code";

/// Deadline used when a device response omits `expires_in`, in seconds.
pub const DEFAULT_DEVICE_EXPIRY_SECONDS: u64 = 900;

/// Polling interval used when the device response omits `interval`, in seconds.
pub const DEFAULT_INTERVAL_SECONDS: u64 = 5;

/// Seconds added to the interval on `slow_down`, per RFC 8628 section 3.5.
///
/// A fixed increment, never a multiplier: the server's rate limiter is written
/// against the RFC's behaviour.
pub const SLOW_DOWN_INCREMENT_SECONDS: u64 = 5;

/// How the client authenticates itself, and on which endpoints.
///
/// RFC 8628 targets public clients, but real deployments contradict that in
/// two ways: confidential clients are common, and authentication may apply to
/// the device authorization request as well as the token request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAuthMethod {
    /// `client_id` in the form body, no secret. The RFC's public-client
    /// assumption, and the default.
    #[default]
    None,
    /// `client_id` and `client_secret` in the form body.
    ClientSecretPost,
    /// HTTP Basic header; `client_id` still goes in the body.
    ClientSecretBasic,
}

/// How a request body is encoded, per request kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BodyEncoding {
    /// `application/x-www-form-urlencoded`. The RFC 6749 default.
    #[default]
    Form,
    /// `application/json`.
    Json,
}

/// Body encoding per request kind.
///
/// A separate axis from response encoding, and per-kind rather than global,
/// because one surveyed vendor's single token endpoint takes form-encoded for
/// the code exchange and JSON for refresh. Conflating them makes that
/// combination inexpressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RequestEncoding {
    /// The device authorization request.
    pub device: BodyEncoding,
    /// The initial token request.
    pub token: BodyEncoding,
    /// The refresh exchange -- the one most likely to need JSON.
    pub refresh: BodyEncoding,
    /// Revocation.
    pub revoke: BodyEncoding,
}

impl RequestEncoding {
    /// The encoding configured for one request kind.
    pub fn for_kind(&self, kind: RequestKind) -> BodyEncoding {
        match kind {
            RequestKind::Device => self.device,
            RequestKind::Token => self.token,
            RequestKind::Refresh => self.refresh,
            RequestKind::Revoke => self.revoke,
        }
    }
}

/// `transform_request(kind, params, headers) -> (params, headers)`.
///
/// Runs immediately before each outbound request. Receives no URL and cannot
/// change one: a hook able to redirect the token request is a hook able to
/// exfiltrate credentials.
pub type TransformRequestHook =
    Arc<dyn Fn(RequestKind, ParamMap, ParamMap) -> (ParamMap, ParamMap) + Send + Sync>;

/// `parse_response(kind, status, content_type, raw_body) -> mapping | None`.
///
/// Runs before the built-in parsers. `None` means "no opinion, use the
/// default", so a hook can special-case one endpoint and ignore the rest.
pub type ParseResponseHook =
    Arc<dyn Fn(RequestKind, u16, Option<&str>, &str) -> Option<ParsedBody> + Send + Sync>;

/// `classify_error(body) -> identifier | None`.
///
/// Returns a `String` rather than a typed identifier deliberately: the spec
/// requires an out-of-range return value to be *rejected at runtime*, loudly,
/// and a typed return would make that unrepresentable and the check
/// untestable. Conformance case 045 asserts the rejection.
pub type ClassifyErrorHook = Arc<dyn Fn(&ParsedBody) -> Option<String> + Send + Sync>;

/// The four extension hooks.
///
/// Hooks may change what the client *understands*, never what it *decides*:
/// transport and serialisation are open, normalisation is open with a
/// constrained output, and the protocol-decision layer -- dispatch, backoff,
/// deadline, expiry -- is closed. That boundary is what keeps the conformance
/// corpus meaningful.
#[derive(Clone, Default)]
pub struct Hooks {
    /// Mutate outbound params and headers. Cannot change the URL.
    pub transform_request: Option<TransformRequestHook>,
    /// Custom response decoding; `None` falls back to the built-in parsers.
    pub parse_response: Option<ParseResponseHook>,
    /// Custom error classification; must return an RFC identifier or `None`.
    pub classify_error: Option<ClassifyErrorHook>,
    /// SDK-native HTTP client injection: proxies, custom CA bundles, mTLS,
    /// connection pooling, and test doubles.
    pub http_client: Option<SharedTransport>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn mark<T>(slot: &Option<T>) -> &'static str {
            if slot.is_some() {
                "<hook>"
            } else {
                "None"
            }
        }
        f.debug_struct("Hooks")
            .field("transform_request", &mark(&self.transform_request))
            .field("parse_response", &mark(&self.parse_response))
            .field("classify_error", &mark(&self.classify_error))
            .field("http_client", &mark(&self.http_client))
            .finish()
    }
}

/// Everything the client needs to talk to one authorization server.
///
/// Only `client_id` and a way to reach the endpoints are required; everything
/// else has a working default, so a conforming provider needs three lines of
/// configuration while a non-conforming one stays reachable without patching
/// the toolkit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAuthConfig {
    /// Base URL for discovery. Endpoints are fetched from it by
    /// [`DeviceAuthConfig::discover`] when not given explicitly.
    pub issuer: Option<String>,
    /// Explicit device authorization URL. Overrides discovery.
    pub device_authorization_endpoint: Option<String>,
    /// Explicit token URL. Overrides discovery.
    pub token_endpoint: Option<String>,
    /// RFC 7009 revocation URL. Absent for providers that do not implement it.
    pub revocation_endpoint: Option<String>,
    /// Public client identifier.
    pub client_id: String,
    /// Set for providers that operate device flow as a confidential client.
    pub client_secret: Option<String>,
    /// How and where the client authenticates itself.
    pub client_auth_method: ClientAuthMethod,
    /// Requested scopes. RFC 8628 marks `scope` OPTIONAL, yet at least one
    /// provider rejects a device request without it, so a non-empty value is
    /// the safer default.
    pub scope: Vec<String>,
    /// How scopes are joined. RFC 6749 mandates a space; a minority of
    /// providers expect commas.
    pub scope_separator: String,
    /// Additional form fields on the device authorization request.
    pub extra_device_params: BTreeMap<String, String>,
    /// Additional form fields on every token request.
    pub extra_token_params: BTreeMap<String, String>,
    /// Additional HTTP headers on both requests.
    pub extra_headers: BTreeMap<String, String>,
    /// Maps non-standard error identifiers onto the RFC's four.
    ///
    /// `invalid_grant -> expired_token` is correct for a provider that reports
    /// device-code expiry that way and wrong everywhere else, so it is never
    /// applied by default -- it is opt-in, per provider.
    pub error_aliases: BTreeMap<String, String>,
    /// Extends the accepted response field-name lists. The standard spelling
    /// is always tried first.
    pub field_aliases: BTreeMap<String, Vec<String>>,
    /// Fallback when the provider omits `interval`.
    pub default_interval: u64,
    /// Per-request timeout, distinct from the flow deadline, in seconds.
    pub http_timeout_seconds: Option<f64>,
    /// Body encoding per request kind.
    pub request_encoding: RequestEncoding,
    /// The extension hooks. Not serialised: they are code, not configuration.
    #[serde(skip)]
    pub hooks: Hooks,
}

impl Default for DeviceAuthConfig {
    fn default() -> Self {
        Self {
            issuer: None,
            device_authorization_endpoint: None,
            token_endpoint: None,
            revocation_endpoint: None,
            client_id: String::new(),
            client_secret: None,
            client_auth_method: ClientAuthMethod::None,
            scope: Vec::new(),
            scope_separator: " ".to_string(),
            extra_device_params: BTreeMap::new(),
            extra_token_params: BTreeMap::new(),
            extra_headers: BTreeMap::new(),
            error_aliases: BTreeMap::new(),
            field_aliases: BTreeMap::new(),
            default_interval: DEFAULT_INTERVAL_SECONDS,
            http_timeout_seconds: None,
            request_encoding: RequestEncoding::default(),
            hooks: Hooks::default(),
        }
    }
}

/// Whether an endpoint URL is acceptable: `https://`, or `http://localhost`
/// for development.
///
/// A plaintext token endpoint is refused at construction time, not at request
/// time, so the failure is visible before any credential exists.
pub fn is_secure_endpoint(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// The origin (`scheme://host[:port]`) of a URL, for the discovery
/// same-origin advisory check.
fn origin_of(url: &str) -> Option<String> {
    let scheme_end = url.find("://")? + 3;
    let rest = &url[scheme_end..];
    let host_end = rest.find('/').map(|i| scheme_end + i).unwrap_or(url.len());
    Some(url[..host_end].to_string())
}

/// The three well-known metadata URLs to try, in order.
///
/// RFC 8414 *inserts* its suffix between host and path; OpenID Connect
/// Discovery 1.0 *appends* to the issuer. For an issuer with no path the two
/// collapse to the same shape, which is why the difference goes unnoticed
/// until a tenant- or realm-scoped issuer appears.
pub fn discovery_candidates(issuer: &str) -> Vec<String> {
    let trimmed = issuer.trim_end_matches('/');
    let (origin, path) = match origin_of(trimmed) {
        Some(origin) => {
            let path = trimmed[origin.len()..].to_string();
            (origin, path)
        }
        None => (trimmed.to_string(), String::new()),
    };
    vec![
        format!("{origin}/.well-known/oauth-authorization-server{path}"),
        format!("{origin}/.well-known/openid-configuration{path}"),
        format!("{trimmed}/.well-known/openid-configuration"),
    ]
}

/// What [`DeviceAuthConfig::discover_reported`] observed, so a caller (and the
/// conformance harness) can see the advisory warnings rather than only the
/// resulting configuration.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryReport {
    /// Advisory warnings. Capability detection warns rather than refuses: a
    /// provider under-reporting its grants is more common than one that
    /// genuinely cannot do device flow.
    pub warnings: Vec<String>,
    /// Which candidate succeeded, 1-based.
    pub candidate_used: Option<usize>,
    /// The `issuer` the accepted metadata document declared.
    pub metadata_issuer: Option<String>,
}

impl DeviceAuthConfig {
    /// A configuration with only a client id set.
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            ..Default::default()
        }
    }

    /// Reject a configuration that cannot be safely used.
    ///
    /// Called by [`DeviceAuthClient::new`](crate::auth::DeviceAuthClient::new).
    /// Rust has no constructor to hang this on -- the struct's fields are
    /// public so it reads like the spec's examples -- so validation is an
    /// explicit step that construction of the client performs.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceAuthError::Config`] for an empty `client_id`, a
    /// plaintext endpoint, an `error_aliases` entry whose target is not one of
    /// the four RFC identifiers, or a `client_secret_*` method with no secret.
    pub fn validate(&self) -> Result<(), DeviceAuthError> {
        if self.client_id.trim().is_empty() {
            return Err(DeviceAuthError::Config(
                "client_id must not be empty".into(),
            ));
        }
        for (name, url) in [
            (
                "device_authorization_endpoint",
                &self.device_authorization_endpoint,
            ),
            ("token_endpoint", &self.token_endpoint),
            ("revocation_endpoint", &self.revocation_endpoint),
        ] {
            if let Some(url) = url {
                if !is_secure_endpoint(url) {
                    return Err(DeviceAuthError::Config(format!(
                        "{name} '{url}' must be https:// (http://localhost is allowed for development)"
                    )));
                }
            }
        }
        validate_error_aliases(&self.error_aliases)?;
        if matches!(
            self.client_auth_method,
            ClientAuthMethod::ClientSecretPost | ClientAuthMethod::ClientSecretBasic
        ) && self.client_secret.is_none()
        {
            return Err(DeviceAuthError::Config(
                "client_auth_method requires a client_secret".into(),
            ));
        }
        Ok(())
    }

    /// The store key for this configuration: `"<issuer>|<client_id>"`.
    ///
    /// Falls back to the token endpoint when no issuer is configured, so an
    /// explicit-endpoint setup still gets a stable, non-colliding key.
    pub fn store_key(&self) -> String {
        let issuer = self
            .issuer
            .clone()
            .or_else(|| self.token_endpoint.clone())
            .unwrap_or_default();
        crate::auth::store::store_key(&issuer, &self.client_id)
    }

    /// Resolve endpoints from the issuer's well-known metadata document.
    ///
    /// Network I/O, and therefore an explicit step: never a hidden fetch
    /// inside `login()`. A caller that supplies endpoints explicitly performs
    /// no network access before the flow starts, and an explicitly configured
    /// endpoint is never replaced by a discovered one.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceAuthError::Config`] when no issuer is set or no
    /// candidate yields an authoritative metadata document.
    pub async fn discover(&self) -> Result<DeviceAuthConfig, DeviceAuthError> {
        Ok(self.discover_reported().await?.0)
    }

    /// [`DeviceAuthConfig::discover`], plus what it observed.
    ///
    /// # Errors
    ///
    /// As [`DeviceAuthConfig::discover`].
    pub async fn discover_reported(
        &self,
    ) -> Result<(DeviceAuthConfig, DiscoveryReport), DeviceAuthError> {
        let issuer = self.issuer.as_ref().ok_or_else(|| {
            DeviceAuthError::Config(
                "discover() requires an issuer; configure the endpoints explicitly instead".into(),
            )
        })?;

        let transport: SharedTransport = match &self.hooks.http_client {
            Some(client) => client.clone(),
            None => Arc::new(ReqwestTransport::default()),
        };
        self.discover_with(transport.as_ref(), issuer).await
    }

    /// Discovery against an explicit transport. Split out so the conformance
    /// harness scripts the well-known responses without any network.
    ///
    /// # Errors
    ///
    /// As [`DeviceAuthConfig::discover`].
    pub async fn discover_with(
        &self,
        transport: &dyn HttpTransport,
        issuer: &str,
    ) -> Result<(DeviceAuthConfig, DiscoveryReport), DeviceAuthError> {
        let mut report = DiscoveryReport::default();
        let mut headers = std::collections::HashMap::new();
        headers.insert("Accept".to_string(), "application/json".to_string());
        for (name, value) in &self.extra_headers {
            headers.insert(name.clone(), value.clone());
        }

        for (index, candidate) in discovery_candidates(issuer).iter().enumerate() {
            let response = match transport.get(candidate, &headers).await {
                Ok(response) => response,
                Err(e) => {
                    report
                        .warnings
                        .push(format!("discovery candidate {candidate} failed: {e}"));
                    continue;
                }
            };
            // HTTP 200 does not mean metadata was found: one surveyed provider
            // serves an HTML single-page app at a well-known path. A candidate
            // counts only when it is 2xx AND parses as a JSON object AND
            // carries the fields being looked for.
            if !(200..300).contains(&response.status) {
                continue;
            }
            let Some(metadata) = decode_body(response.content_type.as_deref(), &response.body)
            else {
                continue;
            };
            if !metadata.contains_key("issuer")
                && !metadata.contains_key("token_endpoint")
                && !metadata.contains_key("device_authorization_endpoint")
            {
                continue;
            }
            // Compare issuers as EXACT strings. No case folding, no default
            // port, no trailing slash, no percent-encoding normalisation -- a
            // general-purpose URL type normalises all of those into equality
            // and weakens the check.
            let declared = read_string(&metadata, "issuer", Some(&self.field_aliases));
            match declared.as_deref() {
                Some(value) if value == issuer => {}
                Some(value) => {
                    report.warnings.push(format!(
                        "metadata at {candidate} declares issuer '{value}', which is not \
                         byte-identical to '{issuer}'; rejected"
                    ));
                    warn!(
                        candidate = %candidate,
                        declared = %value,
                        expected = %issuer,
                        "discovery: issuer mismatch, document is not authoritative"
                    );
                    continue;
                }
                None => {
                    report.warnings.push(format!(
                        "metadata at {candidate} declares no issuer; rejected"
                    ));
                    continue;
                }
            }

            report.candidate_used = Some(index + 1);
            report.metadata_issuer = declared;
            return Ok((self.merge_metadata(&metadata, issuer, &mut report)?, report));
        }

        // Carry WHY each candidate was refused. Dropping the per-candidate
        // reasons leaves an operator with "discovery failed" and no way to
        // tell a 404 from a hostile document whose `issuer` did not match --
        // and leaves a conformance harness unable to assert that the issuer
        // check is what fired.
        Err(DeviceAuthError::Config(format!(
            "no authoritative metadata document found for issuer '{issuer}' \
             (tried {} candidates); configure device_authorization_endpoint and \
             token_endpoint explicitly. Candidate results: {}",
            discovery_candidates(issuer).len(),
            if report.warnings.is_empty() {
                "no candidate returned a parseable JSON metadata object".to_string()
            } else {
                report.warnings.join("; ")
            }
        )))
    }

    /// Fold a validated metadata document into a copy of this configuration.
    fn merge_metadata(
        &self,
        metadata: &ParsedBody,
        issuer: &str,
        report: &mut DiscoveryReport,
    ) -> Result<DeviceAuthConfig, DeviceAuthError> {
        let mut resolved = self.clone();

        // Explicit configuration always wins, so a compromised or
        // misconfigured discovery document cannot silently redirect a request.
        for (key, slot) in [
            ("device_authorization_endpoint", 0usize),
            ("token_endpoint", 1),
            ("revocation_endpoint", 2),
        ] {
            let Some(url) = read_string(metadata, key, Some(&self.field_aliases)) else {
                continue;
            };
            if !is_secure_endpoint(&url) {
                return Err(DeviceAuthError::Config(format!(
                    "discovered {key} '{url}' is not https://; refusing to use it"
                )));
            }
            // Same-origin is advisory, not fatal: the spec words it as SHOULD,
            // and the corpus (cases 029 and 030) discovers endpoints on a
            // different host and expects the flow to proceed. Reported so an
            // operator can still see it.
            if origin_of(&url) != origin_of(issuer) {
                let message =
                    format!("discovered {key} '{url}' is not on the issuer's origin '{issuer}'");
                warn!(endpoint = %url, issuer = %issuer, "discovery: {message}");
                report.warnings.push(message);
            }
            match slot {
                0 if resolved.device_authorization_endpoint.is_none() => {
                    resolved.device_authorization_endpoint = Some(url);
                }
                1 if resolved.token_endpoint.is_none() => resolved.token_endpoint = Some(url),
                2 if resolved.revocation_endpoint.is_none() => {
                    resolved.revocation_endpoint = Some(url)
                }
                _ => {}
            }
        }

        if resolved.device_authorization_endpoint.is_none() {
            return Err(DeviceAuthError::Config(format!(
                "the metadata document for '{issuer}' omits device_authorization_endpoint; \
                 set DeviceAuthConfig.device_authorization_endpoint explicitly"
            )));
        }

        // Capability detection is advisory and permissive. One provider
        // advertises the bare `device_code` short name while requiring the full
        // URN on the wire; another publishes no `grant_types_supported` at all.
        // Absence must never imply "unsupported".
        if let Some(serde_json::Value::Array(grants)) = metadata.get("grant_types_supported") {
            let supported = grants.iter().any(|value| {
                matches!(
                    value.as_str(),
                    Some(DEVICE_CODE_GRANT_TYPE) | Some(DEVICE_CODE_GRANT_SHORT_NAME)
                )
            });
            if !supported {
                let message = format!(
                    "the metadata document for '{issuer}' advertises grant_types_supported \
                     without '{DEVICE_CODE_GRANT_TYPE}' or '{DEVICE_CODE_GRANT_SHORT_NAME}'; \
                     proceeding anyway, the server's own rejection is more authoritative"
                );
                warn!(issuer = %issuer, "discovery: {message}");
                report.warnings.push(message);
            }
        }

        Ok(resolved)
    }
}

/// Aliases may only map ONTO the four standard identifiers.
///
/// Allowing new targets would let configuration introduce states the state
/// machine has no branch for. Conformance cases 025 and 025b.
///
/// # Errors
///
/// Returns [`DeviceAuthError::Config`] naming the offending target.
pub fn validate_error_aliases(aliases: &BTreeMap<String, String>) -> Result<(), DeviceAuthError> {
    for (from, to) in aliases {
        if ErrorIdentifier::from_wire(to).is_none() {
            let allowed: Vec<&str> = ErrorIdentifier::ALL.iter().map(|id| id.as_str()).collect();
            return Err(DeviceAuthError::Config(format!(
                "error_aliases['{from}'] maps onto '{to}', which is not one of the four RFC 8628 \
                 identifiers ({}); aliases may only map onto standard identifiers, never invent \
                 new states",
                allowed.join(", ")
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discovery_candidates_path_bearing_issuer() {
        assert_eq!(
            discovery_candidates("https://auth.example.com/tenant1"),
            vec![
                "https://auth.example.com/.well-known/oauth-authorization-server/tenant1",
                "https://auth.example.com/.well-known/openid-configuration/tenant1",
                "https://auth.example.com/tenant1/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn test_discovery_candidates_bare_issuer_collapses() {
        let candidates = discovery_candidates("https://auth.example.com");
        assert_eq!(
            candidates,
            vec![
                "https://auth.example.com/.well-known/oauth-authorization-server",
                "https://auth.example.com/.well-known/openid-configuration",
                "https://auth.example.com/.well-known/openid-configuration",
            ]
        );
        assert_eq!(candidates[1], candidates[2]);
    }

    #[test]
    fn test_discovery_candidates_ignores_trailing_slash_when_building() {
        assert_eq!(
            discovery_candidates("https://auth.example.com/"),
            discovery_candidates("https://auth.example.com")
        );
    }

    #[test]
    fn test_is_secure_endpoint() {
        assert!(is_secure_endpoint("https://a.example/token"));
        assert!(is_secure_endpoint("http://localhost:8080/token"));
        assert!(is_secure_endpoint("http://127.0.0.1/token"));
        assert!(!is_secure_endpoint("http://a.example/token"));
        assert!(!is_secure_endpoint("ftp://a.example/token"));
    }

    #[test]
    fn test_validate_error_aliases_rejects_invented_state() {
        let mut aliases = BTreeMap::new();
        aliases.insert("weird".to_string(), "not_a_standard_identifier".to_string());
        assert!(validate_error_aliases(&aliases).is_err());
    }

    #[test]
    fn test_validate_error_aliases_accepts_standard_target() {
        let mut aliases = BTreeMap::new();
        aliases.insert(
            "authorization_declined".to_string(),
            "access_denied".to_string(),
        );
        assert!(validate_error_aliases(&aliases).is_ok());
    }

    #[test]
    fn test_validate_rejects_plaintext_endpoint() {
        let mut config = DeviceAuthConfig::new("cid");
        config.token_endpoint = Some("http://a.example/token".to_string());
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_empty_client_id() {
        assert!(DeviceAuthConfig::default().validate().is_err());
    }

    #[test]
    fn test_validate_rejects_secret_method_without_secret() {
        let mut config = DeviceAuthConfig::new("cid");
        config.client_auth_method = ClientAuthMethod::ClientSecretBasic;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_request_encoding_defaults_to_form_everywhere() {
        let encoding = RequestEncoding::default();
        for kind in [
            RequestKind::Device,
            RequestKind::Token,
            RequestKind::Refresh,
            RequestKind::Revoke,
        ] {
            assert_eq!(encoding.for_kind(kind), BodyEncoding::Form);
        }
    }

    #[test]
    fn test_store_key_prefers_issuer() {
        let mut config = DeviceAuthConfig::new("cid");
        config.issuer = Some("https://a.example".to_string());
        assert_eq!(config.store_key(), "https://a.example|cid");
    }

    #[test]
    fn test_config_serde_roundtrip_skips_hooks() {
        let config = DeviceAuthConfig::new("cid");
        let json = serde_json::to_string(&config).expect("serialize");
        assert!(!json.contains("hooks"));
        let back: DeviceAuthConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.client_id, "cid");
    }

    #[test]
    fn test_hooks_debug_does_not_leak_closures() {
        let hooks = Hooks {
            classify_error: Some(Arc::new(|_| None)),
            ..Default::default()
        };
        let rendered = format!("{hooks:?}");
        assert!(
            rendered.contains("classify_error: \"<hook>\""),
            "{rendered}"
        );
        assert!(rendered.contains("parse_response: \"None\""), "{rendered}");
    }
}
