// Conformance harness: assert Rust's RFC 8628 device authorization client
// matches the shared fixture corpus at
// `apcore-toolkit/conformance/fixtures/device_auth.json`.
//
// The Python and TypeScript SDKs run the same cases through their own
// implementations. This is the cross-SDK contract for credential acquisition
// (see `apcore-toolkit/docs/features/device-auth.md`), and it is the one
// surface in the toolkit where a divergence is a security bug rather than a
// formatting nit.
//
// NO HTTP MOCKING IS INVOLVED. The state machine is pure over an injected
// monotonic clock, an injected sleep, and a scripted response sequence, so
// `ScriptedTransport` below is not a mock of `reqwest` -- it IS the fixture's
// `token_responses` array, handed to the client through the same `http_client`
// injection point a consumer would use for a proxy or an mTLS client.
//
// Harness conventions, all stated in the fixture's own `description` and in
// the spec's "Harness contract" section:
//
// - `poll_delays[i]` is the sleep performed BEFORE `token_responses[i]`, so
//   the first entry is the initial wait and is never 0.
// - `polls_made` counts the scripted responses actually consumed. A case that
//   scripts more responses than that is asserting the client STOPPED.
// - `repeat_last_response: true` repeats the final scripted response
//   indefinitely (case 023, whose 15-minute deadline would otherwise need 180
//   literal entries).
// - The wall clock is pinned to 1000 wherever a case asserts `expires_at` or
//   `obtained_at`. The polling clock stays monotonic and advances only by the
//   sleeps.
// - `transport_error: true` is a scripted connection failure rather than an
//   HTTP response.
//
// Case kinds and where each is driven:
//
// | kind | driven through |
// |---|---|
// | `poll` | `DeviceCodeGrant::run` over a scripted transport |
// | `expiry` | `TokenSet::is_expired` |
// | `refresh` | `DeviceAuthClient::refresh` over a real `FileTokenStore` |
// | `discovery_url` | `discovery_candidates` -- pure, no network |
// | `discovery` | `DeviceAuthConfig::discover_with` over a scripted transport |
// | `parse` | `decode_body` / `normalise_device_body`, or a poll for 051 |
// | `request` | `prepare_request` |
// | `redaction` | the hand-written `Debug` on `TokenSet` |
// | `alias_validation` | `validate_error_aliases` |
// | `hook` | the hook call sites, plus `classify` |

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use apcore_toolkit::auth::client::DeviceAuthClient;
use apcore_toolkit::auth::encoding::form_decode;
use apcore_toolkit::auth::request::{device_params, prepare_request, refresh_params, token_params};
use apcore_toolkit::auth::{
    classify, decode_body, decode_response, discovery_candidates, normalise_device_body,
    validate_error_aliases, AuthRuntime, BodyEncoding, ClientAuthMethod, DeviceAuthConfig,
    DeviceAuthError, DeviceCodeGrant, ExpiryCause, FileTokenStore, HttpResponse, HttpTransport,
    LoginCallbacks, ParsedBody, PollSummary, RequestBody, RequestKind, SharedTransport, TokenSet,
    TokenStore, TransportError, UserCodeEvent,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tempfile::TempDir;

const DEVICE_ENDPOINT: &str = "https://e.example/device";
const TOKEN_ENDPOINT: &str = "https://e.example/token";

// ---------------------------------------------------------------- fixture io

fn conformance_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("toolkit-rust dir must have a parent")
        .join("apcore-toolkit")
        .join("conformance")
        .join("fixtures")
}

fn load_cases() -> Vec<Value> {
    let path = conformance_dir().join("device_auth.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => {
            eprintln!("WARN: conformance fixture not found at {path:?}; skipping");
            return Vec::new();
        }
    };
    let doc: Value = serde_json::from_str(&content).expect("fixture must be valid JSON");
    doc["test_cases"].as_array().cloned().unwrap_or_default()
}

// ------------------------------------------------------------ scripted seams

/// The fixture's response arrays, behind the `http_client` injection point.
struct ScriptedTransport {
    /// Responses for the device authorization endpoint.
    device: Mutex<Vec<Result<HttpResponse, TransportError>>>,
    /// Responses for the token endpoint, consumed in order.
    token: Mutex<Vec<Result<HttpResponse, TransportError>>>,
    /// Responses for discovery `GET`s, consumed in order.
    discovery: Mutex<Vec<Result<HttpResponse, TransportError>>>,
    /// Repeat the final token response indefinitely (case 023).
    repeat_last: bool,
    /// Which discovery candidate index was fetched last, 1-based.
    discovery_calls: Mutex<usize>,
}

impl ScriptedTransport {
    fn new(
        device: Vec<Result<HttpResponse, TransportError>>,
        token: Vec<Result<HttpResponse, TransportError>>,
        repeat_last: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            device: Mutex::new(device),
            token: Mutex::new(token),
            discovery: Mutex::new(Vec::new()),
            repeat_last,
            discovery_calls: Mutex::new(0),
        })
    }

    fn for_discovery(responses: Vec<Result<HttpResponse, TransportError>>) -> Arc<Self> {
        Arc::new(Self {
            device: Mutex::new(Vec::new()),
            token: Mutex::new(Vec::new()),
            discovery: Mutex::new(responses),
            repeat_last: false,
            discovery_calls: Mutex::new(0),
        })
    }

    fn take(
        queue: &Mutex<Vec<Result<HttpResponse, TransportError>>>,
        repeat_last: bool,
    ) -> Result<HttpResponse, TransportError> {
        let mut queue = queue.lock().expect("scripted queue lock");
        if queue.len() == 1 && repeat_last {
            return queue[0].clone();
        }
        if queue.is_empty() {
            return Err(TransportError::new("scripted responses exhausted"));
        }
        queue.remove(0)
    }
}

#[async_trait]
impl HttpTransport for ScriptedTransport {
    async fn post(
        &self,
        url: &str,
        _headers: &HashMap<String, String>,
        _body: RequestBody,
    ) -> Result<HttpResponse, TransportError> {
        if url.contains("/device") {
            return Self::take(&self.device, false);
        }
        Self::take(&self.token, self.repeat_last)
    }

