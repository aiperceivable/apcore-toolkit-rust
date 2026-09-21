// DeviceAuthClient: the grant, the store, and the token lifecycle, wired
// together.
//
// The output plugs into an integration point that already exists:
// `HTTPProxyRegistryWriter` has accepted a pluggable `auth_header_factory`
// since it shipped, and this fills that hole with a managed credential rather
// than adding a new integration surface.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::warn;

use crate::auth::config::DeviceAuthConfig;
use crate::auth::error::DeviceAuthError;
use crate::auth::grant::{
    decode_response, system_clock, system_sleep, system_wall_clock, to_header_map, unix_seconds,
    AuthRuntime, BoxFuture, ClockFn, DeviceCodeGrant, Grant, LoginCallbacks, SleepFn, WallClockFn,
};
use crate::auth::parse::{read_string, RequestKind};
use crate::auth::request::{prepare_request, refresh_params};
use crate::auth::store::{NullTokenStore, TokenStore};
use crate::auth::token::{TokenSet, DEFAULT_SKEW_SECONDS};
use crate::auth::transport::{ReqwestTransport, SharedTransport};

/// A synchronous header factory, matching the `auth_header_factory` signature
/// `HTTPProxyRegistryWriter` already accepts.
pub type AuthHeaderFactory = Box<dyn Fn() -> HashMap<String, String> + Send + Sync>;

/// The asynchronous header factory. Recorded design decision 1 for Rust: add a
/// separate async factory rather than change the synchronous one.
pub type AsyncAuthHeaderFactory = Box<
    dyn Fn() -> BoxFuture<'static, Result<HashMap<String, String>, DeviceAuthError>> + Send + Sync,
>;

/// The managed credential for one authorization server.
///
/// Holds a [`Grant`] (device flow in V1), a [`TokenStore`], and the injected
/// clocks. The stored record is keyed `"<issuer>|<client_id>"`, so credentials
/// for several providers coexist without collision.
pub struct DeviceAuthClient {
    runtime: AuthRuntime,
    store: Arc<dyn TokenStore>,
    grant: Arc<dyn Grant>,
    key: String,
    /// Last known credential, so the synchronous header factory has something
    /// to hand out without awaiting.
    cached: Arc<Mutex<Option<TokenSet>>>,
}

impl std::fmt::Debug for DeviceAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceAuthClient")
            .field("runtime", &self.runtime)
            .field("store", &"<store>")
            .field("grant", &self.grant.name())
            .field("key", &self.key)
            // Never the cached TokenSet's contents; its own Debug redacts, and
            // this keeps the nesting honest too.
            .field(
                "cached",
                &self.cached.lock().map(|slot| slot.is_some()).ok(),
            )
            .finish()
    }
}

impl DeviceAuthClient {
    /// Build a client for `config`, persisting to `store`.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceAuthError::Config`] when the configuration is invalid:
    /// an empty `client_id`, a plaintext endpoint, or an `error_aliases` entry
    /// that maps onto something other than the four RFC 8628 identifiers.
    pub fn new(
        config: DeviceAuthConfig,
        store: Arc<dyn TokenStore>,
    ) -> Result<Self, DeviceAuthError> {
        config.validate()?;
        let key = config.store_key();
        let transport: SharedTransport = match &config.hooks.http_client {
            Some(client) => client.clone(),
            None => match config.http_timeout_seconds {
                Some(seconds) if seconds.is_finite() && seconds > 0.0 => Arc::new(
                    ReqwestTransport::with_timeout(std::time::Duration::from_secs_f64(seconds))
                        .map_err(|e| {
                            DeviceAuthError::Config(format!("failed to build HTTP client: {e}"))
                        })?,
                ),
                _ => Arc::new(ReqwestTransport::default()),
            },
        };
        Ok(Self {
            runtime: AuthRuntime {
                config,
                transport,
                clock: system_clock(),
                sleep: system_sleep(),
                wall_clock: system_wall_clock(),
            },
            store,
            grant: Arc::new(DeviceCodeGrant),
            key,
            cached: Arc::new(Mutex::new(None)),
        })
    }

