// Response parsing and normalisation.
//
// Two independent axes, kept separate on purpose:
//
//   * *Encoding* -- JSON, with `application/x-www-form-urlencoded` as the
//     mandated fallback. Some servers return form-encoded unless the request
//     asks otherwise, so this is conformance, not a courtesy.
//   * *Field names* -- providers rename fields (`verification_url`,
//     `error_code`), so each logical field is read from an ordered list of
//     accepted names, standard spelling first.
//
// Unknown fields are ignored, never rejected: providers add proprietary keys
// to both responses and a strict parser breaks against a working server.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::auth::encoding::form_decode;

/// The four RFC 8628 error identifiers the state machine dispatches on.
///
/// Normalisation happens *before* dispatch, so a provider's non-standard
/// spelling reaches the table already mapped onto one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorIdentifier {
    /// The user has not yet completed the authorization. Keep polling.
    AuthorizationPending,
    /// Polling too fast. Back off, see the spec's "Backoff on slow_down".
    SlowDown,
    /// The user refused. Terminal.
    AccessDenied,
    /// The device code expired. Terminal.
    ExpiredToken,
}

impl ErrorIdentifier {
    /// The wire spelling of this identifier.
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorIdentifier::AuthorizationPending => "authorization_pending",
            ErrorIdentifier::SlowDown => "slow_down",
            ErrorIdentifier::AccessDenied => "access_denied",
            ErrorIdentifier::ExpiredToken => "expired_token",
        }
    }

    /// Parse a wire spelling. Returns `None` for anything outside the four --
    /// which is what makes aliases and `classify_error` unable to invent a
    /// fifth state.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "authorization_pending" => Some(ErrorIdentifier::AuthorizationPending),
            "slow_down" => Some(ErrorIdentifier::SlowDown),
            "access_denied" => Some(ErrorIdentifier::AccessDenied),
            "expired_token" => Some(ErrorIdentifier::ExpiredToken),
            _ => None,
        }
    }

    /// Every valid identifier, for error messages and validation.
    pub const ALL: [ErrorIdentifier; 4] = [
        ErrorIdentifier::AuthorizationPending,
        ErrorIdentifier::SlowDown,
        ErrorIdentifier::AccessDenied,
        ErrorIdentifier::ExpiredToken,
    ];
}

/// Which request a response belongs to. Also selects the body encoding and is
/// passed to `transform_request` and `parse_response`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequestKind {
    /// The device authorization request.
    Device,
    /// The initial token request (the polls).
    Token,
    /// A refresh-token exchange.
    Refresh,
    /// An RFC 7009 revocation.
    Revoke,
}

impl RequestKind {
    /// The wire name used in configuration keys and hook arguments.
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestKind::Device => "device",
            RequestKind::Token => "token",
            RequestKind::Refresh => "refresh",
            RequestKind::Revoke => "revoke",
        }
    }

    /// Parse a configuration key.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "device" => Some(RequestKind::Device),
            "token" => Some(RequestKind::Token),
            "refresh" => Some(RequestKind::Refresh),
            "revoke" => Some(RequestKind::Revoke),
            _ => None,
        }
    }
}

/// A decoded response body: a flat mapping of field name to JSON value.
///
/// Flat because both wire encodings are flat, and because a form-encoded body
/// can only ever produce strings -- so `expires_in` may arrive as `"3600"`
/// rather than `3600` and every reader has to tolerate both.
pub type ParsedBody = Map<String, Value>;

/// Decode a response body.
///
/// JSON when the content type says so *or* when the body parses as a JSON
/// object; form-urlencoded otherwise. Returns `None` when neither works, which
/// the caller turns into a fail-soft protocol error carrying the raw body.
pub fn decode_body(content_type: Option<&str>, raw_body: &str) -> Option<ParsedBody> {
    let looks_json = content_type
        .map(|value| value.to_ascii_lowercase().contains("json"))
        .unwrap_or(false);

    if looks_json || raw_body.trim_start().starts_with('{') {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(raw_body) {
            return Some(map);
        }
        // A content type that claims JSON but does not parse falls through to
        // the form parser rather than failing outright: the body is the only
        // diagnostic the operator gets, and the spec requires failing soft.
    }

    let pairs = form_decode(raw_body);
    if pairs.is_empty() {
        return None;
    }
    // A body that is not form-encoded at all (HTML, XML) decodes into a single
    // key-less pair; that is not a response, so reject it.
    if pairs.len() == 1 && pairs[0].1.is_empty() && !raw_body.contains('=') {
        return None;
    }
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert(key, Value::String(value));
    }
    Some(map)
}

/// The accepted names for each logical field, standard spelling first.
///
/// Extended per-provider through `field_aliases`; the built-in lists cover the
/// two divergences the field survey found often enough to deserve first-class
/// support.
pub fn default_field_aliases() -> BTreeMap<String, Vec<String>> {
    let mut aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
    aliases.insert(
        "verification_uri".to_string(),
        vec![
            "verification_uri".to_string(),
            "verification_url".to_string(),
        ],
    );
    aliases.insert(
        "verification_uri_complete".to_string(),
        vec![
            "verification_uri_complete".to_string(),
            "verification_url_complete".to_string(),
        ],
    );
    aliases.insert(
        "error".to_string(),
        vec!["error".to_string(), "error_code".to_string()],
    );
    aliases
}