    async fn get(
        &self,
        _url: &str,
        _headers: &HashMap<String, String>,
    ) -> Result<HttpResponse, TransportError> {
        *self.discovery_calls.lock().expect("counter lock") += 1;
        Self::take(&self.discovery, false)
    }
}

/// The injected clock and sleep. Monotonic time advances ONLY by the sleeps,
/// which is what makes a 15-minute deadline run in microseconds.
#[derive(Clone)]
struct FakeTime {
    base: Instant,
    elapsed: Arc<Mutex<Duration>>,
    delays: Arc<Mutex<Vec<u64>>>,
}

impl FakeTime {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            elapsed: Arc::new(Mutex::new(Duration::ZERO)),
            delays: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn clock(&self) -> apcore_toolkit::auth::ClockFn {
        let base = self.base;
        let elapsed = Arc::clone(&self.elapsed);
        Arc::new(move || base + *elapsed.lock().expect("clock lock"))
    }

    fn sleep(&self) -> apcore_toolkit::auth::SleepFn {
        let elapsed = Arc::clone(&self.elapsed);
        let delays = Arc::clone(&self.delays);
        Arc::new(move |duration| {
            *elapsed.lock().expect("clock lock") += duration;
            delays.lock().expect("delay lock").push(duration.as_secs());
            Box::pin(async {})
        })
    }

    fn recorded_delays(&self) -> Vec<u64> {
        self.delays.lock().expect("delay lock").clone()
    }
}

fn pinned_wall_clock(now: u64) -> apcore_toolkit::auth::WallClockFn {
    Arc::new(move || std::time::UNIX_EPOCH + Duration::from_secs(now))
}

// --------------------------------------------------------------- conversions

fn ok_json(status: u16, body: &Value) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse {
        status,
        content_type: Some("application/json".to_string()),
        body: serde_json::to_string(body).expect("fixture body must serialize"),
    })
}

/// One entry of `token_responses`: either an HTTP response or a scripted
/// connection failure.
fn scripted_response(entry: &Value) -> Result<HttpResponse, TransportError> {
    if entry["transport_error"].as_bool().unwrap_or(false) {
        return Err(TransportError::new("scripted transport failure"));
    }
    let status = entry["status"].as_u64().unwrap_or(200) as u16;
    match entry.get("raw_body").and_then(Value::as_str) {
        Some(raw) => Ok(HttpResponse {
            status,
            content_type: entry
                .get("content_type")
                .and_then(Value::as_str)
                .map(str::to_string),
            body: raw.to_string(),
        }),
        None => ok_json(status, &entry["body"]),
    }
}

/// Build a config from a case's `input.config`, which may be absent.
fn config_from(input: &Value) -> DeviceAuthConfig {
    let raw = &input["config"];
    let mut config =
        DeviceAuthConfig::new(raw["client_id"].as_str().unwrap_or("conformance-client"));
    config.device_authorization_endpoint = Some(DEVICE_ENDPOINT.to_string());
    config.token_endpoint = Some(TOKEN_ENDPOINT.to_string());

    if let Some(issuer) = raw["issuer"].as_str() {
        config.issuer = Some(issuer.to_string());
    }
    if let Some(endpoint) = raw["token_endpoint"].as_str() {
        config.token_endpoint = Some(endpoint.to_string());
    }
    if let Some(endpoint) = raw["device_authorization_endpoint"].as_str() {
        config.device_authorization_endpoint = Some(endpoint.to_string());
    }
    if let Some(separator) = raw["scope_separator"].as_str() {
        config.scope_separator = separator.to_string();
    }
    if let Some(secret) = raw["client_secret"].as_str() {
        config.client_secret = Some(secret.to_string());
    }
    if let Some(method) = raw["client_auth_method"].as_str() {
        config.client_auth_method = match method {
            "client_secret_post" => ClientAuthMethod::ClientSecretPost,
            "client_secret_basic" => ClientAuthMethod::ClientSecretBasic,
            _ => ClientAuthMethod::None,
        };
    }
    if let Some(interval) = raw["default_interval"].as_u64() {
        config.default_interval = interval;
    }
    if let Some(aliases) = raw["error_aliases"].as_object() {
        for (from, to) in aliases {
            config
                .error_aliases
                .insert(from.clone(), to.as_str().unwrap_or_default().to_string());
        }
    }
    if let Some(aliases) = raw["field_aliases"].as_object() {
        for (logical, names) in aliases {
            config.field_aliases.insert(
                logical.clone(),
                names
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            );
        }
    }
    if let Some(encoding) = raw["request_encoding"].as_object() {
        for (kind, value) in encoding {
            let encoding = match value.as_str() {
                Some("json") => BodyEncoding::Json,
                _ => BodyEncoding::Form,
            };
            match RequestKind::from_wire(kind) {
                Some(RequestKind::Device) => config.request_encoding.device = encoding,
                Some(RequestKind::Token) => config.request_encoding.token = encoding,
                Some(RequestKind::Refresh) => config.request_encoding.refresh = encoding,
                Some(RequestKind::Revoke) => config.request_encoding.revoke = encoding,
                None => panic!("unknown request_encoding key {kind:?}"),
            }
        }
    }
    if let Some(scope) = input["params"]["scope"].as_array() {
        config.scope = scope
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    // Any other string-valued `input.params` entry is an additional request
    // parameter. Registered for both kinds; `prepare_request` picks the one
    // matching the request being built.
    if let Some(params) = input["params"].as_object() {
        for (name, value) in params {
            let Some(value) = value.as_str() else {
                continue;
            };
            config
                .extra_device_params
                .insert(name.clone(), value.to_string());
            config
                .extra_token_params
                .insert(name.clone(), value.to_string());
        }
    }
    config
}

/// The fixture's outcome vocabulary.
fn outcome_of(result: &Result<TokenSet, DeviceAuthError>) -> String {
    match result {
        Ok(_) => "success".to_string(),
        Err(DeviceAuthError::AuthorizationDenied) => "access_denied".to_string(),
        Err(DeviceAuthError::AuthorizationExpired(ExpiryCause::ServerExpiredToken)) => {
            "expired_token".to_string()
        }
        Err(DeviceAuthError::AuthorizationExpired(ExpiryCause::DeadlineElapsed)) => {
            "deadline_exceeded".to_string()
        }
        Err(DeviceAuthError::Protocol { .. }) => "protocol_error".to_string(),
        Err(DeviceAuthError::RefreshFailed { .. }) => "refresh_failed".to_string(),
        Err(DeviceAuthError::InvalidHookReturn(_)) => "hook_rejected".to_string(),
        Err(other) => format!("<unmapped: {other}>"),
    }
}

/// Decode standard base64, so a Basic header can be round-tripped the way a
/// server does. Only ever fed the crate's own output plus fixture values.
fn base64_decode(input: &str) -> Option<String> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits: u32 = 0;
    let mut count: u32 = 0;
    let mut out: Vec<u8> = Vec::new();
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let index = ALPHABET.iter().position(|c| *c == byte)? as u32;
        bits = (bits << 6) | index;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    String::from_utf8(out).ok()
}