    /// A client that persists nothing, for daemons and tests.
    ///
    /// # Errors
    ///
    /// As [`DeviceAuthClient::new`].
    pub fn without_store(config: DeviceAuthConfig) -> Result<Self, DeviceAuthError> {
        Self::new(config, Arc::new(NullTokenStore))
    }

    /// Use a different grant. V1 ships only [`DeviceCodeGrant`]; this is the
    /// seam a consumer-supplied grant plugs into.
    pub fn with_grant(mut self, grant: Arc<dyn Grant>) -> Self {
        self.grant = grant;
        self
    }

    /// Replace the monotonic clock. Elapsed-time measurement only.
    pub fn with_clock(mut self, clock: ClockFn) -> Self {
        self.runtime.clock = clock;
        self
    }

    /// Replace the sleep.
    pub fn with_sleep(mut self, sleep: SleepFn) -> Self {
        self.runtime.sleep = sleep;
        self
    }

    /// Replace the wall clock. `expires_at` and `obtained_at` only.
    pub fn with_wall_clock(mut self, wall_clock: WallClockFn) -> Self {
        self.runtime.wall_clock = wall_clock;
        self
    }

    /// Replace the transport.
    pub fn with_transport(mut self, transport: SharedTransport) -> Self {
        self.runtime.transport = transport;
        self
    }

    /// The runtime this client hands to its grant.
    pub fn runtime(&self) -> &AuthRuntime {
        &self.runtime
    }

    /// The store key: `"<issuer>|<client_id>"`.
    pub fn store_key(&self) -> &str {
        &self.key
    }

    /// Run the grant to completion and persist the result.
    ///
    /// # Errors
    ///
    /// - [`DeviceAuthError::AuthorizationDenied`] when the user refused.
    /// - [`DeviceAuthError::AuthorizationExpired`] when the device code
    ///   expired or the deadline elapsed.
    /// - [`DeviceAuthError::Protocol`] for an unrecognised error identifier or
    ///   a malformed body.
    ///
    /// Transport failures during polling are retried until the deadline rather
    /// than raised, and callback panics are swallowed.
    pub async fn login(&self, callbacks: &LoginCallbacks) -> Result<TokenSet, DeviceAuthError> {
        let tokens = self.grant.acquire(&self.runtime, callbacks).await?;
        // Only the TokenSet is persisted. The `device_code` is a short-lived
        // pre-authorization secret and never reaches the store.
        self.store.save(&self.key, &tokens).await?;
        self.set_cached(Some(tokens.clone()));
        Ok(tokens)
    }

    /// Return a credential valid for at least `skew_seconds` longer,
    /// refreshing if needed.
    ///
    /// Idempotent while the token is still valid: it is returned unchanged and
    /// no network call is made.
    ///
    /// # Errors
    ///
    /// - [`DeviceAuthError::NoCredential`] when nothing is stored, or when the
    ///   stored credential has expired and carries no refresh token.
    /// - [`DeviceAuthError::RefreshFailed`] when the refresh was rejected with
    ///   `invalid_grant`; the store is cleared as a side effect.
    ///
    /// Transport errors propagate: unlike polling, there is no deadline here
    /// to bound retries.
    pub async fn ensure_valid(&self, skew_seconds: i64) -> Result<TokenSet, DeviceAuthError> {
        let stored =
            self.store
                .load(&self.key)
                .await?
                .ok_or_else(|| DeviceAuthError::NoCredential {
                    key: self.key.clone(),
                })?;

        let now = unix_seconds((self.runtime.wall_clock)());
        if !stored.is_expired(now, skew_seconds) {
            self.set_cached(Some(stored.clone()));
            return Ok(stored);
        }
        if stored.refresh_token.is_none() {
            // Common for short-lived scopes: no refresh token means a fresh
            // login, not a silent failure.
            return Err(DeviceAuthError::NoCredential {
                key: self.key.clone(),
            });
        }
        self.refresh_stored(&stored).await
    }

