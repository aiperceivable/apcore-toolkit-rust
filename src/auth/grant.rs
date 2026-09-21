// The `Grant` seam and the RFC 8628 polling state machine.
//
// Six of the eight components in this module's neighbourhood are
// grant-independent -- `TokenSet`, the store, refresh, configuration and
// discovery, the hooks, and redaction. Only the polling state machine and the
// device authorization request are device-flow specific, which is why `Grant`
// exists in V1 even though `DeviceCodeGrant` is the only implementation: a
// second grant is then one implementation against a stable seam rather than a
// rewrite.
//
// The state machine is PURE over an injected monotonic clock, an injected
// sleep, and a response sequence. That is what makes the conformance corpus
// runnable with no HTTP mocking at all.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tracing::warn;

use crate::auth::config::{
    DeviceAuthConfig, DEFAULT_DEVICE_EXPIRY_SECONDS, SLOW_DOWN_INCREMENT_SECONDS,
};
use crate::auth::error::{DeviceAuthError, ExpiryCause};
use crate::auth::parse::{
    decode_body, normalise_device_body, read_i64, read_string, ErrorIdentifier, ParsedBody,
    RequestKind,
};
use crate::auth::request::{device_params, prepare_request, token_params};
use crate::auth::token::TokenSet;
use crate::auth::transport::{HttpResponse, SharedTransport};

/// A boxed future, so the injected sleep can be any async timer.
///
/// Defined here rather than pulled from `futures` to keep the crate's runtime
/// dependency list unchanged.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The injected monotonic clock. Used for elapsed time only: an NTP correction
/// or a laptop suspend must not make the polling deadline jump.
pub type ClockFn = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The injected sleep.
pub type SleepFn = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

/// The injected wall clock. Used for `expires_at` and `obtained_at` only,
/// which must survive process restarts and so cannot be monotonic.
///
/// This is the **third** injection point, alongside the monotonic clock and
/// the sleep. The two clocks are not interchangeable: reusing one for both
/// either breaks the deadline under an NTP correction or makes `expires_at`
/// meaningless across a restart.
pub type WallClockFn = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// A wall-clock instant as Unix seconds, which is the form `TokenSet` stores.
///
/// A pre-epoch instant clamps to 0 rather than propagating a negative
/// lifetime into an expiry comparison.
pub fn unix_seconds(instant: SystemTime) -> i64 {
    instant
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// An observational consumer callback over one event type.
pub type EventCallback<T> = Arc<dyn Fn(&T) + Send + Sync>;

/// `on_user_code`: invoked once, after the device-code response.
pub type UserCodeCallback = EventCallback<UserCodeEvent>;

/// `on_poll`: invoked before each poll.
pub type PollCallback = EventCallback<PollEvent>;

/// The real monotonic clock.
pub fn system_clock() -> ClockFn {
    Arc::new(Instant::now)
}

/// The real wall clock.
pub fn system_wall_clock() -> WallClockFn {
    Arc::new(SystemTime::now)
}

/// The real async sleep.
pub fn system_sleep() -> SleepFn {
    Arc::new(|duration| Box::pin(tokio::time::sleep(duration)))
}

/// Everything a grant needs that is not grant-specific: the provider
/// configuration, the transport, and the two clocks plus the sleep.
#[derive(Clone)]
pub struct AuthRuntime {
    /// Provider configuration, already validated.
    pub config: DeviceAuthConfig,
    /// The HTTP seam.
    pub transport: SharedTransport,
    /// Monotonic clock, for the polling deadline.
    pub clock: ClockFn,
    /// Async sleep, for the polling interval.
    pub sleep: SleepFn,
    /// Wall clock, for `expires_at`.
    pub wall_clock: WallClockFn,
}

impl std::fmt::Debug for AuthRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthRuntime")
            .field("config", &self.config)
            .field("transport", &"<transport>")
            .field("clock", &"<clock>")
            .field("sleep", &"<sleep>")
            .field("wall_clock", &"<wall_clock>")
            .finish()
    }
}