/// Form-decode one component, by round-tripping it through the pair decoder.
fn form_decode_single(value: &str) -> String {
    form_decode(&format!("k={value}"))
        .into_iter()
        .next()
        .map(|(_, decoded)| decoded)
        .unwrap_or_default()
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
}

/// Result of one fixture case.
enum Outcome {
    Pass,
    Fail(String),
}

/// Fail loudly when the fixture asserts something this harness does not read.
///
/// Without this, a corpus that grows a new expectation key passes silently:
/// every `expected["..."].as_str()` on an absent key yields `None` and the
/// check is skipped. That is the failure mode that made fixture cases 045 and
/// 046 vacuous, and a harness can suffer it just as easily as a fixture.
fn guard_expectations(expected: &Value, handled: &[&str]) -> Option<Outcome> {
    let map = expected.as_object()?;
    let unhandled: Vec<&String> = map
        .keys()
        .filter(|key| !handled.contains(&key.as_str()))
        .collect();
    if unhandled.is_empty() {
        return None;
    }
    Some(Outcome::Fail(format!(
        "the fixture asserts {unhandled:?}, which this harness does not check; \
         add the assertion rather than letting the case pass vacuously"
    )))
}

macro_rules! guard {
    ($expected:expr, $handled:expr) => {
        if let Some(failure) = guard_expectations($expected, &$handled) {
            return failure;
        }
    };
}

macro_rules! check {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Outcome::Fail(format!($($arg)*));
        }
    };
}

// ------------------------------------------------------------------ kind: poll

struct PollRun {
    result: Result<TokenSet, DeviceAuthError>,
    summary: PollSummary,
    delays: Vec<u64>,
}

/// Drive the polling state machine over one case's scripted responses.
fn run_poll(input: &Value, wall_clock_now: u64) -> PollRun {
    let config = config_from(input);
    run_poll_with(input, config, wall_clock_now)
}

/// [`run_poll`] with a caller-built config, so a hook case can install one.
fn run_poll_with(input: &Value, config: DeviceAuthConfig, wall_clock_now: u64) -> PollRun {
    let token_responses: Vec<Result<HttpResponse, TransportError>> = input["token_responses"]
        .as_array()
        .map(|list| list.iter().map(scripted_response).collect())
        .unwrap_or_default();
    let transport: SharedTransport = ScriptedTransport::new(
        vec![ok_json(200, &input["device_response"])],
        token_responses,
        input["repeat_last_response"].as_bool().unwrap_or(false),
    );

    let time = FakeTime::new();
    let runtime = AuthRuntime {
        config,
        transport,
        clock: time.clock(),
        sleep: time.sleep(),
        wall_clock: pinned_wall_clock(wall_clock_now),
    };

    let (result, summary) = block_on(DeviceCodeGrant.run(&runtime, &LoginCallbacks::default()));
    PollRun {
        result,
        summary,
        delays: time.recorded_delays(),
    }
}

fn run_poll_case(input: &Value, expected: &Value) -> Outcome {
    guard!(
        expected,
        [
            "outcome",
            "poll_delays",
            "poll_delays_length",
            "poll_delays_all_equal",
            "final_interval",
            "polls_made",
            "token_set"
        ]
    );
    let run = run_poll(input, 1000);

    let got_outcome = outcome_of(&run.result);
    let want_outcome = expected["outcome"].as_str().unwrap_or("<missing>");
    check!(
        got_outcome == want_outcome,
        "outcome: expected {want_outcome:?}, got {got_outcome:?} (error: {:?})",
        run.result.as_ref().err().map(ToString::to_string)
    );

    if let Some(want) = expected["poll_delays"].as_array() {
        let want: Vec<u64> = want.iter().filter_map(Value::as_u64).collect();
        check!(
            run.delays == want,
            "poll_delays: expected {want:?}, got {:?}",
            run.delays
        );
    }
    if let Some(want_length) = expected["poll_delays_length"].as_u64() {
        check!(
            run.delays.len() as u64 == want_length,
            "poll_delays_length: expected {want_length}, got {}",
            run.delays.len()
        );
    }
    if let Some(want_each) = expected["poll_delays_all_equal"].as_u64() {
        check!(
            run.delays.iter().all(|delay| *delay == want_each),
            "poll_delays_all_equal: expected every entry to be {want_each}, got {:?}",
            run.delays
        );
    }
    if let Some(want) = expected["final_interval"].as_u64() {
        check!(
            run.summary.final_interval.as_secs() == want,
            "final_interval: expected {want}, got {}",
            run.summary.final_interval.as_secs()
        );
    }
    if let Some(want) = expected["polls_made"].as_u64() {
        check!(
            u64::from(run.summary.polls_made) == want,
            "polls_made: expected {want}, got {}",
            run.summary.polls_made
        );
    }

    if let Some(want) = expected["token_set"].as_object() {
        let Ok(tokens) = run.result.as_ref() else {
            return Outcome::Fail("token_set asserted but the flow did not succeed".to_string());
        };
        if let Some(token_type) = want.get("token_type").and_then(Value::as_str) {
            check!(
                tokens.token_type == token_type,
                "token_type: expected {token_type:?}, got {:?}",
                tokens.token_type
            );
        }
        if let Some(scope) = want.get("scope").and_then(Value::as_array) {
            let scope: Vec<String> = scope
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            check!(
                tokens.scope == scope,
                "scope: expected {scope:?}, got {:?}",
                tokens.scope
            );
        }
        if let Some(expires_at) = want.get("expires_at").and_then(Value::as_i64) {
            check!(
                tokens.expires_at == Some(expires_at),
                "expires_at: expected {expires_at}, got {:?}",
                tokens.expires_at
            );
        }
    }

    Outcome::Pass
}