    /// [`DeviceAuthClient::ensure_valid`] with the default 30-second skew.
    ///
    /// # Errors
    ///
    /// As [`DeviceAuthClient::ensure_valid`].
    pub async fn ensure_valid_default(&self) -> Result<TokenSet, DeviceAuthError> {
        self.ensure_valid(DEFAULT_SKEW_SECONDS).await
    }

    /// Exchange the stored refresh token for a new [`TokenSet`].
    ///
    /// # Errors
    ///
    /// As [`DeviceAuthClient::ensure_valid`], plus
    /// [`DeviceAuthError::NoCredential`] when the stored record carries no
    /// refresh token.
    pub async fn refresh(&self) -> Result<TokenSet, DeviceAuthError> {
        let stored =
            self.store
                .load(&self.key)
                .await?
                .ok_or_else(|| DeviceAuthError::NoCredential {
                    key: self.key.clone(),
                })?;
        self.refresh_stored(&stored).await
    }

    /// The refresh exchange itself.
    ///
    /// Rotation is assumed: the entire stored record is replaced, never merged
    /// into, because merging a new access token into an old record keeps a
    /// refresh token the server has already invalidated.
    async fn refresh_stored(&self, stored: &TokenSet) -> Result<TokenSet, DeviceAuthError> {
        let config = &self.runtime.config;
        let refresh_token =
            stored
                .refresh_token
                .clone()
                .ok_or_else(|| DeviceAuthError::NoCredential {
                    key: self.key.clone(),
                })?;
        let token_endpoint = config
            .token_endpoint
            .as_ref()
            .ok_or_else(|| DeviceAuthError::Config("token_endpoint is not configured".into()))?;

        let request = prepare_request(config, RequestKind::Refresh, refresh_params(&refresh_token));
        let response = self
            .runtime
            .transport
            .post(
                token_endpoint,
                &to_header_map(&request.headers),
                request.body,
            )
            .await
            .map_err(|e| DeviceAuthError::Transport(e.to_string()))?;

        let decoded = decode_response(config, RequestKind::Refresh, &response);

        if (200..300).contains(&response.status) {
            let tokens = decoded
                .as_ref()
                .and_then(|body| crate::auth::grant::token_set_from_body(body, config))
                .ok_or_else(|| DeviceAuthError::Protocol {
                    message: "refresh response carries no access_token".into(),
                    raw_body: Some(response.body.clone()),
                })?;
            let now = unix_seconds((self.runtime.wall_clock)());
            let tokens = TokenSet {
                obtained_at: now,
                expires_at: tokens.expires_at.map(|lifetime| now + lifetime),
                ..tokens
            };
            self.store.save(&self.key, &tokens).await?;
            self.set_cached(Some(tokens.clone()));
            return Ok(tokens);
        }

        let identifier = decoded
            .as_ref()
            .and_then(|body| read_string(body, "error", Some(&config.field_aliases)));
        if identifier.as_deref() == Some("invalid_grant") {
            // Terminal. The refresh token is spent or revoked, so the correct
            // response is to discard the stored credential and require a fresh
            // login -- never to retry.
            self.store.clear(&self.key).await?;
            self.set_cached(None);
            return Err(DeviceAuthError::RefreshFailed {
                message: "invalid_grant".into(),
            });
        }

        Err(DeviceAuthError::Protocol {
            message: match identifier {
                Some(value) => format!("refresh failed (HTTP {}): {value}", response.status),
                None => format!("refresh failed (HTTP {})", response.status),
            },
            raw_body: Some(response.body),
        })
    }

    /// Discard the stored credential for this client's key.
    ///
    /// # Errors
    ///
    /// Propagates store I/O errors.
    pub async fn forget(&self) -> Result<(), DeviceAuthError> {
        self.store.clear(&self.key).await?;
        self.set_cached(None);
        Ok(())
    }

    /// The currently cached credential, if any.
    pub fn cached(&self) -> Option<TokenSet> {
        self.cached.lock().ok().and_then(|slot| slot.clone())
    }

