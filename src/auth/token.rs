// TokenSet: the credential a successful grant produces.
//
// The access token is opaque. It is never decoded, never parsed for `exp` /
// `sub` / `roles`, and never used to construct an apcore `Identity` -- see
// "Boundary: this produces tokens, not identities" in
// `apcore-toolkit/docs/features/device-auth.md`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Default clock skew, in seconds, applied by [`TokenSet::is_expired`].
///
/// A token is treated as expired slightly before it really is, so a request is
/// not dispatched with a credential that expires in flight.
pub const DEFAULT_SKEW_SECONDS: i64 = 30;

/// Placeholder substituted for token values in the [`std::fmt::Debug`] output.
pub const REDACTED: &str = "<redacted>";

/// An OAuth credential: opaque token values plus the lifecycle metadata the
/// client needs to decide when to refresh.
///
/// `expires_at` is stored as an absolute wall-clock instant (Unix seconds)
/// rather than a duration, because a duration is meaningless after a process
/// restart. The *polling* deadline uses a monotonic clock instead; the two
/// clocks are deliberately different, see the spec's "Two clocks" note.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSet {
    /// The bearer credential. Opaque: never parsed.
    pub access_token: String,
    /// Normalised to `"Bearer"` casing on read; servers vary.
    pub token_type: String,
    /// Wall-clock Unix seconds at which the access token expires. `None` when
    /// the server stated no lifetime, in which case the token never
    /// auto-expires.
    pub expires_at: Option<i64>,
    /// Present only when the server issued one. Rotation is assumed.
    pub refresh_token: Option<String>,
    /// Granted scopes, split on spaces per RFC 6749. Empty when absent.
    pub scope: Vec<String>,
    /// Wall-clock Unix seconds at which this record was created. Diagnostics
    /// and store-format migration only.
    pub obtained_at: i64,
}

/// Redacted by hand rather than derived: `#[derive(Debug)]` would print both
/// token values, and a leaked debug log is the most common way CLI credentials
/// escape. Conformance case `device_auth_redaction_020` asserts this.
///
/// Follows the same shape as `HTTPProxyRegistryWriter`'s hand-written `Debug`,
/// which hides its `auth_header_factory` the same way.
impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &REDACTED)
            .field("token_type", &self.token_type)
            .field("expires_at", &self.expires_at)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| REDACTED),
            )
            .field("scope", &self.scope)
            .field("obtained_at", &self.obtained_at)
            .finish()
    }
}

/// Same redaction for `to_string()` / `{}` as for `{:?}`.
impl std::fmt::Display for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl TokenSet {
    /// Normalise a server-supplied `token_type`.
    ///
    /// Only the `bearer` spelling is re-cased; an unrecognised type is passed
    /// through verbatim rather than mangled, and an absent one defaults to
    /// `"Bearer"`.
    pub fn normalise_token_type(raw: Option<&str>) -> String {
        match raw {
            None => "Bearer".to_string(),
            Some(value) if value.eq_ignore_ascii_case("bearer") => "Bearer".to_string(),
            Some(value) => value.to_string(),
        }
    }

    /// Split an RFC 6749 space-delimited scope string into a list.
    ///
    /// Returns an empty list for `None` or an all-whitespace value.
    pub fn split_scope(raw: Option<&str>) -> Vec<String> {
        raw.map(|value| {
            value
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
    }

    /// Whether this token should be treated as expired at wall-clock `now`.
    ///
    /// True when `now + skew_seconds >= expires_at`. A token with no
    /// `expires_at` never auto-expires.
    pub fn is_expired(&self, now: i64, skew_seconds: i64) -> bool {
        match self.expires_at {
            None => false,
            Some(expires_at) => now + skew_seconds >= expires_at,
        }
    }

    /// The complete header mapping that carries this credential.
    ///
    /// A mapping rather than a bare token string, because the header *name* is
    /// provider data: `Authorization: Bearer`, `x-api-key`, and `api-key` are
    /// all in live use. See "Authentication header shape" in the spec.
    pub fn auth_headers(&self) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        headers.insert(
            "Authorization".to_string(),
            format!("{} {}", self.token_type, self.access_token),
        );
        headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TokenSet {
        TokenSet {
            access_token: "SECRET-ACCESS-VALUE".to_string(),
            token_type: "Bearer".to_string(),
            expires_at: Some(1000),
            refresh_token: Some("SECRET-REFRESH-VALUE".to_string()),
            scope: vec!["openid".to_string()],
            obtained_at: 1,
        }
    }

    #[test]
    fn test_token_set_debug_redacts_both_tokens() {
        let rendered = format!("{:?}", sample());
        assert!(!rendered.contains("SECRET-ACCESS-VALUE"), "{rendered}");
        assert!(!rendered.contains("SECRET-REFRESH-VALUE"), "{rendered}");
        assert!(rendered.contains(REDACTED));
        // Non-secret fields stay visible, or the Debug impl is useless.
        assert!(rendered.contains("Bearer"));
        assert!(rendered.contains("1000"));
    }

    #[test]
    fn test_token_set_display_redacts_both_tokens() {
        let rendered = sample().to_string();
        assert!(!rendered.contains("SECRET-ACCESS-VALUE"));
        assert!(!rendered.contains("SECRET-REFRESH-VALUE"));
    }

    #[test]
    fn test_token_set_debug_hides_absent_refresh_token() {
        let mut token = sample();
        token.refresh_token = None;
        let rendered = format!("{token:?}");
        assert!(rendered.contains("refresh_token: None"), "{rendered}");
    }

    #[test]
    fn test_normalise_token_type_recases_bearer_only() {
        assert_eq!(TokenSet::normalise_token_type(Some("bearer")), "Bearer");
        assert_eq!(TokenSet::normalise_token_type(Some("BEARER")), "Bearer");
        assert_eq!(TokenSet::normalise_token_type(Some("Bearer")), "Bearer");
        assert_eq!(TokenSet::normalise_token_type(None), "Bearer");
        assert_eq!(TokenSet::normalise_token_type(Some("DPoP")), "DPoP");
    }

    #[test]
    fn test_split_scope_handles_absent_and_multiple() {
        assert_eq!(TokenSet::split_scope(None), Vec::<String>::new());
        assert_eq!(TokenSet::split_scope(Some("   ")), Vec::<String>::new());
        assert_eq!(
            TokenSet::split_scope(Some("openid api.read")),
            vec!["openid".to_string(), "api.read".to_string()]
        );
    }

    #[test]
    fn test_is_expired_skew_window() {
        let token = sample();
        assert!(token.is_expired(975, 30), "975 + 30 >= 1000");
        assert!(!token.is_expired(900, 30), "900 + 30 < 1000");
    }

    #[test]
    fn test_is_expired_absent_expiry_never_expires() {
        let mut token = sample();
        token.expires_at = None;
        assert!(!token.is_expired(99_999_999, 30));
    }

    #[test]
    fn test_auth_headers_uses_token_type() {
        let headers = sample().auth_headers();
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer SECRET-ACCESS-VALUE")
        );
    }

    #[test]
    fn test_token_set_serde_roundtrip() {
        let token = sample();
        let json = serde_json::to_string(&token).expect("serialize");
        let back: TokenSet = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, token);
    }
}