/// What the consumer is asked to display, once, after the device-code
/// response.
///
/// `verification_uri_complete` is passed separately rather than pre-merged:
/// it embeds the user code and suits a QR code, but the consumer should still
/// show the plain URI and code for manual entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCodeEvent {
    /// Where the user goes.
    pub verification_uri: String,
    /// What the user types. Byte-for-byte as the server sent it.
    pub user_code: String,
    /// The combined URL, when the provider returns one. Most do not.
    pub verification_uri_complete: Option<String>,
    /// Seconds until the client gives up -- the deadline actually in force,
    /// never absent and never the raw wire value.
    ///
    /// Two adjustments have already been applied: the 15-minute fallback when
    /// the server omits `expires_in`, and the clamp to `timeout_seconds` when
    /// the caller set a shorter ceiling. A consumer rendering a countdown gets
    /// one number, the one the poll loop uses; handing over the unadjusted
    /// value would show "expires in 600s" on a flow that dies at 7, and would
    /// make every consumer reimplement the fallback -- precisely the per-CLI
    /// duplication this feature exists to remove.
    pub expires_in: u64,
}

/// Progress before each poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollEvent {
    /// 1-based attempt number.
    pub attempt: u32,
    /// The interval that was just slept.
    pub interval: Duration,
    /// Monotonic time since the device-code response.
    pub elapsed: Duration,
}

/// The toolkit's only channel to a user interface.
///
/// The toolkit writes nothing to a terminal: no print, no spinner, no colour,
/// no browser launch. Both callbacks are optional; omitting them yields a
/// silent, headless flow suitable for tests and daemons. Neither may influence
/// protocol behaviour, so a callback that panics is swallowed and logged --
/// a rendering failure in the UI layer is not a reason to lose an in-flight
/// authorization.
#[derive(Clone, Default)]
pub struct LoginCallbacks {
    /// Invoked once, after the device-code response.
    pub on_user_code: Option<UserCodeCallback>,
    /// Invoked before each poll.
    pub on_poll: Option<PollCallback>,
    /// A hard ceiling independent of the server's `expires_in`. When both
    /// apply, the shorter wins.
    pub timeout_seconds: Option<f64>,
}

impl std::fmt::Debug for LoginCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginCallbacks")
            .field(
                "on_user_code",
                &self.on_user_code.as_ref().map(|_| "<callback>"),
            )
            .field("on_poll", &self.on_poll.as_ref().map(|_| "<callback>"))
            .field("timeout_seconds", &self.timeout_seconds)
            .finish()
    }
}

/// Run an observational callback without letting it affect the flow.
fn notify<T>(callback: Option<&EventCallback<T>>, event: &T) {
    let Some(callback) = callback else { return };
    if std::panic::catch_unwind(AssertUnwindSafe(|| callback(event))).is_err() {
        warn!("device auth: a consumer callback panicked; the flow continues");
    }
}

/// The parsed device authorization response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    /// The short-lived pre-authorization secret. Never persisted.
    pub device_code: String,
    /// Verbatim, as the server sent it.
    pub user_code: String,
    /// Where the user goes.
    pub verification_uri: String,
    /// The combined URL, when returned.
    pub verification_uri_complete: Option<String>,
    /// The **effective** device-code lifetime in seconds, with the 15-minute
    /// fallback already applied. See [`effective_expires_in`].
    pub expires_in: u64,
    /// Server-suggested polling interval, when stated.
    pub interval: Option<u64>,
}

/// What a completed poll loop did, independent of whether it succeeded.
///
/// Exposed so a consumer -- and the conformance harness -- can assert the
/// polling behaviour itself, not only its outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PollSummary {
    /// How many token requests were actually issued.
    pub polls_made: u32,
    /// The interval in force when the loop ended.
    pub final_interval: Duration,
    /// Monotonic time from the device-code response to termination.
    pub elapsed: Duration,
}

/// A way of obtaining a [`TokenSet`].
///
/// `DeviceCodeGrant` is the only implementation the toolkit ships, and V1's
/// scope is deliberately that one grant. The trait exists so a consumer can
/// add a manual-code-entry grant, or one of the proprietary "device flows"
/// that are not RFC 8628 at all, without forking.
#[async_trait]
pub trait Grant: Send + Sync {
    /// A stable identifier for logs and errors.
    fn name(&self) -> &'static str;