    /// A synchronous header factory, for
    /// `HTTPProxyRegistryWriter::new(base_url, auth_header_factory, ..)`.
    ///
    /// Returns a **complete header mapping** rather than a token string,
    /// because the header name is provider data: `Authorization: Bearer`,
    /// `x-api-key`, and `api-key` are all in live use, and one vendor picks
    /// between two of them by credential type.
    ///
    /// This factory does **not** refresh. The shipped writer types the factory
    /// as synchronous in all three SDKs, and a refresh is an HTTP round trip:
    /// blocking on one inside an async context deadlocks or panics in Rust. Use
    /// [`DeviceAuthClient::as_async_auth_header_factory`] for transparent
    /// refresh, or call [`DeviceAuthClient::ensure_valid`] on your own
    /// schedule and let this factory read the refreshed cache.
    ///
    /// Consumers MUST NOT reuse one factory across hosts: it carries a bearer
    /// token, and sending it to an unintended host leaks it.
    pub fn as_auth_header_factory(&self) -> AuthHeaderFactory {
        let cached = Arc::clone(&self.cached);
        Box::new(move || {
            let tokens = cached.lock().ok().and_then(|slot| slot.clone());
            match tokens {
                Some(tokens) => tokens.auth_headers(),
                None => {
                    warn!(
                        "device auth: header factory called with no cached credential; \
                         call login() or ensure_valid() first"
                    );
                    HashMap::new()
                }
            }
        })
    }

    /// An asynchronous header factory that refreshes transparently.
    ///
    /// This is the separate `async_auth_header_factory` the design decision
    /// records for Rust: the existing synchronous signature is left intact and
    /// an async variant is added beside it, rather than changed.
    pub fn as_async_auth_header_factory(self: &Arc<Self>) -> AsyncAuthHeaderFactory {
        let client = Arc::clone(self);
        Box::new(move || {
            let client = Arc::clone(&client);
            Box::pin(async move { Ok(client.ensure_valid_default().await?.auth_headers()) })
        })
    }

