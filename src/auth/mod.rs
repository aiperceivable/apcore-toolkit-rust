//! RFC 8628 Device Authorization Flow client -- protocol only, no terminal UI.
//!
//! Feature-gated behind `device-auth` (on by default, like `http-proxy`, so a
//! vanilla `cargo add apcore-toolkit` exposes the same public surface as the
//! Python and TypeScript SDKs). `default-features = false` yields a build with
//! neither `reqwest` nor `tokio`.
//!
//! # What this is, and what it is not
//!
//! This is the **protocol half**: the polling state machine, token lifecycle
//! (expiry, refresh, persistence), and a portable storage protocol. The
//! **presentation half** -- displaying the user code, opening a browser,
//! rendering a spinner -- belongs to the consumer and is reached through
//! callbacks. The toolkit writes nothing to a terminal: no print, no spinner,
//! no colour, no browser launch. A library that writes to stdout cannot be
//! used by a daemon, a GUI, or a test.
//!
//! It also produces **tokens, not identities**. The access token is opaque: it
//! is never decoded, and no `Identity` is constructed from it. `Identity` is
//! the output of *verifying* a credential, and only the party holding the
//! verification key can produce one honestly.
//!
//! # Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use apcore_toolkit::auth::{
//!     DeviceAuthClient, DeviceAuthConfig, FileTokenStore, LoginCallbacks,
//! };
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut config = DeviceAuthConfig::new("apcore-cli");
//! config.issuer = Some("https://auth.example.com".into());
//! config.scope = vec!["openid".into(), "api.read".into()];
//!
//! // Discovery is an explicit network step, never a hidden fetch in login().
//! let config = config.discover().await?;
//!
//! let client = DeviceAuthClient::new(config, Arc::new(FileTokenStore::default()))?;
//! let callbacks = LoginCallbacks {
//!     on_user_code: Some(Arc::new(|event| {
//!         // The consumer renders; the toolkit never does.
//!         tracing::info!(uri = %event.verification_uri, code = %event.user_code, "visit");
//!     })),
//!     ..Default::default()
//! };
//! let tokens = client.login(&callbacks).await?;
//! let headers = client.as_auth_header_factory();
//! # let _ = (tokens, headers);
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod config;
pub mod encoding;
pub mod error;
pub mod grant;
pub mod parse;
pub mod request;
pub mod store;
pub mod token;
pub mod transport;

pub use client::DeviceAuthClient;
pub use config::{
    discovery_candidates, is_secure_endpoint, validate_error_aliases, BodyEncoding,
    ClassifyErrorHook, ClientAuthMethod, DeviceAuthConfig, DiscoveryReport, Hooks, ParamMap,
    ParseResponseHook, RequestEncoding, TransformRequestHook, DEFAULT_DEVICE_EXPIRY_SECONDS,
    DEFAULT_INTERVAL_SECONDS, DEVICE_CODE_GRANT_SHORT_NAME, DEVICE_CODE_GRANT_TYPE,
    SLOW_DOWN_INCREMENT_SECONDS,
};
pub use error::{DeviceAuthError, ExpiryCause, TokenStoreError};
pub use grant::{
    classify, decode_response, effective_expires_in, poll_deadline, system_clock, system_sleep,
    system_wall_clock, token_set_from_body, unix_seconds, AuthRuntime, BoxFuture, ClockFn,
    DeviceAuthorization, DeviceCodeGrant, Grant, LoginCallbacks, PollEvent, PollSummary, SleepFn,
    UserCodeEvent, WallClockFn,
};
pub use parse::{
    decode_body, default_field_aliases, normalise_device_body, read_field, read_i64, read_string,
    ErrorIdentifier, ParsedBody, RequestKind,
};
pub use request::{
    device_params, prepare_request, refresh_params, revoke_params, token_params, PreparedRequest,
};
pub use store::{default_credentials_path, store_key, FileTokenStore, NullTokenStore, TokenStore};
pub use token::{TokenSet, DEFAULT_SKEW_SECONDS, REDACTED};
pub use transport::{
    HttpResponse, HttpTransport, RequestBody, ReqwestTransport, SharedTransport, TransportError,
};