    /// Run the grant to completion.
    ///
    /// # Errors
    ///
    /// Returns whichever [`DeviceAuthError`] the grant's own protocol defines
    /// for denial, expiry, and unrecognised responses.
    async fn acquire(
        &self,
        runtime: &AuthRuntime,
        callbacks: &LoginCallbacks,
    ) -> Result<TokenSet, DeviceAuthError>;
}

/// RFC 8628 Device Authorization Grant.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeviceCodeGrant;

#[async_trait]
impl Grant for DeviceCodeGrant {
    fn name(&self) -> &'static str {
        "device_code"
    }

    async fn acquire(
        &self,
        runtime: &AuthRuntime,
        callbacks: &LoginCallbacks,
    ) -> Result<TokenSet, DeviceAuthError> {
        self.run(runtime, callbacks).await.0
    }
}

impl DeviceCodeGrant {
    /// [`Grant::acquire`], plus what the poll loop did.
    ///
    /// The summary is returned even on failure, because "how many polls did it
    /// make before stopping" is exactly what distinguishes a client that
    /// terminated on a terminal error from one that kept going.
    pub async fn run(
        &self,
        runtime: &AuthRuntime,
        callbacks: &LoginCallbacks,
    ) -> (Result<TokenSet, DeviceAuthError>, PollSummary) {
        let authorization = match self.request_device_code(runtime).await {
            Ok(authorization) => authorization,
            Err(e) => return (Err(e), PollSummary::default()),
        };

        // Computed ONCE, before the callback, and then handed to the poll
        // loop. The consumer and the loop therefore cannot disagree about when
        // the flow gives up -- which is the whole point of reporting a
        // resolved number rather than the raw wire value.
        let deadline = poll_deadline(authorization.expires_in, callbacks.timeout_seconds);

        notify(
            callbacks.on_user_code.as_ref(),
            &UserCodeEvent {
                verification_uri: authorization.verification_uri.clone(),
                user_code: authorization.user_code.clone(),
                verification_uri_complete: authorization.verification_uri_complete.clone(),
                expires_in: deadline.as_secs(),
            },
        );

        self.poll(runtime, &authorization, callbacks, deadline)
            .await
    }

    /// Step 1: `POST` to the device authorization endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceAuthError::Config`] when the endpoint is not
    /// configured, [`DeviceAuthError::Transport`] on a connection failure --
    /// there is no deadline to bound retries yet -- and
    /// [`DeviceAuthError::Protocol`] for a response that is not a usable
    /// device authorization.
    pub async fn request_device_code(
        &self,
        runtime: &AuthRuntime,
    ) -> Result<DeviceAuthorization, DeviceAuthError> {
        let config = &runtime.config;
        let endpoint = config
            .device_authorization_endpoint
            .as_ref()
            .ok_or_else(|| {
                DeviceAuthError::Config(
                    "device_authorization_endpoint is not configured; set it explicitly or call \
                 DeviceAuthConfig::discover()"
                        .into(),
                )
            })?;

        let request = prepare_request(config, RequestKind::Device, device_params(config));
        let response = runtime
            .transport
            .post(endpoint, &to_header_map(&request.headers), request.body)
            .await
            .map_err(|e| DeviceAuthError::Transport(e.to_string()))?;

        let Some(body) = decode_response(config, RequestKind::Device, &response) else {
            return Err(DeviceAuthError::Protocol {
                message: format!(
                    "device authorization response (HTTP {}) could not be decoded as JSON or \
                     form-urlencoded",
                    response.status
                ),
                raw_body: Some(response.body),
            });
        };
        let body = normalise_device_body(&body, Some(&config.field_aliases));

        if !(200..300).contains(&response.status) {
            let identifier = read_string(&body, "error", Some(&config.field_aliases))
                .unwrap_or_else(|| "unknown".to_string());
            return Err(DeviceAuthError::Protocol {
                message: format!(
                    "device authorization request failed (HTTP {}): {identifier}",
                    response.status
                ),
                raw_body: Some(response.body),
            });
        }

        let missing = |field: &str| DeviceAuthError::Protocol {
            message: format!("device authorization response is missing '{field}'"),
            raw_body: Some(response.body.clone()),
        };

        Ok(DeviceAuthorization {
            device_code: read_string(&body, "device_code", Some(&config.field_aliases))
                .ok_or_else(|| missing("device_code"))?,
            // Verbatim: no upper-casing, no stripping, no re-grouping.
            user_code: read_string(&body, "user_code", Some(&config.field_aliases))
                .ok_or_else(|| missing("user_code"))?,
            verification_uri: read_string(&body, "verification_uri", Some(&config.field_aliases))
                .ok_or_else(|| missing("verification_uri"))?,
            verification_uri_complete: read_string(
                &body,
                "verification_uri_complete",
                Some(&config.field_aliases),
            ),
            expires_in: effective_expires_in(
                read_i64(&body, "expires_in", Some(&config.field_aliases))
                    .and_then(|value| u64::try_from(value).ok()),
            ),
            interval: read_i64(&body, "interval", Some(&config.field_aliases))
                .and_then(|value| u64::try_from(value).ok()),
        })
    }

