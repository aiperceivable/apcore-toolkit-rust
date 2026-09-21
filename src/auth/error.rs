// Error types for the device authorization flow.
//
// The variants mirror the error names in
// `apcore-toolkit/docs/features/device-auth.md` ("Contract:
// DeviceAuthClient.login" and "Contract: DeviceAuthClient.ensure_valid") so a
// porter reading the spec finds the same set in all three SDKs.

use thiserror::Error;

/// Why an authorization stopped without a token.
///
/// The spec models device-code expiry and the client-side deadline as one
/// error (`AuthorizationExpiredError`), but the conformance corpus reports
/// them as two distinct outcomes (`expired_token` and `deadline_exceeded`), so
/// the cause is carried on the variant rather than collapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiryCause {
    /// The authorization server returned `expired_token`.
    ServerExpiredToken,
    /// The client's own deadline elapsed first: `expires_in` from the device
    /// response (or the 15-minute fallback), or `timeout_seconds`, whichever
    /// was shorter.
    DeadlineElapsed,
}

impl ExpiryCause {
    /// Stable identifier used in messages and by the conformance harness.
    pub fn as_str(&self) -> &'static str {
        match self {
            ExpiryCause::ServerExpiredToken => "expired_token",
            ExpiryCause::DeadlineElapsed => "deadline_exceeded",
        }
    }
}

/// Errors produced by the device authorization client.
#[derive(Debug, Error)]
pub enum DeviceAuthError {
    /// The authorization server returned `access_denied`: the user refused.
    #[error("authorization denied by the user (access_denied)")]
    AuthorizationDenied,

    /// The device code expired, or the client's own deadline elapsed.
    #[error("authorization expired ({})", .0.as_str())]
    AuthorizationExpired(ExpiryCause),

    /// An unrecognised error identifier, a malformed body, or a response the
    /// client cannot interpret. Carries the raw body when one was received, so
    /// an operator keeps the only diagnostic a proprietary error envelope
    /// offers (spec: "fail soft on shape").
    #[error("device authorization protocol error: {message}")]
    Protocol {
        /// Human-readable description of what could not be interpreted.
        message: String,
        /// The undecoded response body, when the failure came from one.
        raw_body: Option<String>,
    },

    /// Nothing is stored for this issuer and client id; the caller must
    /// `login()` first.
    #[error("no stored credential for '{key}'; call login() first")]
    NoCredential {
        /// The store key that was looked up (`"<issuer>|<client_id>"`).
        key: String,
    },

    /// A refresh was rejected with `invalid_grant`. The stored record has been
    /// cleared as a side effect; the refresh token is spent or revoked.
    #[error("refresh failed and the stored credential was cleared: {message}")]
    RefreshFailed {
        /// Server-reported reason.
        message: String,
    },

    /// A transport failure that reached a caller. Transport failures *during
    /// polling* are retried rather than raised; this surfaces the ones outside
    /// that loop (discovery, refresh).
    #[error("transport error: {0}")]
    Transport(String),

    /// Invalid configuration, rejected before any network access.
    #[error("invalid device auth configuration: {0}")]
    Config(String),

    /// A `classify_error` hook returned a value outside the four RFC 8628
    /// identifiers. Never coerced, never defaulted (conformance case 045).
    #[error("classify_error hook returned an invalid identifier: {0:?}")]
    InvalidHookReturn(String),

    /// A token store operation failed.
    #[error(transparent)]
    Store(#[from] TokenStoreError),
}

/// Errors produced by a [`TokenStore`](crate::auth::TokenStore) implementation.
#[derive(Debug, Error)]
pub enum TokenStoreError {
    /// An existing credentials file is readable by users other than its owner.
    /// The store refuses to read it rather than silently using a credential
    /// other local users can see.
    #[error(
        "credentials file {path} has mode {mode:04o}, which is broader than 0600; \
         refusing to read it (run: chmod 600 {path})"
    )]
    Permission {
        /// Path of the offending file.
        path: String,
        /// The POSIX mode bits that were found.
        mode: u32,
    },

    /// The store file exists but does not hold a JSON object of records.
    #[error("credentials file {path} is not valid JSON: {message}")]
    Corrupt {
        /// Path of the offending file.
        path: String,
        /// Parser message.
        message: String,
    },

    /// An underlying filesystem error.
    #[error("token store I/O error at {path}: {source}")]
    Io {
        /// Path being operated on.
        path: String,
        /// The originating I/O error.
        source: std::io::Error,
    },
}