/// Read one logical field, trying each accepted name in order.
///
/// `extra` extends (never replaces) the built-in list, and the standard
/// spelling is always tried first, so a conforming provider is unaffected.
pub fn read_field<'a>(
    body: &'a ParsedBody,
    logical: &str,
    extra: Option<&BTreeMap<String, Vec<String>>>,
) -> Option<&'a Value> {
    let defaults = default_field_aliases();
    let mut names: Vec<String> = defaults
        .get(logical)
        .cloned()
        .unwrap_or_else(|| vec![logical.to_string()]);
    if let Some(map) = extra.and_then(|map| map.get(logical)) {
        for name in map {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    names.iter().find_map(|name| body.get(name))
}

/// Read a logical field as a string.
///
/// Numbers are rendered rather than rejected, because a form-encoded body
/// yields strings and a JSON one yields numbers for the same field.
pub fn read_string(
    body: &ParsedBody,
    logical: &str,
    extra: Option<&BTreeMap<String, Vec<String>>>,
) -> Option<String> {
    match read_field(body, logical, extra)? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// Read a logical field as an integer, tolerating the string form.
pub fn read_i64(
    body: &ParsedBody,
    logical: &str,
    extra: Option<&BTreeMap<String, Vec<String>>>,
) -> Option<i64> {
    match read_field(body, logical, extra)? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Normalise a device-authorization response into standard field names.
///
/// `user_code` is copied byte for byte: no upper-casing, no stripping, no
/// re-grouping. It is case-sensitive at some providers and at least one
/// embeds it unmodified into a URL query parameter.
pub fn normalise_device_body(
    body: &ParsedBody,
    extra: Option<&BTreeMap<String, Vec<String>>>,
) -> ParsedBody {
    let mut out = body.clone();
    for logical in [
        "verification_uri",
        "verification_uri_complete",
        "error",
        "device_code",
        "user_code",
        "expires_in",
        "interval",
    ] {
        if let Some(value) = read_field(body, logical, extra) {
            out.insert(logical.to_string(), value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(raw: &str) -> ParsedBody {
        decode_body(Some("application/json"), raw).expect("decodes")
    }

    #[test]
    fn test_error_identifier_roundtrip() {
        for identifier in ErrorIdentifier::ALL {
            assert_eq!(
                ErrorIdentifier::from_wire(identifier.as_str()),
                Some(identifier)
            );
        }
        assert_eq!(ErrorIdentifier::from_wire("invalid_grant"), None);
        assert_eq!(ErrorIdentifier::from_wire("server_on_fire"), None);
    }

    #[test]
    fn test_request_kind_roundtrip() {
        for kind in [
            RequestKind::Device,
            RequestKind::Token,
            RequestKind::Refresh,
            RequestKind::Revoke,
        ] {
            assert_eq!(RequestKind::from_wire(kind.as_str()), Some(kind));
        }
        assert_eq!(RequestKind::from_wire("nope"), None);
    }

    #[test]
    fn test_decode_body_json() {
        let body = json(r#"{"access_token":"t","expires_in":3600}"#);
        assert_eq!(body["access_token"], Value::String("t".into()));
        assert_eq!(read_i64(&body, "expires_in", None), Some(3600));
    }

    #[test]
    fn test_decode_body_form_urlencoded() {
        let body = decode_body(
            Some("application/x-www-form-urlencoded"),
            "access_token=t&token_type=Bearer&expires_in=3600",
        )
        .expect("decodes");
        assert_eq!(body["expires_in"], Value::String("3600".into()));
        // A form body yields strings; readers must tolerate that.
        assert_eq!(read_i64(&body, "expires_in", None), Some(3600));
    }

    #[test]
    fn test_decode_body_rejects_html() {
        assert!(decode_body(Some("text/html"), "<!doctype html><html>").is_none());
    }

    #[test]
    fn test_decode_body_json_without_content_type() {
        assert!(decode_body(None, r#"{"a":1}"#).is_some());
    }

    #[test]
    fn test_read_field_prefers_standard_spelling() {
        let body = json(r#"{"verification_uri":"std","verification_url":"alias"}"#);
        assert_eq!(
            read_string(&body, "verification_uri", None).as_deref(),
            Some("std")
        );
    }

    #[test]
    fn test_read_field_falls_back_to_alias() {
        let body = json(r#"{"verification_url":"alias"}"#);
        assert_eq!(
            read_string(&body, "verification_uri", None).as_deref(),
            Some("alias")
        );
    }

    #[test]
    fn test_read_field_honours_configured_extra_alias() {
        let body = json(r#"{"vendor_uri":"x"}"#);
        let mut extra: BTreeMap<String, Vec<String>> = BTreeMap::new();
        extra.insert(
            "verification_uri".to_string(),
            vec!["vendor_uri".to_string()],
        );
        assert_eq!(
            read_string(&body, "verification_uri", Some(&extra)).as_deref(),
            Some("x")
        );
    }

    #[test]
    fn test_normalise_device_body_keeps_unknown_fields() {
        let body = json(
            r#"{"device_code":"d","user_code":"AB-CD","verification_url":"u",
                "expires_in":600,"message":"hi","x_vendor":{"a":1}}"#,
        );
        let normalised = normalise_device_body(&body, None);
        assert_eq!(normalised["verification_uri"], Value::String("u".into()));
        assert!(normalised.contains_key("message"));
        assert!(normalised.contains_key("x_vendor"));
    }

    #[test]
    fn test_normalise_device_body_leaves_user_code_verbatim() {
        let body = json(r#"{"user_code":"wdjb-mjht"}"#);
        assert_eq!(
            normalise_device_body(&body, None)["user_code"],
            Value::String("wdjb-mjht".into())
        );
    }

    #[test]
    fn test_read_error_identifier_from_error_code_field() {
        let body = json(r#"{"error_code":"authorization_pending"}"#);
        assert_eq!(
            read_string(&body, "error", None).as_deref(),
            Some("authorization_pending")
        );
    }
}