    /// Steps 3 to 6: wait, poll, dispatch, terminate.
    async fn poll(
        &self,
        runtime: &AuthRuntime,
        authorization: &DeviceAuthorization,
        callbacks: &LoginCallbacks,
        deadline: Duration,
    ) -> (Result<TokenSet, DeviceAuthError>, PollSummary) {
        let config = &runtime.config;
        let token_endpoint = match config.token_endpoint.as_ref() {
            Some(endpoint) => endpoint.clone(),
            None => {
                return (
                    Err(DeviceAuthError::Config(
                        "token_endpoint is not configured; set it explicitly or call \
                         DeviceAuthConfig::discover()"
                            .into(),
                    )),
                    PollSummary::default(),
                )
            }
        };

        let mut interval = Duration::from_secs(
            authorization
                .interval
                .unwrap_or(config.default_interval)
                .max(1),
        );
        let start = (runtime.clock)();
        let mut summary = PollSummary {
            polls_made: 0,
            final_interval: interval,
            elapsed: Duration::ZERO,
        };

        loop {
            // RFC 8628 section 3.5: wait BEFORE the first poll. An eager first
            // request races the user, who has not yet visited the URI.
            (runtime.sleep)(interval).await;
            let elapsed = (runtime.clock)().saturating_duration_since(start);
            summary.final_interval = interval;
            summary.elapsed = elapsed;

            // The client stops at its own deadline even when the server never
            // says `expired_token`; relying on the server alone leaves a
            // client polling indefinitely.
            if elapsed >= deadline {
                return (
                    Err(DeviceAuthError::AuthorizationExpired(
                        ExpiryCause::DeadlineElapsed,
                    )),
                    summary,
                );
            }

            summary.polls_made += 1;
            notify(
                callbacks.on_poll.as_ref(),
                &PollEvent {
                    attempt: summary.polls_made,
                    interval,
                    elapsed,
                },
            );

            let request = prepare_request(
                config,
                RequestKind::Token,
                token_params(&authorization.device_code),
            );
            let response = match runtime
                .transport
                .post(
                    &token_endpoint,
                    &to_header_map(&request.headers),
                    request.body,
                )
                .await
            {
                Ok(response) => response,
                Err(e) => {
                    // Retryable, not terminal: a dropped connection mid-flow is
                    // common on flaky networks and the deadline already bounds
                    // the total wait.
                    warn!(error = %e, "device auth: transport failure while polling, retrying");
                    continue;
                }
            };

            match self.dispatch(config, &response) {
                Dispatch::Success(tokens) => {
                    let mut tokens = tokens;
                    let now = unix_seconds((runtime.wall_clock)());
                    tokens.obtained_at = now;
                    tokens.expires_at = tokens.expires_at.map(|lifetime| now + lifetime);
                    return (Ok(tokens), summary);
                }
                Dispatch::Continue => continue,
                Dispatch::Backoff(server_interval) => {
                    interval = next_interval_from(interval, server_interval);
                    summary.final_interval = interval;
                    continue;
                }
                Dispatch::Terminate(error) => return (Err(error), summary),
            }
        }
    }