// ---------------------------------------------------------------- kind: expiry

fn run_expiry_case(input: &Value, expected: &Value) -> Outcome {
    guard!(expected, ["is_expired"]);
    let tokens = TokenSet {
        access_token: "t".to_string(),
        token_type: "Bearer".to_string(),
        expires_at: input["token_set"]["expires_at"].as_i64(),
        refresh_token: None,
        scope: Vec::new(),
        obtained_at: 0,
    };
    let now = input["now"].as_i64().expect("now must be an integer");
    let skew = input["skew_seconds"].as_i64().unwrap_or(30);
    let want = expected["is_expired"]
        .as_bool()
        .expect("is_expired must be a bool");
    let got = tokens.is_expired(now, skew);
    check!(got == want, "is_expired: expected {want}, got {got}");
    Outcome::Pass
}

// --------------------------------------------------------------- kind: refresh

fn token_set_from_fixture(raw: &Value) -> TokenSet {
    TokenSet {
        access_token: raw["access_token"].as_str().unwrap_or_default().to_string(),
        token_type: raw["token_type"].as_str().unwrap_or("Bearer").to_string(),
        expires_at: raw["expires_at"].as_i64(),
        refresh_token: raw["refresh_token"].as_str().map(str::to_string),
        scope: raw["scope"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        obtained_at: raw["obtained_at"].as_i64().unwrap_or(0),
    }
}

fn run_refresh_case(input: &Value, expected: &Value) -> Outcome {
    guard!(expected, ["outcome", "stored", "store_cleared"]);
    let directory = TempDir::new().expect("temp dir");
    let store = Arc::new(FileTokenStore::at(
        directory.path().join("credentials.json"),
    ));
    let transport: SharedTransport = ScriptedTransport::new(
        Vec::new(),
        vec![scripted_response(&input["response"])],
        false,
    );

    let mut config = config_from(input);
    config.issuer = Some("https://a.example".to_string());
    let now = input["now"].as_u64().unwrap_or(1000);
    let client = DeviceAuthClient::new(config, store.clone())
        .expect("client")
        .with_transport(transport)
        .with_wall_clock(pinned_wall_clock(now));

    block_on(store.save(
        client.store_key(),
        &token_set_from_fixture(&input["stored"]),
    ))
    .expect("seed store");

    let result = block_on(client.refresh());
    let got_outcome = outcome_of(&result);
    let want_outcome = expected["outcome"].as_str().unwrap_or("<missing>");
    check!(
        got_outcome == want_outcome,
        "outcome: expected {want_outcome:?}, got {got_outcome:?}"
    );

    let persisted = block_on(store.load(client.store_key())).expect("load");

    if expected["stored"].is_null() {
        check!(
            persisted.is_none(),
            "expected the store to be cleared, found {persisted:?}"
        );
    } else if let Some(want) = expected["stored"].as_object() {
        let Some(persisted) = persisted else {
            return Outcome::Fail("expected a stored record, found none".to_string());
        };
        if let Some(value) = want.get("access_token").and_then(Value::as_str) {
            check!(
                persisted.access_token == value,
                "stored.access_token: expected {value:?}, got {:?}",
                persisted.access_token
            );
        }
        if let Some(value) = want.get("refresh_token").and_then(Value::as_str) {
            check!(
                persisted.refresh_token.as_deref() == Some(value),
                "stored.refresh_token: expected {value:?}, got {:?}",
                persisted.refresh_token
            );
        }
        if let Some(value) = want.get("expires_at").and_then(Value::as_i64) {
            check!(
                persisted.expires_at == Some(value),
                "stored.expires_at: expected {value}, got {:?}",
                persisted.expires_at
            );
        }
        if let Some(value) = want.get("scope").and_then(Value::as_array) {
            let value: Vec<String> = value
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            check!(
                persisted.scope == value,
                "stored.scope: expected {value:?}, got {:?} \
                 (a merged record would keep the old scope)",
                persisted.scope
            );
        }
    }

    if expected["store_cleared"].as_bool().unwrap_or(false) {
        check!(
            block_on(store.load(client.store_key()))
                .expect("load")
                .is_none(),
            "store_cleared: the record survived"
        );
    }

    Outcome::Pass
}

// ------------------------------------------------------------- kind: redaction

fn run_redaction_case(input: &Value, expected: &Value) -> Outcome {
    guard!(expected, ["must_not_contain"]);
    let tokens = token_set_from_fixture(&input["token_set"]);
    let rendered = format!("{tokens:?}");
    let displayed = tokens.to_string();
    for forbidden in expected["must_not_contain"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let forbidden = forbidden.as_str().unwrap_or_default();
        check!(
            !rendered.contains(forbidden),
            "Debug output contains {forbidden:?}: {rendered}"
        );
        check!(
            !displayed.contains(forbidden),
            "Display output contains {forbidden:?}: {displayed}"
        );
    }
    Outcome::Pass
}

// ------------------------------------------------------- kind: alias_validation

fn run_alias_validation_case(input: &Value, expected: &Value) -> Outcome {
    guard!(expected, ["valid"]);
    let mut aliases = std::collections::BTreeMap::new();
    for (from, to) in input["error_aliases"]
        .as_object()
        .expect("error_aliases must be an object")
    {
        aliases.insert(from.clone(), to.as_str().unwrap_or_default().to_string());
    }
    let want = expected["valid"].as_bool().expect("valid must be a bool");
    let got = validate_error_aliases(&aliases).is_ok();
    check!(
        got == want,
        "validity: expected {want}, got {got} ({:?})",
        validate_error_aliases(&aliases)
            .err()
            .map(|e| e.to_string())
    );

    // Construction rejects it too, not just the standalone validator: that is
    // what "rejected at construction" means for a consumer.
    let mut config = DeviceAuthConfig::new("cid");
    config.error_aliases = aliases;
    check!(
        config.validate().is_ok() == want,
        "DeviceAuthConfig::validate disagreed with validate_error_aliases"
    );
    Outcome::Pass
}

// --------------------------------------------------------- kind: discovery_url

fn run_discovery_url_case(input: &Value, expected: &Value) -> Outcome {
    guard!(expected, ["candidates", "third_candidate"]);
    let issuer = input["issuer"].as_str().expect("issuer must be a string");
    let got = discovery_candidates(issuer);

    if let Some(want) = expected["candidates"].as_array() {
        let want: Vec<String> = want
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        check!(got == want, "candidates: expected {want:?}, got {got:?}");
    }
    if let Some(want) = expected["third_candidate"].as_str() {
        check!(
            got.get(2).map(String::as_str) == Some(want),
            "third_candidate: expected {want:?}, got {:?}",
            got.get(2)
        );
    }
    Outcome::Pass
}

// ------------------------------------------------------------- kind: discovery

/// The `issuer` the scripted metadata declares, from either fixture shape.
fn declared_issuer(input: &Value) -> Option<String> {
    if let Some(issuer) = input["metadata"]["issuer"].as_str() {
        return Some(issuer.to_string());
    }
    let responses = input.get("responses")?.as_array()?;
    let raw = responses.first()?.get("raw_body")?.as_str()?;
    let parsed: Value = serde_json::from_str(raw).ok()?;
    parsed.get("issuer")?.as_str().map(str::to_string)
}

fn run_discovery_case(input: &Value, expected: &Value) -> Outcome {
    guard!(
        expected,
        [
            "accepted",
            "proceeds",
            "warned",
            "candidate_used",
            "token_endpoint",
            "device_authorization_endpoint",
            "reason"
        ]
    );
    let config = {
        let mut config =
            DeviceAuthConfig::new(input["config"]["client_id"].as_str().unwrap_or("cid"));
        config.issuer = input["config"]["issuer"].as_str().map(str::to_string);
        config.token_endpoint = input["config"]["token_endpoint"]
            .as_str()
            .map(str::to_string);
        config.device_authorization_endpoint = input["config"]["device_authorization_endpoint"]
            .as_str()
            .map(str::to_string);
        config
    };
    let issuer = config
        .issuer
        .clone()
        .expect("a discovery case must configure an issuer");

    // Two fixture shapes: a single `metadata` document, or an ordered list of
    // scripted well-known `responses` (used to assert fall-through).
    let responses: Vec<Result<HttpResponse, TransportError>> =
        match input.get("responses").and_then(Value::as_array) {
            Some(list) => list.iter().map(scripted_response).collect(),
            None => vec![ok_json(200, &input["metadata"])],
        };
    let transport = ScriptedTransport::for_discovery(responses);
    let result = block_on(config.discover_with(transport.as_ref(), &issuer));

    if let Some(accepted) = expected["accepted"].as_bool() {
        check!(
            result.is_ok() == accepted,
            "accepted: expected {accepted}, got {} ({:?})",
            result.is_ok(),
            result.as_ref().err().map(ToString::to_string)
        );
        if !accepted {
            let message = result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();

            // The fixture names WHY a document was refused, so a rejection for
            // the wrong reason does not count as a pass.
            if let Some(reason) = expected["reason"].as_str() {
                let matched = match reason {
                    "insecure_endpoint" => message.contains("not https"),
                    other => return Outcome::Fail(format!("unknown rejection reason {other:?}")),
                };
                check!(matched, "reason {reason:?} not evident in error: {message}");
            }

            // Cases 054 and 055 carry no `reason`, but their metadata declares
            // a mismatching `issuer` and is otherwise COMPLETE -- deliberately
            // so, because both previously omitted device_authorization_endpoint
            // and therefore failed whether or not the issuer check fired. The
            // reason is derived from the fixture input rather than hardcoded
            // per case: whenever the scripted metadata declares an issuer that
            // is not byte-identical to the configured one, the error must say
            // so, or the rejection came from somewhere else entirely.
            if let Some(declared) = declared_issuer(input) {
                if declared != issuer {
                    check!(
                        message.contains(&declared) && message.contains("issuer"),
                        "the metadata declared issuer {declared:?} against configured \
                         {issuer:?}, so the rejection must name the mismatch; got: {message}"
                    );
                }
            }
            return Outcome::Pass;
        }
    }
    if let Some(proceeds) = expected["proceeds"].as_bool() {
        check!(
            result.is_ok() == proceeds,
            "proceeds: expected {proceeds}, got {} ({:?})",
            result.is_ok(),
            result.as_ref().err().map(ToString::to_string)
        );
    }

    let Ok((resolved, report)) = result else {
        return Outcome::Fail(format!(
            "discovery failed: {:?}",
            result.err().map(|e| e.to_string())
        ));
    };

    if let Some(want) = expected["token_endpoint"].as_str() {
        check!(
            resolved.token_endpoint.as_deref() == Some(want),
            "token_endpoint: expected {want:?}, got {:?}",
            resolved.token_endpoint
        );
    }
    if let Some(want) = expected["device_authorization_endpoint"].as_str() {
        check!(
            resolved.device_authorization_endpoint.as_deref() == Some(want),
            "device_authorization_endpoint: expected {want:?}, got {:?}",
            resolved.device_authorization_endpoint
        );
    }
    if let Some(want) = expected["candidate_used"].as_u64() {
        check!(
            report.candidate_used == Some(want as usize),
            "candidate_used: expected {want}, got {:?}",
            report.candidate_used
        );
    }
    if let Some(want) = expected["warned"].as_bool() {
        let warned = !report.warnings.is_empty();
        check!(
            warned == want,
            "warned: expected {want}, got {warned} ({:?})",
            report.warnings
        );
    }
    Outcome::Pass
}

// ----------------------------------------------------------------- kind: parse

/// Assert that every key/value in `want` appears in `got`, comparing JSON
/// values so a form-decoded `"3600"` is not silently equated with `3600`.
fn assert_subset(got: &ParsedBody, want: &serde_json::Map<String, Value>) -> Option<String> {
    for (key, value) in want {
        match got.get(key) {
            Some(actual) if actual == value => {}
            Some(actual) => {
                return Some(format!("parsed[{key:?}]: expected {value}, got {actual}"))
            }
            None => return Some(format!("parsed is missing {key:?}: {got:?}")),
        }
    }
    None
}

fn run_parse_case(input: &Value, expected: &Value) -> Outcome {
    guard!(expected, ["parsed", "outcome", "raw_body_preserved"]);
    let content_type = input["content_type"].as_str();
    let raw_body = input["raw_body"].as_str().unwrap_or_default();

    // Case 051: an unrecognised error envelope must become a protocol error
    // carrying the raw body, which is a dispatch property rather than a
    // decoding one, so it is driven through the state machine.
    if input["as"].as_str() == Some("error") {
        let status = input["status"].as_u64().unwrap_or(400);
        let poll_input = json!({
            "device_response": {
                "device_code": "dc", "user_code": "AB-CD",
                "verification_uri": "https://e.example/device",
                "expires_in": 600, "interval": 5
            },
            "token_responses": [{
                "status": status,
                "content_type": content_type,
                "raw_body": raw_body,
            }],
        });
        let run = run_poll(&poll_input, 1000);
        let got = outcome_of(&run.result);
        let want = expected["outcome"].as_str().unwrap_or("protocol_error");
        check!(got == want, "outcome: expected {want:?}, got {got:?}");
        if expected["raw_body_preserved"].as_bool().unwrap_or(false) {
            match run.result {
                Err(DeviceAuthError::Protocol { raw_body: kept, .. }) => {
                    check!(
                        kept.as_deref() == Some(raw_body),
                        "raw_body_preserved: expected {raw_body:?}, got {kept:?}"
                    );
                }
                other => return Outcome::Fail(format!("expected a Protocol error, got {other:?}")),
            }
        }
        return Outcome::Pass;
    }

    let Some(decoded) = decode_body(content_type, raw_body) else {
        return Outcome::Fail(format!("body did not decode: {raw_body:?}"));
    };
    let decoded = if input["as"].as_str() == Some("device") {
        normalise_device_body(&decoded, None)
    } else {
        decoded
    };

    if let Some(want) = expected["parsed"].as_object() {
        if let Some(reason) = assert_subset(&decoded, want) {
            return Outcome::Fail(reason);
        }
    }
    Outcome::Pass
}

// --------------------------------------------------------------- kind: request

/// The request body, decoded back into pairs regardless of encoding.
fn body_pairs(body: &RequestBody) -> Vec<(String, String)> {
    match body {
        RequestBody::Form(text) => form_decode(text),
        RequestBody::Json(text) => serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .map(|map| {
                map.into_iter()
                    .map(|(key, value)| {
                        (
                            key,
                            value
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| value.to_string()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        RequestBody::Empty => Vec::new(),
    }
}

fn run_request_case(input: &Value, expected: &Value) -> Outcome {
    guard!(
        expected,
        [
            "body_contains",
            "body_excludes",
            "headers_contain",
            "headers_exclude",
            "encoding_by_kind",
            "basic_credentials_roundtrip",
            "form_body_roundtrip"
        ]
    );
    let config = config_from(input);

    let kinds: Vec<RequestKind> = match input["kinds"].as_array() {
        Some(list) => list
            .iter()
            .filter_map(|v| v.as_str().and_then(RequestKind::from_wire))
            .collect(),
        None => vec![input["kind"]
            .as_str()
            .and_then(RequestKind::from_wire)
            .unwrap_or(RequestKind::Device)],
    };

    for kind in kinds {
        let base = match kind {
            RequestKind::Device => device_params(&config),
            RequestKind::Refresh => refresh_params("r1"),
            _ => token_params("dc"),
        };
        let request = prepare_request(&config, kind, base);
        let pairs = body_pairs(&request.body);

        if let Some(want) = expected["body_contains"].as_object() {
            for (key, value) in want {
                let value = value.as_str().unwrap_or_default().to_string();
                check!(
                    pairs.contains(&(key.clone(), value.clone())),
                    "{}: body_contains {key:?}={value:?}, got {pairs:?}",
                    kind.as_str()
                );
            }
        }
        for key in expected["body_excludes"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let key = key.as_str().unwrap_or_default();
            check!(
                !pairs.iter().any(|(name, _)| name == key),
                "{}: body_excludes {key:?}, got {pairs:?}",
                kind.as_str()
            );
        }
        if let Some(want) = expected["headers_contain"].as_object() {
            for (name, value) in want {
                let value = value.as_str().unwrap_or_default();
                check!(
                    request.headers.get(name).map(String::as_str) == Some(value),
                    "{}: headers_contain {name:?}={value:?}, got {:?}",
                    kind.as_str(),
                    request.headers
                );
            }
        }
        for name in expected["headers_exclude"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let name = name.as_str().unwrap_or_default();
            check!(
                !request.headers.contains_key(name),
                "{}: headers_exclude {name:?}, got {:?}",
                kind.as_str(),
                request.headers
            );
        }
        // Asserted as the ROUND TRIP a server performs, not as exact bytes:
        // the three languages' form encoders disagree on how a space, `*` and
        // `~` are spelled, and every one of those spellings decodes
        // identically. What must not vary is that encoding happens at all --
        // raw concatenation turns the secret's `+` into a space.
        if let Some(want) = expected["basic_credentials_roundtrip"].as_object() {
            let Some(header) = request.headers.get("Authorization") else {
                return Outcome::Fail(format!(
                    "{}: no Authorization header to round-trip",
                    kind.as_str()
                ));
            };
            let Some(encoded) = header.strip_prefix("Basic ") else {
                return Outcome::Fail(format!("expected a Basic header, got {header:?}"));
            };
            let Some(decoded) = base64_decode(encoded) else {
                return Outcome::Fail(format!("Basic payload is not valid base64: {encoded:?}"));
            };
            // Split on the FIRST ':' exactly as a server does -- which is why
            // both halves must be encoded: an unencoded ':' in the secret
            // would move the split point.
            let Some((raw_id, raw_secret)) = decoded.split_once(':') else {
                return Outcome::Fail(format!("Basic payload has no ':' separator: {decoded:?}"));
            };
            let got_id = form_decode_single(raw_id);
            let got_secret = form_decode_single(raw_secret);
            if let Some(value) = want.get("client_id").and_then(Value::as_str) {
                check!(
                    got_id == value,
                    "{}: client_id round-tripped to {got_id:?}, expected {value:?}",
                    kind.as_str()
                );
            }
            if let Some(value) = want.get("client_secret").and_then(Value::as_str) {
                check!(
                    got_secret == value,
                    "{}: client_secret round-tripped to {got_secret:?}, expected {value:?} \
                     (raw concatenation decodes a '+' back as a space)",
                    kind.as_str()
                );
            }
        }

        // Same reasoning for the body: a conforming
        // application/x-www-form-urlencoded serialization is one that
        // form-decodes back to the values that went in.
        if let Some(want) = expected["form_body_roundtrip"].as_object() {
            check!(
                matches!(request.body, RequestBody::Form(_)),
                "{}: form_body_roundtrip needs a form-encoded body, got {:?}",
                kind.as_str(),
                request.encoding
            );
            for (name, value) in want {
                let value = value.as_str().unwrap_or_default().to_string();
                check!(
                    pairs.contains(&(name.clone(), value.clone())),
                    "{}: {name:?} round-tripped wrong, expected {value:?}, got {pairs:?}",
                    kind.as_str()
                );
            }
        }

        if let Some(want) = expected["encoding_by_kind"].as_object() {
            if let Some(value) = want.get(kind.as_str()).and_then(Value::as_str) {
                let want_encoding = match value {
                    "json" => BodyEncoding::Json,
                    _ => BodyEncoding::Form,
                };
                check!(
                    request.encoding == want_encoding,
                    "{}: encoding expected {value:?}, got {:?}",
                    kind.as_str(),
                    request.encoding
                );
            }
        }
    }
    Outcome::Pass
}

// -------------------------------------------------------------- kind: callback

/// Assert what `on_user_code` is handed. The device response is scripted and
/// the flow is allowed to run on into the poll loop, which then runs out of
/// scripted responses -- the callback has already fired by then, and the
/// outcome of the poll is not what this kind is about.
fn run_callback_case(input: &Value, expected: &Value) -> Outcome {
    guard!(expected, ["on_user_code"]);

    let seen: Arc<Mutex<Vec<UserCodeEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let callbacks = LoginCallbacks {
        on_user_code: Some(Arc::new(move |event| {
            sink.lock().expect("sink lock").push(event.clone());
        })),
        // 060c supplies one; 060 and 060b omit it.
        timeout_seconds: input["timeout_seconds"].as_f64(),
        ..Default::default()
    };

    let transport: SharedTransport = ScriptedTransport::new(
        vec![ok_json(200, &input["device_response"])],
        Vec::new(),
        false,
    );
    let time = FakeTime::new();
    let runtime = AuthRuntime {
        config: config_from(input),
        transport,
        clock: time.clock(),
        sleep: time.sleep(),
        wall_clock: pinned_wall_clock(1000),
    };
    let _ = block_on(DeviceCodeGrant.run(&runtime, &callbacks));

    let events = seen.lock().expect("sink lock");
    check!(
        events.len() == 1,
        "on_user_code must fire exactly once, fired {} time(s)",
        events.len()
    );
    let event = &events[0];

    let Some(want) = expected["on_user_code"].as_object() else {
        return Outcome::Fail("expected.on_user_code must be an object".to_string());
    };
    for (field, value) in want {
        let got = match field.as_str() {
            "verification_uri" => Value::String(event.verification_uri.clone()),
            "user_code" => Value::String(event.user_code.clone()),
            "verification_uri_complete" => match &event.verification_uri_complete {
                Some(text) => Value::String(text.clone()),
                None => Value::Null,
            },
            // A plain number, never null: the fallback is applied before the
            // consumer sees it.
            "expires_in" => Value::from(event.expires_in),
            other => {
                return Outcome::Fail(format!(
                    "the fixture asserts on_user_code.{other:?}, which this harness does not read"
                ))
            }
        };
        check!(
            &got == value,
            "on_user_code.{field}: expected {value}, got {got}"
        );
    }
    Outcome::Pass
}

// ------------------------------------------------------------------ kind: hook

fn run_hook_case(case: &Value, input: &Value, expected: &Value, all: &[Value]) -> Outcome {
    guard!(
        expected,
        [
            "body_contains",
            "headers_contain",
            "parsed",
            "raises",
            "identifier",
            "outcome",
            "identical_to_baseline",
        ]
    );
    match input["hook"].as_str() {
        Some("transform_request") => {
            let params: Vec<(String, String)> = input["returns"]["params"]
                .as_object()
                .map(|map| {
                    map.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let headers: Vec<(String, String)> = input["returns"]["headers"]
                .as_object()
                .map(|map| {
                    map.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                        .collect()
                })
                .unwrap_or_default();

            let mut config = config_from(input);
            config.hooks.transform_request = Some(Arc::new(move |_kind, mut p, mut h| {
                for (key, value) in &params {
                    p.insert(key.clone(), value.clone());
                }
                for (name, value) in &headers {
                    h.insert(name.clone(), value.clone());
                }
                (p, h)
            }));
            let request = prepare_request(&config, RequestKind::Device, device_params(&config));
            let pairs = body_pairs(&request.body);
            if let Some(want) = expected["body_contains"].as_object() {
                for (key, value) in want {
                    let value = value.as_str().unwrap_or_default().to_string();
                    check!(
                        pairs.contains(&(key.clone(), value.clone())),
                        "body_contains {key:?}={value:?}, got {pairs:?}"
                    );
                }
            }
            if let Some(want) = expected["headers_contain"].as_object() {
                for (name, value) in want {
                    let value = value.as_str().unwrap_or_default();
                    check!(
                        request.headers.get(name).map(String::as_str) == Some(value),
                        "headers_contain {name:?}={value:?}, got {:?}",
                        request.headers
                    );
                }
            }
            Outcome::Pass
        }

        Some("parse_response") => {
            // `returns: null` means the hook has no opinion, so the built-in
            // parser must still produce a usable body.
            let mut config = config_from(input);
            config.hooks.parse_response = Some(Arc::new(|_kind, _status, _ct, _raw| None));
            let response = HttpResponse {
                status: 200,
                content_type: input["content_type"].as_str().map(str::to_string),
                body: input["raw_body"].as_str().unwrap_or_default().to_string(),
            };
            let Some(decoded) = decode_response(&config, RequestKind::Token, &response) else {
                return Outcome::Fail(
                    "parse_response returning None did not fall back to the built-in parser"
                        .to_string(),
                );
            };
            if let Some(want) = expected["parsed"].as_object() {
                if let Some(reason) = assert_subset(&decoded, want) {
                    return Outcome::Fail(reason);
                }
            }
            Outcome::Pass
        }

        Some("classify_error") => {
            let returns = input["returns"].as_str().map(str::to_string);
            let mut config = config_from(input);
            config.hooks.classify_error = Some(Arc::new(move |_body| returns.clone()));

            // The hook is reached only for an identifier normalisation has not
            // already resolved -- that is the recorded ordering rule, and it is
            // why cases 045 and 046 carry a deliberately unrecognised code.
            let body = match input["body"].as_object() {
                Some(map) => map.clone(),
                None => {
                    return Outcome::Fail(
                        "a classify_error case must supply input.body; without one the case \
                         cannot distinguish a hook that ran from one that was skipped"
                            .to_string(),
                    )
                }
            };

            let result = classify(&config, &body);

            if expected["raises"].as_bool().unwrap_or(false) {
                check!(
                    matches!(result, Err(DeviceAuthError::InvalidHookReturn(_))),
                    "expected the invalid return to be rejected loudly, got {result:?}"
                );
            }

            // `identifier` is asserted whenever the KEY is present, including
            // when its value is JSON null ("no opinion"). Reading it with
            // `as_str()` alone would skip the null case silently.
            if let Some(want) = expected.get("identifier") {
                let got = match &result {
                    Ok(Some(identifier)) => Value::String(identifier.as_str().to_string()),
                    Ok(None) => Value::Null,
                    Err(e) => Value::String(format!("<error: {e}>")),
                };
                check!(&got == want, "identifier: expected {want}, got {got}");
            }

            // An `outcome` expectation is a dispatch property, so it is driven
            // through the whole state machine rather than through `classify`.
            if let Some(want) = expected["outcome"].as_str() {
                let mut poll_input = json!({
                    "device_response": {
                        "device_code": "dc", "user_code": "AB-CD",
                        "verification_uri": "https://e.example/device",
                        "expires_in": 600, "interval": 5
                    },
                    "token_responses": [{"status": 400, "body": input["body"]}],
                });
                if let Some(config) = input.get("config") {
                    poll_input["config"] = config.clone();
                }
                let returns = input["returns"].as_str().map(str::to_string);
                let mut config = config_from(&poll_input);
                config.hooks.classify_error = Some(Arc::new(move |_body| returns.clone()));
                let run = run_poll_with(&poll_input, config, 1000);
                let got = outcome_of(&run.result);
                check!(got == want, "outcome: expected {want:?}, got {got:?}");
            }

            Outcome::Pass
        }

        // Case 048: no hooks installed reproduces the baseline case exactly.
        None => {
            let baseline_id = input["baseline_case"]
                .as_str()
                .expect("a hook case with no hook must name a baseline_case");
            let Some(baseline) = all.iter().find(|c| c["id"].as_str() == Some(baseline_id)) else {
                return Outcome::Fail(format!("baseline case {baseline_id:?} not found"));
            };
            check!(
                expected["identical_to_baseline"].as_bool().unwrap_or(true),
                "unsupported hook expectation on case {}",
                case["id"].as_str().unwrap_or("<unknown>")
            );
            run_poll_case(&baseline["input"], &baseline["expected"])
        }

        Some(other) => Outcome::Fail(format!("unknown hook {other:?}")),
    }
}

// ------------------------------------------------------------------ the runner

#[test]
fn device_auth_matches_shared_conformance_fixture() {
    let cases = load_cases();
    if cases.is_empty() {
        return;
    }

    let mut failures: Vec<String> = Vec::new();
    let mut passed: usize = 0;

    for case in &cases {
        let id = case["id"].as_str().unwrap_or("<unknown>");
        let description = case["description"].as_str().unwrap_or("");
        let kind = case["kind"].as_str().unwrap_or("<missing>");
        let input = &case["input"];
        let expected = &case["expected"];

        let outcome = match kind {
            "poll" => run_poll_case(input, expected),
            "expiry" => run_expiry_case(input, expected),
            "refresh" => run_refresh_case(input, expected),
            "redaction" => run_redaction_case(input, expected),
            "alias_validation" => run_alias_validation_case(input, expected),
            "discovery_url" => run_discovery_url_case(input, expected),
            "discovery" => run_discovery_case(input, expected),
            "parse" => run_parse_case(input, expected),
            "request" => run_request_case(input, expected),
            "callback" => run_callback_case(input, expected),
            "hook" => run_hook_case(case, input, expected, &cases),
            other => Outcome::Fail(format!("unknown case kind {other:?}")),
        };

        match outcome {
            Outcome::Pass => passed += 1,
            Outcome::Fail(reason) => {
                failures.push(format!("\nCase {id} ({kind}): {description}\n  {reason}"));
            }
        }
    }

    eprintln!(
        "device_auth: {} passed, {} failed, of {} shared conformance case(s)",
        passed,
        failures.len(),
        cases.len()
    );

    assert!(
        failures.is_empty(),
        "{} of {} case(s) failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
    assert_eq!(
        passed,
        cases.len(),
        "every fixture case must be run: the corpus is the cross-SDK contract"
    );
    // A floor, not an equality: the corpus is the authority and grows as
    // divergences are found, so pinning an exact count turns every legitimate
    // addition into a false failure. A DROP still fails here, which is the
    // direction worth catching -- silently losing cases is how coverage rots.
    assert!(
        cases.len() >= 57,
        "the corpus has shrunk to {} cases, below the 57 the spec enumerates",
        cases.len()
    );
}
