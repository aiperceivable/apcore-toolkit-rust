// Outbound request construction: parameters, client authentication, headers,
// and body encoding.
//
// Shared by the device request, the polls, and refresh, so client
// authentication reaches BOTH endpoints -- the easy half to miss. A provider
// can reject an unauthenticated `/device/authorize` with `invalid_client`,
// long before any token request happens.

use indexmap::IndexMap;

use crate::auth::config::{
    BodyEncoding, ClientAuthMethod, DeviceAuthConfig, ParamMap, DEVICE_CODE_GRANT_TYPE,
};
use crate::auth::encoding::{basic_credentials, form_encode};
use crate::auth::parse::RequestKind;
use crate::auth::transport::RequestBody;

/// An outbound request, fully prepared but not yet sent.
///
/// Carries no URL: request targeting stays with configuration and discovery,
/// and `transform_request` never sees one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRequest {
    /// The final parameters, in insertion order.
    pub params: ParamMap,
    /// The final headers, in insertion order.
    pub headers: ParamMap,
    /// The encoded body.
    pub body: RequestBody,
    /// The encoding that produced [`PreparedRequest::body`].
    pub encoding: BodyEncoding,
}

/// The base parameters for a device authorization request.
pub fn device_params(config: &DeviceAuthConfig) -> ParamMap {
    let mut params = ParamMap::new();
    if !config.scope.is_empty() {
        params.insert(
            "scope".to_string(),
            config.scope.join(&config.scope_separator),
        );
    }
    params
}

/// The base parameters for a token poll.
pub fn token_params(device_code: &str) -> ParamMap {
    let mut params = ParamMap::new();
    params.insert("grant_type".to_string(), DEVICE_CODE_GRANT_TYPE.to_string());
    params.insert("device_code".to_string(), device_code.to_string());
    params
}

/// The base parameters for a refresh exchange.
pub fn refresh_params(refresh_token: &str) -> ParamMap {
    let mut params = ParamMap::new();
    params.insert("grant_type".to_string(), "refresh_token".to_string());
    params.insert("refresh_token".to_string(), refresh_token.to_string());
    params
}

/// The base parameters for an RFC 7009 revocation request.
///
/// `token_type_hint` is optional per the RFC and, when given, is a hint only
/// -- servers must still accept the token if the hint is wrong or absent.
pub fn revoke_params(token: &str, token_type_hint: Option<&str>) -> ParamMap {
    let mut params = ParamMap::new();
    params.insert("token".to_string(), token.to_string());
    if let Some(hint) = token_type_hint {
        params.insert("token_type_hint".to_string(), hint.to_string());
    }
    params
}

/// Assemble one outbound request.
///
/// Order of assembly, which is also the order the parameters appear in the
/// body: kind-specific parameters, `client_id`, client authentication, then
/// the configured `extra_*` parameters. `transform_request` runs last, on the
/// complete pair, immediately before the body is encoded.
pub fn prepare_request(
    config: &DeviceAuthConfig,
    kind: RequestKind,
    base: ParamMap,
) -> PreparedRequest {
    let mut params = base;
    params.insert("client_id".to_string(), config.client_id.clone());

    let mut headers: ParamMap = IndexMap::new();
    // Mandatory: some providers return form-urlencoded unless the request asks
    // for JSON, and a client that omits this gets a parse error against an
    // otherwise perfectly conforming server.
    headers.insert("Accept".to_string(), "application/json".to_string());

    apply_client_auth(config, &mut params, &mut headers);

    let extra = match kind {
        RequestKind::Device => &config.extra_device_params,
        _ => &config.extra_token_params,
    };
    for (name, value) in extra {
        params.insert(name.clone(), value.clone());
    }
    for (name, value) in &config.extra_headers {
        headers.insert(name.clone(), value.clone());
    }

    if let Some(hook) = &config.hooks.transform_request {
        let (transformed_params, transformed_headers) = hook(kind, params, headers);
        params = transformed_params;
        headers = transformed_headers;
    }

    let encoding = config.request_encoding.for_kind(kind);
    let body = encode_body(&params, encoding);
    PreparedRequest {
        params,
        headers,
        body,
        encoding,
    }
}