    /// The normative dispatch table.
    ///
    /// Dispatch is on the response BODY. The status code decides exactly one
    /// thing: whether the body is a success payload (2xx) or an error payload
    /// (everything else). One major provider returns `authorization_pending`
    /// as HTTP 428 and both `slow_down` and `access_denied` as HTTP 403; a
    /// status-driven client treats all three as fatal.
    fn dispatch(&self, config: &DeviceAuthConfig, response: &HttpResponse) -> Dispatch {
        let decoded = decode_response(config, RequestKind::Token, response);

        if (200..300).contains(&response.status) {
            let Some(body) = decoded else {
                return Dispatch::Terminate(DeviceAuthError::Protocol {
                    message: "token response could not be decoded as JSON or form-urlencoded"
                        .into(),
                    raw_body: Some(response.body.clone()),
                });
            };
            return match token_set_from_body(&body, config) {
                Some(tokens) => Dispatch::Success(tokens),
                None => Dispatch::Terminate(DeviceAuthError::Protocol {
                    message: "token response carries no access_token".into(),
                    raw_body: Some(response.body.clone()),
                }),
            };
        }

        // Fail soft on shape: a body matching no known envelope becomes a
        // protocol error CARRYING the raw body, never a crash on a missing key.
        let Some(body) = decoded else {
            return Dispatch::Terminate(DeviceAuthError::Protocol {
                message: format!(
                    "error response (HTTP {}) could not be decoded as JSON or form-urlencoded",
                    response.status
                ),
                raw_body: Some(response.body.clone()),
            });
        };

        let identifier = match classify(config, &body) {
            Ok(identifier) => identifier,
            Err(e) => return Dispatch::Terminate(e),
        };

        match identifier {
            Some(ErrorIdentifier::AuthorizationPending) => Dispatch::Continue,
            Some(ErrorIdentifier::SlowDown) => {
                Dispatch::Backoff(read_i64(&body, "interval", Some(&config.field_aliases)))
            }
            Some(ErrorIdentifier::AccessDenied) => {
                Dispatch::Terminate(DeviceAuthError::AuthorizationDenied)
            }
            Some(ErrorIdentifier::ExpiredToken) => Dispatch::Terminate(
                DeviceAuthError::AuthorizationExpired(ExpiryCause::ServerExpiredToken),
            ),
            None => {
                let reported = read_string(&body, "error", Some(&config.field_aliases));
                Dispatch::Terminate(DeviceAuthError::Protocol {
                    message: match reported {
                        Some(value) => format!(
                            "unrecognised error identifier '{value}' (HTTP {})",
                            response.status
                        ),
                        None => format!(
                            "error response (HTTP {}) carries no recognisable error identifier",
                            response.status
                        ),
                    },
                    raw_body: Some(response.body.clone()),
                })
            }
        }
    }
}

/// One step of the state machine, decided purely from a response body.
enum Dispatch {
    /// A token was issued; `expires_at` still holds the raw lifetime.
    Success(TokenSet),
    /// `authorization_pending`: keep polling, interval unchanged.
    Continue,
    /// `slow_down`: keep polling, carrying the server's own `interval` when the
    /// error body supplied one.
    Backoff(Option<i64>),
    /// Terminal.
    Terminate(DeviceAuthError),
}

/// The interval to use after a `slow_down`.
///
/// RFC 8628 section 3.5 mandates a fixed +5 seconds, not a multiplier: the
/// server's rate limiter is written against that behaviour. Some providers
/// return an updated `interval` inside the `slow_down` body; when present that
/// value is authoritative and is used verbatim.
fn next_interval_from(current: Duration, body_interval: Option<i64>) -> Duration {
    match body_interval.and_then(|value| u64::try_from(value).ok()) {
        Some(seconds) => Duration::from_secs(seconds),
        None => current + Duration::from_secs(SLOW_DOWN_INCREMENT_SECONDS),
    }
}

/// The device-code lifetime the client will actually honour.
///
/// A device response without `expires_in` falls back to 15 minutes rather than
/// polling forever. Applied once, here, at parse time: the value reaches both
/// the deadline and `on_user_code` already resolved, so the two can never
/// disagree about when the flow gives up.
pub fn effective_expires_in(raw: Option<u64>) -> u64 {
    raw.unwrap_or(DEFAULT_DEVICE_EXPIRY_SECONDS)
}

/// Fold the effective lifetime and `timeout_seconds` into one deadline.
///
/// When a caller-supplied timeout also applies, the shorter of the two wins.
pub fn poll_deadline(expires_in: u64, timeout_seconds: Option<f64>) -> Duration {
    let server = Duration::from_secs(expires_in);
    match timeout_seconds {
        Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
            server.min(Duration::from_secs_f64(seconds))
        }
        _ => server,
    }
}