    fn set_cached(&self, tokens: Option<TokenSet>) {
        if let Ok(mut slot) = self.cached.lock() {
            *slot = tokens;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::FileTokenStore;
    use crate::auth::transport::{HttpResponse, HttpTransport, RequestBody, TransportError};
    use async_trait::async_trait;
    use std::time::Duration;
    use tempfile::TempDir;

    /// A scripted transport: the injection seam the conformance corpus uses,
    /// exercised here for the store-facing half of the client.
    struct Scripted {
        responses: Mutex<Vec<Result<HttpResponse, TransportError>>>,
    }

    impl Scripted {
        fn new(responses: Vec<Result<HttpResponse, TransportError>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses),
            })
        }
    }

    #[async_trait]
    impl HttpTransport for Scripted {
        async fn post(
            &self,
            _url: &str,
            _headers: &HashMap<String, String>,
            _body: RequestBody,
        ) -> Result<HttpResponse, TransportError> {
            let mut queue = self.responses.lock().expect("lock");
            if queue.is_empty() {
                return Err(TransportError::new("scripted transport exhausted"));
            }
            queue.remove(0)
        }

        async fn get(
            &self,
            _url: &str,
            _headers: &HashMap<String, String>,
        ) -> Result<HttpResponse, TransportError> {
            self.post(_url, _headers, RequestBody::Empty).await
        }
    }

    fn json(status: u16, body: &str) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            content_type: Some("application/json".to_string()),
            body: body.to_string(),
        })
    }

    fn config() -> DeviceAuthConfig {
        let mut config = DeviceAuthConfig::new("cid");
        config.issuer = Some("https://a.example".to_string());
        config.device_authorization_endpoint = Some("https://a.example/device".to_string());
        config.token_endpoint = Some("https://a.example/token".to_string());
        config
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        futures::executor::block_on(future)
    }

    fn no_sleep() -> SleepFn {
        Arc::new(|_| Box::pin(async {}))
    }

    fn pinned_wall_clock(now: u64) -> WallClockFn {
        Arc::new(move || std::time::UNIX_EPOCH + Duration::from_secs(now))
    }

    fn stored(access: &str, refresh: Option<&str>, expires_at: Option<i64>) -> TokenSet {
        TokenSet {
            access_token: access.to_string(),
            token_type: "Bearer".to_string(),
            expires_at,
            refresh_token: refresh.map(str::to_string),
            scope: vec!["openid".to_string()],
            obtained_at: 0,
        }
    }

    #[test]
    fn test_new_rejects_invalid_alias_target() {
        let mut config = config();
        config
            .error_aliases
            .insert("weird".into(), "not_standard".into());
        assert!(DeviceAuthClient::new(config, Arc::new(NullTokenStore)).is_err());
    }

    #[test]
    fn test_login_persists_the_token_set() {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(FileTokenStore::at(dir.path().join("credentials.json")));
        let transport = Scripted::new(vec![
            json(
                200,
                r#"{"device_code":"dc","user_code":"AB-CD",
                    "verification_uri":"https://a.example/d","expires_in":600,"interval":5}"#,
            ),
            json(
                200,
                r#"{"access_token":"t","token_type":"bearer","expires_in":3600}"#,
            ),
        ]);
        let client = DeviceAuthClient::new(config(), store.clone())
            .expect("client")
            .with_transport(transport)
            .with_sleep(no_sleep())
            .with_wall_clock(pinned_wall_clock(1000));

        let tokens = block_on(client.login(&LoginCallbacks::default())).expect("login");
        assert_eq!(tokens.access_token, "t");
        assert_eq!(tokens.token_type, "Bearer");
        assert_eq!(tokens.expires_at, Some(4600));

        let persisted = block_on(store.load(client.store_key()))
            .expect("load")
            .expect("some");
        assert_eq!(persisted, tokens);
        // The device_code is a pre-authorization secret and is never stored.
        let raw = std::fs::read_to_string(dir.path().join("credentials.json")).expect("read");
        assert!(!raw.contains("dc"), "{raw}");
    }

    #[test]
    fn test_login_reports_user_code_verbatim_through_the_callback() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&seen);
        let callbacks = LoginCallbacks {
            on_user_code: Some(Arc::new(move |event| {
                sink.lock().expect("lock").push(event.user_code.clone());
            })),
            ..Default::default()
        };
        let transport = Scripted::new(vec![
            json(
                200,
                r#"{"device_code":"dc","user_code":"wdjb-mjht",
                    "verification_url":"https://a.example/d","expires_in":600,"interval":5}"#,
            ),
            json(200, r#"{"access_token":"t","expires_in":3600}"#),
        ]);
        let client = DeviceAuthClient::without_store(config())
            .expect("client")
            .with_transport(transport)
            .with_sleep(no_sleep())
            .with_wall_clock(pinned_wall_clock(1000));
        block_on(client.login(&callbacks)).expect("login");
        assert_eq!(seen.lock().expect("lock").as_slice(), ["wdjb-mjht"]);
    }

    #[test]
    fn test_on_user_code_reports_the_deadline_actually_in_force() {
        // One number, one derivation: the callback and the poll loop are both
        // handed the output of `poll_deadline`, so a consumer rendering a
        // countdown can never show "expires in 600s" on a flow that dies at 7.
        let seen = Arc::new(Mutex::new(Vec::<u64>::new()));
        let sink = Arc::clone(&seen);
        let callbacks = LoginCallbacks {
            on_user_code: Some(Arc::new(move |event| {
                sink.lock().expect("lock").push(event.expires_in);
            })),
            timeout_seconds: Some(7.0),
            ..Default::default()
        };
        let transport = Scripted::new(vec![
            json(
                200,
                r#"{"device_code":"dc","user_code":"AB-CD",
                    "verification_uri":"https://a.example/d","expires_in":600,"interval":5}"#,
            ),
            json(200, r#"{"access_token":"t","expires_in":3600}"#),
        ]);
        let client = DeviceAuthClient::without_store(config())
            .expect("client")
            .with_transport(transport)
            .with_sleep(no_sleep())
            .with_wall_clock(pinned_wall_clock(1000));
        block_on(client.login(&callbacks)).expect("login");
        assert_eq!(seen.lock().expect("lock").as_slice(), [7]);
    }

    #[test]
    fn test_on_user_code_applies_the_fifteen_minute_fallback() {
        let seen = Arc::new(Mutex::new(Vec::<u64>::new()));
        let sink = Arc::clone(&seen);
        let callbacks = LoginCallbacks {
            on_user_code: Some(Arc::new(move |event| {
                sink.lock().expect("lock").push(event.expires_in);
            })),
            ..Default::default()
        };
        let transport = Scripted::new(vec![
            json(
                200,
                r#"{"device_code":"dc","user_code":"AB-CD",
                    "verification_uri":"https://a.example/d","interval":5}"#,
            ),
            json(200, r#"{"access_token":"t","expires_in":3600}"#),
        ]);
        let client = DeviceAuthClient::without_store(config())
            .expect("client")
            .with_transport(transport)
            .with_sleep(no_sleep())
            .with_wall_clock(pinned_wall_clock(1000));
        block_on(client.login(&callbacks)).expect("login");
        assert_eq!(seen.lock().expect("lock").as_slice(), [900]);
    }

    #[test]
    fn test_ensure_valid_returns_a_live_token_without_network() {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(FileTokenStore::at(dir.path().join("credentials.json")));
        let client = DeviceAuthClient::new(config(), store.clone())
            .expect("client")
            // An exhausted transport proves no request was made.
            .with_transport(Scripted::new(vec![]))
            .with_wall_clock(pinned_wall_clock(1000));
        block_on(store.save(client.store_key(), &stored("live", Some("r1"), Some(9999))))
            .expect("save");

        let tokens = block_on(client.ensure_valid_default()).expect("ensure_valid");
        assert_eq!(tokens.access_token, "live");
    }

    #[test]
    fn test_ensure_valid_without_stored_credential_is_no_credential() {
        let client = DeviceAuthClient::without_store(config()).expect("client");
        assert!(matches!(
            block_on(client.ensure_valid_default()),
            Err(DeviceAuthError::NoCredential { .. })
        ));
    }

    #[test]
    fn test_ensure_valid_expired_without_refresh_token_is_no_credential() {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(FileTokenStore::at(dir.path().join("credentials.json")));
        let client = DeviceAuthClient::new(config(), store.clone())
            .expect("client")
            .with_wall_clock(pinned_wall_clock(2000));
        block_on(store.save(client.store_key(), &stored("old", None, Some(1000)))).expect("save");
        assert!(matches!(
            block_on(client.ensure_valid_default()),
            Err(DeviceAuthError::NoCredential { .. })
        ));
    }

    #[test]
    fn test_refresh_replaces_the_whole_record() {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(FileTokenStore::at(dir.path().join("credentials.json")));
        let transport = Scripted::new(vec![json(
            200,
            r#"{"access_token":"new","token_type":"Bearer","expires_in":3600,
                "refresh_token":"r2"}"#,
        )]);
        let client = DeviceAuthClient::new(config(), store.clone())
            .expect("client")
            .with_transport(transport)
            .with_wall_clock(pinned_wall_clock(2000));
        block_on(store.save(client.store_key(), &stored("old", Some("r1"), Some(1000))))
            .expect("save");

        let tokens = block_on(client.ensure_valid_default()).expect("refresh");
        assert_eq!(tokens.access_token, "new");
        assert_eq!(tokens.refresh_token.as_deref(), Some("r2"));
        assert_eq!(tokens.expires_at, Some(5600));
        // Replaced wholesale: the old scope does not survive an omitted one.
        assert!(tokens.scope.is_empty());
        assert_eq!(
            block_on(store.load(client.store_key()))
                .expect("load")
                .expect("some"),
            tokens
        );
    }

    #[test]
    fn test_refresh_invalid_grant_clears_the_store() {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(FileTokenStore::at(dir.path().join("credentials.json")));
        let transport = Scripted::new(vec![json(400, r#"{"error":"invalid_grant"}"#)]);
        let client = DeviceAuthClient::new(config(), store.clone())
            .expect("client")
            .with_transport(transport)
            .with_wall_clock(pinned_wall_clock(2000));
        block_on(store.save(client.store_key(), &stored("old", Some("r1"), Some(1000))))
            .expect("save");

        assert!(matches!(
            block_on(client.ensure_valid_default()),
            Err(DeviceAuthError::RefreshFailed { .. })
        ));
        assert!(block_on(store.load(client.store_key()))
            .expect("load")
            .is_none());
        assert!(client.cached().is_none());
    }

    #[test]
    fn test_refresh_ignores_error_aliases() {
        // error_aliases must NOT apply on the refresh path. A consumer who
        // opted into invalid_grant -> expired_token for the device-code flow
        // would otherwise disable the one terminal rule that protects the
        // store, leaving a spent refresh token on disk.
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(FileTokenStore::at(dir.path().join("credentials.json")));
        let mut config = config();
        config
            .error_aliases
            .insert("invalid_grant".into(), "expired_token".into());
        let transport = Scripted::new(vec![json(400, r#"{"error":"invalid_grant"}"#)]);
        let client = DeviceAuthClient::new(config, store.clone())
            .expect("client")
            .with_transport(transport)
            .with_wall_clock(pinned_wall_clock(2000));
        block_on(store.save(client.store_key(), &stored("old", Some("r1"), Some(1000))))
            .expect("save");

        assert!(matches!(
            block_on(client.refresh()),
            Err(DeviceAuthError::RefreshFailed { .. })
        ));
        assert!(block_on(store.load(client.store_key()))
            .expect("load")
            .is_none());
    }

    #[test]
    fn test_store_key_falls_back_to_token_endpoint_without_issuer() {
        let mut config = config();
        config.issuer = None;
        let client = DeviceAuthClient::new(config, Arc::new(NullTokenStore)).expect("client");
        assert_eq!(client.store_key(), "https://a.example/token|cid");
    }

    #[test]
    fn test_auth_header_factory_returns_a_complete_mapping() {
        let client = DeviceAuthClient::without_store(config()).expect("client");
        let factory = client.as_auth_header_factory();
        assert!(factory().is_empty(), "no credential yet");

        client.set_cached(Some(stored("t", None, None)));
        let headers = factory();
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer t")
        );
    }

    #[test]
    fn test_async_auth_header_factory_refreshes() {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(FileTokenStore::at(dir.path().join("credentials.json")));
        let transport = Scripted::new(vec![json(
            200,
            r#"{"access_token":"fresh","expires_in":3600,"refresh_token":"r2"}"#,
        )]);
        let client = Arc::new(
            DeviceAuthClient::new(config(), store.clone())
                .expect("client")
                .with_transport(transport)
                .with_wall_clock(pinned_wall_clock(2000)),
        );
        block_on(store.save(client.store_key(), &stored("old", Some("r1"), Some(1000))))
            .expect("save");

        let factory = client.as_async_auth_header_factory();
        let headers = block_on(factory()).expect("factory");
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer fresh")
        );
    }

    #[test]
    fn test_client_debug_never_renders_a_token() {
        let client = DeviceAuthClient::without_store(config()).expect("client");
        client.set_cached(Some(stored("SECRET-ACCESS", Some("SECRET-REFRESH"), None)));
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("SECRET-ACCESS"), "{rendered}");
        assert!(!rendered.contains("SECRET-REFRESH"), "{rendered}");
    }

    #[test]
    fn test_forget_clears_the_store_and_cache() {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(FileTokenStore::at(dir.path().join("credentials.json")));
        let client = DeviceAuthClient::new(config(), store.clone()).expect("client");
        block_on(store.save(client.store_key(), &stored("t", None, None))).expect("save");
        block_on(client.forget()).expect("forget");
        assert!(block_on(store.load(client.store_key()))
            .expect("load")
            .is_none());
    }

    #[test]
    fn test_timeout_seconds_shortens_the_deadline() {
        // A 600-second device code with a 12-second caller ceiling stops at 12.
        assert_eq!(
            crate::auth::grant::poll_deadline(600, Some(12.0)),
            Duration::from_secs(12)
        );
    }
}