/// Place the client's credentials according to `client_auth_method`.
///
/// `client_id` stays in the body for every method, including
/// `client_secret_basic`: providers ignore credentials they do not require far
/// more gracefully than they accept requests missing ones they do.
fn apply_client_auth(config: &DeviceAuthConfig, params: &mut ParamMap, headers: &mut ParamMap) {
    match config.client_auth_method {
        ClientAuthMethod::None => {}
        ClientAuthMethod::ClientSecretPost => {
            if let Some(secret) = &config.client_secret {
                params.insert("client_secret".to_string(), secret.clone());
            }
        }
        ClientAuthMethod::ClientSecretBasic => {
            if let Some(secret) = &config.client_secret {
                // RFC 6749 section 2.3.1: form-urlencode both halves before
                // base64, not after joining them raw.
                let encoded = basic_credentials(&config.client_id, secret);
                headers.insert("Authorization".to_string(), format!("Basic {encoded}"));
            }
        }
    }
}

/// Encode parameters into a request body.
fn encode_body(params: &ParamMap, encoding: BodyEncoding) -> RequestBody {
    match encoding {
        BodyEncoding::Form => {
            let pairs: Vec<(String, String)> = params
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            RequestBody::Form(form_encode(&pairs))
        }
        BodyEncoding::Json => {
            let mut map = serde_json::Map::new();
            for (key, value) in params {
                map.insert(key.clone(), serde_json::Value::String(value.clone()));
            }
            RequestBody::Json(
                serde_json::to_string(&serde_json::Value::Object(map))
                    .unwrap_or_else(|_| "{}".to_string()),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::config::RequestEncoding;
    use crate::auth::encoding::form_decode;
    use std::sync::Arc;

    fn decoded(request: &PreparedRequest) -> Vec<(String, String)> {
        form_decode(request.body.as_str())
    }

    #[test]
    fn test_prepare_device_request_joins_scope_with_separator() {
        let mut config = DeviceAuthConfig::new("cid");
        config.scope = vec!["openid".into(), "api.read".into()];
        config.scope_separator = ",".into();
        let request = prepare_request(&config, RequestKind::Device, device_params(&config));
        let body = decoded(&request);
        assert!(body.contains(&("scope".to_string(), "openid,api.read".to_string())));
        assert!(body.contains(&("client_id".to_string(), "cid".to_string())));
    }

    #[test]
    fn test_prepare_omits_scope_when_unset() {
        let config = DeviceAuthConfig::new("cid");
        let request = prepare_request(&config, RequestKind::Device, device_params(&config));
        assert!(!request.params.contains_key("scope"));
    }

    #[test]
    fn test_client_secret_basic_uses_header_on_both_endpoints() {
        let mut config = DeviceAuthConfig::new("cid");
        config.client_secret = Some("sec".into());
        config.client_auth_method = ClientAuthMethod::ClientSecretBasic;
        for kind in [RequestKind::Device, RequestKind::Token] {
            let base = match kind {
                RequestKind::Device => device_params(&config),
                _ => token_params("dc"),
            };
            let request = prepare_request(&config, kind, base);
            assert_eq!(
                request.headers.get("Authorization").map(String::as_str),
                Some("Basic Y2lkOnNlYw==")
            );
            assert!(!request.params.contains_key("client_secret"));
            assert_eq!(
                request.params.get("client_id").map(String::as_str),
                Some("cid")
            );
        }
    }

    #[test]
    fn test_client_auth_none_sends_no_secret_anywhere() {
        let mut config = DeviceAuthConfig::new("cid");
        config.client_secret = Some("sec".into());
        let request = prepare_request(&config, RequestKind::Token, token_params("dc"));
        assert!(!request.params.contains_key("client_secret"));
        assert!(!request.headers.contains_key("Authorization"));
    }

    #[test]
    fn test_client_secret_basic_form_encodes_a_secret_with_reserved_bytes() {
        use crate::auth::encoding::base64_encode;

        let mut config = DeviceAuthConfig::new("cid");
        config.client_secret = Some("se:c ret".into());
        config.client_auth_method = ClientAuthMethod::ClientSecretBasic;
        let request = prepare_request(&config, RequestKind::Token, token_params("dc"));
        assert_eq!(
            request.headers.get("Authorization").map(String::as_str),
            Some(format!("Basic {}", base64_encode("cid:se%3Ac+ret")).as_str())
        );
    }

    #[test]
    fn test_client_secret_post_puts_secret_in_body() {
        let mut config = DeviceAuthConfig::new("cid");
        config.client_secret = Some("sec".into());
        config.client_auth_method = ClientAuthMethod::ClientSecretPost;
        let request = prepare_request(&config, RequestKind::Token, token_params("dc"));
        assert_eq!(
            request.params.get("client_secret").map(String::as_str),
            Some("sec")
        );
        assert!(!request.headers.contains_key("Authorization"));
    }

    #[test]
    fn test_per_kind_encoding_json_refresh_form_token() {
        let mut config = DeviceAuthConfig::new("cid");
        config.request_encoding = RequestEncoding {
            refresh: BodyEncoding::Json,
            ..Default::default()
        };
        let token = prepare_request(&config, RequestKind::Token, token_params("dc"));
        let refresh = prepare_request(&config, RequestKind::Refresh, refresh_params("r1"));
        assert_eq!(token.encoding, BodyEncoding::Form);
        assert_eq!(refresh.encoding, BodyEncoding::Json);
        assert!(matches!(token.body, RequestBody::Form(_)));
        assert!(matches!(refresh.body, RequestBody::Json(_)));
        assert!(refresh.body.as_str().starts_with('{'));
    }

    #[test]
    fn test_transform_request_reaches_body_and_headers() {
        let mut config = DeviceAuthConfig::new("cid");
        config.hooks.transform_request = Some(Arc::new(|_kind, mut params, mut headers| {
            params.insert("audience".to_string(), "https://api.example".to_string());
            headers.insert("X-Vendor".to_string(), "1".to_string());
            (params, headers)
        }));
        let request = prepare_request(&config, RequestKind::Device, device_params(&config));
        assert!(decoded(&request)
            .contains(&("audience".to_string(), "https://api.example".to_string())));
        assert_eq!(
            request.headers.get("X-Vendor").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn test_accept_header_is_always_json() {
        let config = DeviceAuthConfig::new("cid");
        let request = prepare_request(&config, RequestKind::Token, token_params("dc"));
        assert_eq!(
            request.headers.get("Accept").map(String::as_str),
            Some("application/json")
        );
    }

    #[test]
    fn test_revoke_params_include_token_and_hint() {
        let params = revoke_params("tok", Some("access_token"));
        assert_eq!(params.get("token").map(String::as_str), Some("tok"));
        assert_eq!(
            params.get("token_type_hint").map(String::as_str),
            Some("access_token")
        );
    }

    #[test]
    fn test_revoke_params_omit_hint_when_none() {
        let params = revoke_params("tok", None);
        assert!(!params.contains_key("token_type_hint"));
    }

    #[test]
    fn test_prepare_revoke_request_carries_client_id_and_token() {
        let config = DeviceAuthConfig::new("cid");
        let request = prepare_request(
            &config,
            RequestKind::Revoke,
            revoke_params("tok", Some("access_token")),
        );
        let body = decoded(&request);
        assert!(body.contains(&("token".to_string(), "tok".to_string())));
        assert!(body.contains(&("client_id".to_string(), "cid".to_string())));
    }

    #[test]
    fn test_extra_params_are_kind_scoped() {
        let mut config = DeviceAuthConfig::new("cid");
        config
            .extra_device_params
            .insert("audience".into(), "https://api".into());
        config
            .extra_token_params
            .insert("tenant".into(), "t1".into());
        let device = prepare_request(&config, RequestKind::Device, device_params(&config));
        let token = prepare_request(&config, RequestKind::Token, token_params("dc"));
        assert!(device.params.contains_key("audience"));
        assert!(!device.params.contains_key("tenant"));
        assert!(token.params.contains_key("tenant"));
        assert!(!token.params.contains_key("audience"));
    }
}