/// Decode a response, giving the `parse_response` hook first refusal.
///
/// Returning `None` from the hook means "no opinion, use the default", so a
/// hook can special-case one endpoint and ignore the rest.
pub fn decode_response(
    config: &DeviceAuthConfig,
    kind: RequestKind,
    response: &HttpResponse,
) -> Option<ParsedBody> {
    if let Some(hook) = &config.hooks.parse_response {
        if let Some(body) = hook(
            kind,
            response.status,
            response.content_type.as_deref(),
            &response.body,
        ) {
            return Some(body);
        }
    }
    decode_body(response.content_type.as_deref(), &response.body)
}

/// Normalise an error body onto one of the four RFC identifiers.
///
/// Fixed order: field-name aliasing, then `error_aliases`, then
/// `classify_error`, then dispatch. An alias that already resolves to a
/// standard identifier settles the question, so the hook cannot overrule it --
/// conformance case 047 pins that, because "the user refused" and "the code
/// timed out" are not interchangeable.
///
/// # Errors
///
/// Returns [`DeviceAuthError::InvalidHookReturn`] when `classify_error`
/// returns anything outside the four identifiers. Never coerced, never
/// defaulted: a hook that can invent a fifth state is a back door into the
/// closed protocol-decision layer.
pub fn classify(
    config: &DeviceAuthConfig,
    body: &ParsedBody,
) -> Result<Option<ErrorIdentifier>, DeviceAuthError> {
    let raw = read_string(body, "error", Some(&config.field_aliases));
    let aliased = raw.as_ref().map(|value| {
        config
            .error_aliases
            .get(value)
            .cloned()
            .unwrap_or_else(|| value.clone())
    });
    if let Some(identifier) = aliased.as_deref().and_then(ErrorIdentifier::from_wire) {
        return Ok(Some(identifier));
    }

    let Some(hook) = &config.hooks.classify_error else {
        return Ok(None);
    };
    match hook(body) {
        None => Ok(None),
        Some(value) => match ErrorIdentifier::from_wire(&value) {
            Some(identifier) => Ok(Some(identifier)),
            None => Err(DeviceAuthError::InvalidHookReturn(value)),
        },
    }
}

/// Build a [`TokenSet`] from a success payload.
///
/// `expires_at` is left as the raw `expires_in` lifetime; the caller adds the
/// wall clock, because only it holds the injected clock.
pub fn token_set_from_body(body: &ParsedBody, config: &DeviceAuthConfig) -> Option<TokenSet> {
    let access_token = read_string(body, "access_token", Some(&config.field_aliases))?;
    Some(TokenSet {
        access_token,
        token_type: TokenSet::normalise_token_type(
            read_string(body, "token_type", Some(&config.field_aliases)).as_deref(),
        ),
        expires_at: read_i64(body, "expires_in", Some(&config.field_aliases)),
        refresh_token: read_string(body, "refresh_token", Some(&config.field_aliases)),
        scope: TokenSet::split_scope(
            read_string(body, "scope", Some(&config.field_aliases)).as_deref(),
        ),
        obtained_at: 0,
    })
}

/// Convert prepared headers into the transport's map type.
pub(crate) fn to_header_map(headers: &crate::auth::config::ParamMap) -> HashMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::parse::decode_body;

    fn body(raw: &str) -> ParsedBody {
        decode_body(Some("application/json"), raw).expect("decodes")
    }

    #[test]
    fn test_next_interval_adds_five_by_default() {
        assert_eq!(
            next_interval_from(Duration::from_secs(5), None),
            Duration::from_secs(10)
        );
        assert_eq!(
            next_interval_from(Duration::from_secs(10), None),
            Duration::from_secs(15)
        );
    }

    #[test]
    fn test_next_interval_prefers_server_value() {
        assert_eq!(
            next_interval_from(Duration::from_secs(5), Some(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn test_poll_deadline_falls_back_to_fifteen_minutes() {
        assert_eq!(effective_expires_in(None), 900);
        assert_eq!(effective_expires_in(Some(600)), 600);
        assert_eq!(
            poll_deadline(effective_expires_in(None), None),
            Duration::from_secs(900)
        );
        assert_eq!(poll_deadline(600, None), Duration::from_secs(600));
    }

    #[test]
    fn test_poll_deadline_shorter_of_the_two_wins() {
        assert_eq!(poll_deadline(600, Some(30.0)), Duration::from_secs(30));
        assert_eq!(poll_deadline(20, Some(300.0)), Duration::from_secs(20));
    }

    #[test]
    fn test_classify_standard_identifier() {
        let config = DeviceAuthConfig::new("cid");
        assert_eq!(
            classify(&config, &body(r#"{"error":"slow_down"}"#)).expect("classify"),
            Some(ErrorIdentifier::SlowDown)
        );
    }

    #[test]
    fn test_classify_invalid_grant_is_not_aliased_by_default() {
        let config = DeviceAuthConfig::new("cid");
        assert_eq!(
            classify(&config, &body(r#"{"error":"invalid_grant"}"#)).expect("classify"),
            None
        );
    }

    #[test]
    fn test_classify_applies_error_aliases() {
        let mut config = DeviceAuthConfig::new("cid");
        config
            .error_aliases
            .insert("authorization_declined".into(), "access_denied".into());
        assert_eq!(
            classify(&config, &body(r#"{"error":"authorization_declined"}"#)).expect("classify"),
            Some(ErrorIdentifier::AccessDenied)
        );
    }

    #[test]
    fn test_classify_reads_error_code_field() {
        let config = DeviceAuthConfig::new("cid");
        assert_eq!(
            classify(&config, &body(r#"{"error_code":"authorization_pending"}"#))
                .expect("classify"),
            Some(ErrorIdentifier::AuthorizationPending)
        );
    }

    #[test]
    fn test_classify_alias_wins_over_hook() {
        let mut config = DeviceAuthConfig::new("cid");
        config
            .error_aliases
            .insert("vendor_denied".into(), "access_denied".into());
        config.hooks.classify_error = Some(Arc::new(|_| Some("expired_token".to_string())));
        assert_eq!(
            classify(&config, &body(r#"{"error":"vendor_denied"}"#)).expect("classify"),
            Some(ErrorIdentifier::AccessDenied)
        );
    }

    #[test]
    fn test_classify_hook_none_falls_back() {
        let mut config = DeviceAuthConfig::new("cid");
        config.hooks.classify_error = Some(Arc::new(|_| None));
        assert_eq!(
            classify(&config, &body(r#"{"error":"access_denied"}"#)).expect("classify"),
            Some(ErrorIdentifier::AccessDenied)
        );
        assert_eq!(
            classify(&config, &body(r#"{"errorCode":"E01"}"#)).expect("classify"),
            None
        );
    }

    #[test]
    fn test_classify_hook_invalid_return_is_rejected() {
        let mut config = DeviceAuthConfig::new("cid");
        config.hooks.classify_error = Some(Arc::new(|_| Some("something_else".to_string())));
        match classify(&config, &body(r#"{"errorCode":"E01"}"#)) {
            Err(DeviceAuthError::InvalidHookReturn(value)) => assert_eq!(value, "something_else"),
            other => panic!("expected InvalidHookReturn, got {other:?}"),
        }
    }

    #[test]
    fn test_token_set_from_body_normalises() {
        let config = DeviceAuthConfig::new("cid");
        let tokens = token_set_from_body(
            &body(r#"{"access_token":"t","token_type":"bearer","expires_in":3600,"scope":"a b"}"#),
            &config,
        )
        .expect("token set");
        assert_eq!(tokens.token_type, "Bearer");
        assert_eq!(tokens.scope, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(tokens.expires_at, Some(3600));
    }

    #[test]
    fn test_token_set_from_body_requires_access_token() {
        let config = DeviceAuthConfig::new("cid");
        assert!(token_set_from_body(&body(r#"{"unexpected":"shape"}"#), &config).is_none());
    }

    #[test]
    fn test_notify_swallows_a_panicking_callback() {
        let callback: PollCallback = Arc::new(|_| panic!("rendering failed"));
        // Must not propagate: a UI failure is not a reason to lose an
        // in-flight authorization.
        notify(
            Some(&callback),
            &PollEvent {
                attempt: 1,
                interval: Duration::from_secs(5),
                elapsed: Duration::ZERO,
            },
        );
    }
}
