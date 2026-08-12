use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::audit::Record;
use crate::config::Config;
use crate::detector::{DetectorClient, DetectorError};
use crate::mapping::{Mapping, MappingError};
use crate::provider::{read_pointer, write_pointer, Anthropic, OpenAi, Provider, ShapeError};
use crate::session::{key_from, Limits, SessionError, SessionStore};

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("{0}")]
    Shape(#[from] ShapeError),
    #[error("{0}")]
    Detector(#[from] DetectorError),
    #[error("{0}")]
    Mapping(#[from] MappingError),
    #[error("upstream request failed: {0}")]
    Upstream(String),
    #[error("{0}")]
    Session(#[from] crate::session::SessionError),
    #[error("{0}")]
    Audit(#[from] crate::audit::AuditError),
}

impl ProxyError {
    fn status(&self) -> StatusCode {
        match self {
            // A body we cannot read is the client's to fix, and it is refused
            // rather than forwarded unmasked. So is a session the gateway
            // cannot honour as asked.
            ProxyError::Shape(ShapeError::Request(_))
            | ProxyError::Shape(ShapeError::Unsupported(_, _))
            | ProxyError::Session(SessionError::BadId)
            | ProxyError::Session(SessionError::Disabled)
            | ProxyError::Session(SessionError::NoCredential(_)) => StatusCode::BAD_REQUEST,
            // Saturation is this gateway's own capacity rather than anything
            // the caller got wrong, and the same request may well succeed a
            // moment later. No `Retry-After`: the wait is another request's
            // detector round-trip, and the gateway has no honest number for it.
            // A journal that cannot be written is the same kind of fact about
            // this gateway rather than about the caller.
            ProxyError::Session(SessionError::Saturated) | ProxyError::Audit(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::BAD_GATEWAY,
        }
    }

    /// The fixed vocabulary the journal records. A class rather than the
    /// message, so that no expression in the audit writer could interpolate
    /// submitted text even if a message one day carried it.
    fn audit_class(&self) -> &'static str {
        match self {
            ProxyError::Shape(ShapeError::Request(_)) => "shape_request",
            ProxyError::Shape(ShapeError::Unsupported(_, _)) => "shape_unsupported",
            ProxyError::Shape(_) => "shape_response",
            ProxyError::Detector(DetectorError::Transport(_)) => "detector_transport",
            ProxyError::Detector(DetectorError::Status(_)) => "detector_status",
            ProxyError::Mapping(MappingError::Unknown(_)) => "mapping_unknown_placeholder",
            ProxyError::Mapping(MappingError::BadSpan(_)) => "mapping_bad_span",
            ProxyError::Mapping(MappingError::BadEntityType(_)) => "mapping_bad_entity_type",
            ProxyError::Upstream(_) => "upstream_failed",
            ProxyError::Session(SessionError::BadId) => "session_bad_id",
            ProxyError::Session(SessionError::Disabled) => "session_disabled",
            ProxyError::Session(SessionError::NoCredential(_)) => "session_no_credential",
            ProxyError::Session(SessionError::Saturated) => "session_saturated",
            ProxyError::Audit(_) => "audit_write_failed",
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        // The reason names the failure. It never carries the submitted text.
        (self.status(), Json(json!({ "error": self.to_string() }))).into_response()
    }
}

/// Headers the upstream needs to authenticate and route the call. An allowlist
/// rather than a passthrough: `host`, `content-length` and friends belong to
/// the hop we are making, not the one we received, and forwarding a client's
/// cookies to a model provider is nobody's intent.
/// Per provider, so a caller holding credentials for both does not send the
/// Anthropic key to OpenAI. A secret crossing a provider boundary is a leak of
/// a different kind than the one this proxy was built for, but a leak.
const OPENAI_HEADERS: [&str; 4] = [
    "authorization",
    "openai-organization",
    "openai-project",
    "openai-beta",
];
const ANTHROPIC_HEADERS: [&str; 3] = ["x-api-key", "anthropic-version", "anthropic-beta"];

/// Response headers a client needs to behave well against a provider that is
/// pushing back.
const RETURNED_HEADERS: [&str; 6] = [
    "retry-after",
    "x-ratelimit-limit-requests",
    "x-ratelimit-remaining-requests",
    "x-ratelimit-reset-requests",
    "anthropic-ratelimit-requests-remaining",
    "anthropic-ratelimit-requests-reset",
];

pub struct AppState {
    pub detector: DetectorClient,
    pub upstream: reqwest::Client,
    pub openai_base: String,
    pub anthropic_base: String,
    pub sessions: SessionStore,
    pub audit: Arc<crate::audit::Audit>,
}

impl AppState {
    pub fn from_config(config: &Config, audit: Arc<crate::audit::Audit>) -> Self {
        Self {
            detector: DetectorClient::new(
                config.detector_url.clone(),
                Duration::from_secs(config.detector_timeout_secs),
            ),
            upstream: reqwest::Client::new(),
            openai_base: config.openai_base.clone(),
            anthropic_base: config.anthropic_base.clone(),
            sessions: SessionStore::new(Limits {
                idle: Duration::from_secs(config.session_idle_secs),
                max_sessions: config.max_sessions,
                max_values: config.max_session_values,
            }),
            audit,
        }
    }

    fn base_for(&self, provider: &dyn Provider) -> &str {
        match provider.name() {
            "anthropic" => &self.anthropic_base,
            _ => &self.openai_base,
        }
    }
}

/// Detect and mask every text the provider pointed at, and report what was
/// found. Shared by both branches of `handle`: inline it would exist twice and
/// diverge at the first edit.
///
/// The counts describe *this request's* texts. Counting the mapping instead
/// would report a session's running total on every turn, and the record would
/// stop describing the request. The values themselves live in the local set
/// only long enough to be counted and never leave this function.
async fn mask_all(
    detector: &DetectorClient,
    body: &Value,
    pointers: &[String],
    mapping: &mut Mapping,
) -> Result<(Value, usize, BTreeMap<String, usize>), ProxyError> {
    let mut masked = body.clone();
    let mut total = 0usize;
    let mut distinct: HashSet<(String, String)> = HashSet::new();
    for pointer in pointers {
        let text = read_pointer(body, pointer)?;
        let spans = detector.detect(&text).await?;
        total += spans.len();
        let characters: Vec<char> = text.chars().collect();
        for span in &spans {
            // Offsets are in characters, and a span the mapping would reject
            // is counted only if it addresses real text.
            if let Some(value) = characters.get(span.start..span.end) {
                distinct.insert((span.entity_type.clone(), value.iter().collect()));
            }
        }
        write_pointer(&mut masked, pointer, &mapping.mask(&text, &spans)?)?;
    }
    let mut types: BTreeMap<String, usize> = BTreeMap::new();
    for (entity_type, _) in distinct {
        *types.entry(entity_type).or_default() += 1;
    }
    Ok((masked, total, types))
}

/// Which branch `handle` took. `serve` uses this to decide whether it is the
/// one that gets to record this request's outcome: a streamed response's own
/// handle — the clone `handle` hands to `restore_stream` — already claims
/// that job, and `serve` calling `completed` too would race the drop that
/// actually decides it, leaving whichever call ran last to overwrite whatever
/// the other one wrote.
enum Handled {
    Buffered(Response),
    Streamed(Response),
}

async fn handle(
    state: Arc<AppState>,
    provider: &'static dyn Provider,
    headers: HeaderMap,
    body: Value,
    record: &Record,
) -> Result<Handled, ProxyError> {
    // Attribution first, before even the shape check: a request refused for
    // its body or its session id should still say whose it was. Resolved
    // once — `credential_of` borrows from `headers`, which outlives the
    // function — so the digest sent here and the one sent below cannot drift
    // apart into two different readings of the same header.
    let credential = crate::session::credential_of(&headers, provider);
    if let Some(credential) = credential {
        record.attribute(state.audit.digest(&[credential]), None);
    }

    // Where is the text? A shape we do not recognize is refused, not forwarded.
    let pointers = provider.request_pointers(&body)?;

    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        record.streaming();
    }

    // Resolved before detection: a malformed header must cost nothing, not a
    // second per 1 200 characters.
    let key = key_from(&headers, provider, state.sessions.enabled())?;

    if let (Some(credential), Some(key)) = (credential, key.as_ref()) {
        record.attribute(
            state.audit.digest(&[credential]),
            Some(state.audit.digest(&[credential, key.id().as_bytes()])),
        );
    }

    // One mapping for the whole request so a value keeps one name; seeded from
    // the conversation's table so it keeps that name across turns too.
    let (masked, mapping) = match key {
        Some(key) => {
            let claimed = state.sessions.acquire(&key)?;
            let mut guard = match claimed.guard {
                Some(guard) => guard,
                None => Arc::clone(&claimed.session.mapping).lock_owned().await,
            };
            let mut work = guard.clone();
            let (masked, spans, types) =
                mask_all(&state.detector, &body, &pointers, &mut work).await?;
            record.detected(pointers.len(), spans, types);
            // After the last `?`, and on a copy until here: a refused request
            // leaves the session exactly as it was, so a client whose detector
            // blinked does not carry a hole in its numbering for the rest of
            // the conversation.
            guard.absorb(&work, state.sessions.max_values());
            (masked, work)
            // `guard` is dropped here — before the upstream call, so a stream
            // that runs for minutes holds no lock on its session.
        }
        None => {
            let mut work = Mapping::new();
            let (masked, spans, types) =
                mask_all(&state.detector, &body, &pointers, &mut work).await?;
            record.detected(pointers.len(), spans, types);
            (masked, work)
        }
    };

    // Durable before anything leaves the perimeter. This ordering is the whole
    // reason the journal exists, so it happens here and not a line later.
    record.masked().await?;

    // Only what is masked leaves the process.
    let mut request = state.upstream.post(format!(
        "{}{}",
        state.base_for(provider),
        provider.upstream_path()
    ));
    let allowed: &[&str] = match provider.name() {
        "anthropic" => &ANTHROPIC_HEADERS,
        _ => &OPENAI_HEADERS,
    };
    for name in allowed {
        let header = HeaderName::from_static(name);
        if let Some(value) = headers.get(&header) {
            request = request.header(header, value);
        }
    }
    let response = request
        .json(&masked)
        .send()
        .await
        .map_err(|error| ProxyError::Upstream(error.to_string()))?;

    let status = StatusCode::from_u16(response.status().as_u16())
        .map_err(|error| ProxyError::Upstream(error.to_string()))?;
    // The rate-limit headers are what let a client back off as the provider
    // asked; rebuilding the response without them silently drops that.
    let mut returned = HeaderMap::new();
    for name in RETURNED_HEADERS {
        let header = HeaderName::from_static(name);
        if let Some(value) = response.headers().get(&header) {
            returned.insert(header, value.clone());
        }
    }
    // A stream is restored as it arrives. A non-success status is not a stream,
    // whatever its content type, and keeps the buffered path below.
    if status.is_success()
        && response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            // Media types are case-insensitive, and a parameter may follow.
            .and_then(|value| value.split(';').next())
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/event-stream"))
    {
        return Ok(Handled::Streamed(crate::stream::restore_stream(
            response,
            provider,
            mapping,
            returned,
            record.clone(),
        )));
    }

    let raw = response
        .bytes()
        .await
        .map_err(|error| ProxyError::Upstream(error.to_string()))?;

    if !status.is_success() {
        // The provider's status and error envelope carry retry semantics the
        // client needs; turning a 429 into a generic 502 loses them. The body
        // may still echo what we sent, so it is restored before it goes back —
        // and an error body is not always JSON, so text is handled too.
        return Ok(Handled::Buffered(
            match serde_json::from_slice::<Value>(&raw) {
                Ok(parsed) => {
                    (status, returned, Json(mapping.restore_value(&parsed)?)).into_response()
                }
                Err(_) => {
                    let text = String::from_utf8_lossy(&raw);
                    (status, returned, mapping.restore(&text)?).into_response()
                }
            },
        ));
    }

    let upstream: Value =
        serde_json::from_slice(&raw).map_err(|error| ProxyError::Upstream(error.to_string()))?;

    // Restore, and refuse rather than hand a placeholder to the client.
    let mut restored = upstream.clone();
    for pointer in provider.response_pointers(&upstream)? {
        let text = read_pointer(&upstream, &pointer)?;
        write_pointer(&mut restored, &pointer, &mapping.restore(&text)?)?;
    }
    // The same quota headers matter on a 200: a client that only learns its
    // remaining budget from errors learns it too late.
    Ok(Handled::Buffered(
        (returned, Json(restored)).into_response(),
    ))
}

async fn openai(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    serve(state, &OpenAi, "/v1/chat/completions", headers, body).await
}

async fn anthropic(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    serve(state, &Anthropic, "/v1/messages", headers, body).await
}

/// Where the request's outcome becomes a value one can record.
///
/// The record is constructed here rather than inside `handle` because a
/// `ProxyError` has no status until `into_response` runs, and a bare `?` inside
/// `handle` unwinds past any guard without saying which failure occurred. A
/// guard that dropped on `handle`'s return would have to invent both fields.
async fn serve(
    state: Arc<AppState>,
    provider: &'static dyn Provider,
    route: &'static str,
    headers: HeaderMap,
    body: Value,
) -> Response {
    let record = Record::new(Arc::clone(&state.audit), provider.name(), route);
    match handle(state, provider, headers, body, &record).await {
        // The wrapper is the only handle a buffered response ever gets, so
        // this is the whole outcome.
        Ok(Handled::Buffered(response)) => {
            record.completed(response.status().as_u16());
            response
        }
        // `restore_stream` holds its own clone of `record` and calls
        // `completed` or `stream_failed` itself once the stream actually
        // ends. Recording anything here too would race that drop: whichever
        // call happened to run last would overwrite the other's answer, and
        // a wrapper that always wins would put back the bug this exists to
        // fix — an outcome decided before the stream ever ran.
        Ok(Handled::Streamed(response)) => response,
        Err(error) => {
            record.refused(error.status().as_u16(), error.audit_class());
            error.into_response()
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(openai))
        .route("/v1/messages", post(anthropic))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use wiremock::matchers::path as path_matcher;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::session::{SessionKey, SESSION_HEADER};

    const SECRET: &str = "Weber";

    fn person_span() -> Value {
        json!([{"entity_type": "PERSON", "start": 0, "end": 5, "confidence": 1.0,
                "recognizer": "ner:fake", "tier": 2, "boosted": false}])
    }

    async fn detector_returning(spans: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"spans": spans, "layers_run": ["deterministic"]})),
            )
            .mount(&server)
            .await;
        server
    }

    async fn failing_detector() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        server
    }

    async fn upstream_returning(route: &str, body: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    fn test_limits() -> Limits {
        Limits {
            idle: Duration::from_secs(1800),
            max_sessions: 8,
            max_values: 8,
        }
    }

    /// A state whose journal is a fresh file, returned alongside it so a test
    /// can read what was written.
    fn state_with(
        detector: &MockServer,
        upstream: &MockServer,
        limits: Limits,
    ) -> (Arc<AppState>, tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        let audit = Arc::new(crate::audit::Audit::open(&path).expect("opens"));
        let state = Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5)),
            upstream: reqwest::Client::new(),
            openai_base: upstream.uri(),
            anthropic_base: upstream.uri(),
            sessions: SessionStore::new(limits),
            audit,
        });
        (state, dir, path)
    }

    /// The journal's lines, parsed. The `TempDir` must outlive the call.
    fn journal(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .expect("readable")
            .lines()
            .map(|line| serde_json::from_str(line).expect("one JSON object per line"))
            .collect()
    }

    fn session_headers<'a>(credential: &'a str, id: &'a str) -> [(&'a str, &'a str); 2] {
        [("authorization", credential), (SESSION_HEADER, id)]
    }

    fn test_key(id: &str, credential: &str) -> SessionKey {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", credential.parse().unwrap());
        headers.insert(HeaderName::from_static(SESSION_HEADER), id.parse().unwrap());
        key_from(&headers, &OpenAi, true).unwrap().unwrap()
    }

    /// A detector that finds "Weber" and nothing else. wiremock takes the
    /// first mount that matches, so the specific rule is mounted first.
    async fn detector_finding_weber() -> MockServer {
        use wiremock::matchers::body_string_contains;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .and(body_string_contains(SECRET))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"spans": person_span(), "layers_run": ["deterministic"]}),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"spans": [], "layers_run": ["deterministic"]})),
            )
            .mount(&server)
            .await;
        server
    }

    fn state(detector: &MockServer, upstream: &MockServer) -> Arc<AppState> {
        state_with(detector, upstream, test_limits()).0
    }

    async fn call(state: Arc<AppState>, route: &str, body: Value) -> (StatusCode, String) {
        call_with_headers(state, route, body, &[]).await
    }

    async fn call_with_headers(
        state: Arc<AppState>,
        route: &str,
        body: Value,
        headers: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(route)
            .header("content-type", "application/json");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let response = router(state)
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn the_upstream_never_sees_the_original() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "Hallo [PERSON_1]"}}]}),
        )
        .await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let received = &upstream.received_requests().await.unwrap()[0];
        let sent = String::from_utf8(received.body.clone()).unwrap();
        assert!(
            !sent.contains(SECRET),
            "the original reached the upstream: {sent}"
        );
        assert!(sent.contains("[PERSON_1]"));
    }

    #[tokio::test]
    async fn the_client_gets_the_original_back() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "Hallo [PERSON_1]"}}]}),
        )
        .await;

        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Hallo Weber"), "not restored: {body}");
        assert!(
            !body.contains("PERSON_1"),
            "a placeholder reached the client: {body}"
        );
    }

    #[tokio::test]
    async fn a_detector_failure_refuses_the_request() {
        let detector = failing_detector().await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "fail closed means the upstream is never called"
        );
    }

    #[tokio::test]
    async fn an_unparsable_body_refuses_the_request() {
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt"}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_lost_mapping_refuses_the_response() {
        // The upstream invents a placeholder nobody issued. Handing it to the
        // client would put "[PERSON_9]" where a name belongs.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "Hallo [PERSON_9]"}}]}),
        )
        .await;

        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(!body.contains("PERSON_9") || body.contains("no mapping"));
    }

    #[tokio::test]
    async fn errors_never_carry_the_original_text() {
        let detector = failing_detector().await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;

        let (_, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user",
                   "content": "Weber, IBAN CH9300762011623852957"}]}),
        )
        .await;

        assert!(
            !body.contains(SECRET),
            "the error body echoed the text: {body}"
        );
        assert!(!body.contains("CH9300762011623852957"));
    }

    #[tokio::test]
    async fn an_upstream_error_keeps_its_status_and_body() {
        // A 429 turned into a generic 502 loses the retry semantics the client
        // needs to behave well.
        let detector = detector_returning(json!([])).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_json(
                json!({"error": {"type": "rate_limit_error", "message": "slow down"}}),
            ))
            .mount(&upstream)
            .await;

        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(
            body.contains("rate_limit_error"),
            "the envelope was lost: {body}"
        );
    }

    #[tokio::test]
    async fn an_upstream_error_still_gets_its_placeholders_restored() {
        // Providers quote the offending request back; that quote is masked.
        let detector = detector_returning(person_span()).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"error": {"message": "bad request near [PERSON_1]"}})),
            )
            .mount(&upstream)
            .await;

        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("near Weber"), "not restored: {body}");
    }

    #[tokio::test]
    async fn identifier_fields_outside_content_are_masked() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "user": "Weber",
                   "messages": [{"role": "user", "name": "Weber", "content": "Weber fragt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let sent =
            String::from_utf8(upstream.received_requests().await.unwrap()[0].body.clone()).unwrap();
        assert!(!sent.contains(SECRET), "an identifier field leaked: {sent}");
    }

    #[tokio::test]
    async fn a_malformed_span_refuses_the_request() {
        // A detector contract bug must not become raw egress.
        let detector = detector_returning(
            json!([{"entity_type": "PERSON", "start": 0, "end": 999, "confidence": 1.0,
                    "recognizer": "ner:fake", "tier": 2, "boosted": false}]),
        )
        .await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn provider_credentials_reach_the_upstream() {
        // Without these the proxy is not a drop-in: every authenticated request
        // fails before it reaches a model.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

        let (status, _) = call_with_headers(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Hallo"}]}),
            &[
                ("authorization", "Bearer sk-test"),
                ("openai-organization", "org-1"),
            ],
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let received = &upstream.received_requests().await.unwrap()[0];
        assert_eq!(received.headers["authorization"], "Bearer sk-test");
        assert_eq!(received.headers["openai-organization"], "org-1");
    }

    #[tokio::test]
    async fn anthropic_credentials_and_version_reach_the_upstream() {
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "text", "text": "ok"}]}),
        )
        .await;

        let (status, _) = call_with_headers(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "Hallo"}]}),
            &[
                ("x-api-key", "sk-ant-test"),
                ("anthropic-version", "2023-06-01"),
            ],
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let received = &upstream.received_requests().await.unwrap()[0];
        assert_eq!(received.headers["x-api-key"], "sk-ant-test");
        assert_eq!(received.headers["anthropic-version"], "2023-06-01");
    }

    #[tokio::test]
    async fn headers_outside_the_allowlist_are_not_forwarded() {
        // A client's cookies are not the model provider's business.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

        call_with_headers(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Hallo"}]}),
            &[
                ("cookie", "session=secret"),
                ("authorization", "Bearer sk-test"),
            ],
        )
        .await;

        let received = &upstream.received_requests().await.unwrap()[0];
        assert!(
            received.headers.get("cookie").is_none(),
            "a cookie was forwarded"
        );
    }

    /// An SSE upstream that sends `body` and then closes.
    ///
    /// The content-type has to travel through `set_body_raw`'s mime
    /// parameter, not a separately inserted header: `ResponseTemplate`
    /// stores a body call's mime apart from its headers and applies it last,
    /// so a header inserted earlier is silently overwritten by whichever body
    /// call runs after it.
    async fn upstream_streaming(route: &str, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher(route))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;
        server
    }

    const STREAM_BODY: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hallo [PER\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"SON_1]!\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    /// An upstream that promises more body than it sends and then drops the
    /// connection. wiremock cannot sever a stream mid-body, and the claim under
    /// test is exactly what happens when one is severed.
    async fn truncating_upstream(body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut scratch = vec![0u8; 8192];
            let _ = socket.read(&mut scratch).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                        content-length: 100000\r\n\r\n";
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            // Dropped here, far short of the promised length.
        });
        base
    }

    fn state_for(detector: &MockServer, upstream_base: String) -> Arc<AppState> {
        // The `TempDir` is dropped here rather than threaded through: nothing
        // in the streaming tests reads the journal back, and the open file
        // handle keeps working after the directory entry is unlinked.
        let dir = tempfile::tempdir().expect("a temp dir");
        let audit =
            Arc::new(crate::audit::Audit::open(&dir.path().join("audit.jsonl")).expect("opens"));
        Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5)),
            upstream: reqwest::Client::new(),
            openai_base: upstream_base.clone(),
            anthropic_base: upstream_base,
            sessions: SessionStore::new(test_limits()),
            audit,
        })
    }

    #[tokio::test]
    async fn a_severed_stream_still_serves_what_was_already_restored() {
        // The waiting event is restored and safe; the connection dying does not
        // make it unsafe, and dropping it would lose text the client paid for.
        let detector = detector_returning(person_span()).await;
        let base = truncating_upstream(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hallo [PERSON_1]\"}}]}\n\n",
        )
        .await;

        let (status, served) = call(
            state_for(&detector, base),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            served.contains(SECRET),
            "restored text was dropped: {served}"
        );
        assert!(
            served.contains("tessera_restoration_failed"),
            "the break was not reported: {served}"
        );
    }

    #[tokio::test]
    async fn an_extended_thinking_request_never_reaches_the_upstream() {
        // Refusing at the first streamed block would already have cost the
        // caller the call and its tokens.
        let detector = detector_returning(json!([])).await;
        let upstream = MockServer::start().await;
        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "stream": true,
                   "thinking": {"type": "enabled", "budget_tokens": 1024},
                   "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_streaming_response_reaches_the_client_restored() {
        // The placeholder is split across two events; the client must see the
        // value once, whole, and never the token.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_streaming("/v1/chat/completions", STREAM_BODY).await;

        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(served.contains(SECRET), "not restored: {served}");
        assert!(!served.contains("PERSON_1"), "placeholder served: {served}");
        assert!(served.ends_with("data: [DONE]\n\n"), "truncated: {served}");
    }

    #[tokio::test]
    async fn a_stream_holds_no_session_lock() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_streaming("/v1/chat/completions", STREAM_BODY).await;
        let state = state(&detector, &upstream);

        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", "Bearer k1")
            .header(SESSION_HEADER, "conv-1")
            .body(Body::from(
                json!({"model": "gpt", "stream": true,
                       "messages": [{"role": "user", "content": "Weber schreibt"}]})
                .to_string(),
            ))
            .unwrap();
        let response = router(Arc::clone(&state)).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // The stream has not been read yet. If the masking guard were still
        // held, this would fail — and a stream that hung would block its
        // conversation for as long as it hung.
        // `acquire` itself claims the mapping's lock synchronously whenever it
        // is free, so a successful claim here — without a second, separate
        // `try_lock` that would deadlock against `claimed`'s own guard — is
        // exactly the proof that nothing else (in particular, no still-live
        // stream) is holding it.
        let claimed = state
            .sessions
            .acquire(&test_key("conv-1", "Bearer k1"))
            .unwrap();
        assert!(
            claimed.guard.is_some(),
            "the stream is holding its session lock"
        );

        // Draining afterwards proves the stream still restores from the
        // snapshot it was handed.
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let served = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            served.contains(SECRET),
            "the stream did not restore: {served}"
        );
    }

    #[tokio::test]
    async fn the_anthropic_stream_shape_is_restored_too() {
        // Anthropic carries text under a different pointer and separates blocks
        // with events that carry none.
        let detector = detector_returning(person_span()).await;
        let upstream = MockServer::start().await;
        let body = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\
             \"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
             \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hallo [PER\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
             \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"SON_1]\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&upstream)
            .await;

        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "stream": true,
                   "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(served.contains(SECRET), "not restored: {served}");
        assert!(!served.contains("PERSON_1"), "placeholder served: {served}");
        assert!(
            served.contains("event: message_stop"),
            "truncated: {served}"
        );
    }

    #[tokio::test]
    async fn an_oddly_cased_media_type_is_still_a_stream() {
        // `Text/Event-Stream; charset=utf-8` is the same media type, and missing
        // it would buffer a live response and fail to parse it as JSON.
        let detector = detector_returning(person_span()).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                STREAM_BODY.as_bytes().to_vec(),
                "Text/Event-Stream; charset=utf-8",
            ))
            .mount(&upstream)
            .await;

        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(served.contains(SECRET), "not streamed: {served}");
        assert!(!served.contains("PERSON_1"), "placeholder served: {served}");
    }

    #[tokio::test]
    async fn a_streaming_response_is_served_as_a_stream() {
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_streaming("/v1/chat/completions", "data: [DONE]\n\n").await;
        let response = router(state(&detector, &upstream))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": "gpt", "stream": true,
                               "messages": [{"role": "user", "content": "Hallo"}]})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
    }

    #[tokio::test]
    async fn the_upstream_is_still_told_to_stream() {
        // Dropping the flag would make the provider answer with a whole body the
        // client is not waiting for.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_streaming("/v1/chat/completions", "data: [DONE]\n\n").await;
        call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;
        let received = &upstream.received_requests().await.unwrap()[0];
        let sent: Value = serde_json::from_slice(&received.body).unwrap();
        assert_eq!(sent["stream"], json!(true));
    }

    #[tokio::test]
    async fn a_stream_carrying_an_unknown_placeholder_ends_with_an_error() {
        // Bytes have already gone out, so the request cannot be refused. It ends
        // instead — the client never receives a token in place of a name.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hallo [PERSON_9]\"}}]}\n\n",
                "data: [DONE]\n\n",
            ),
        )
        .await;

        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(served.contains("tessera_restoration_failed"), "{served}");
        assert!(
            !served.contains("data: [DONE]"),
            "served as complete: {served}"
        );
    }

    #[tokio::test]
    async fn a_broken_event_does_not_take_the_good_ones_with_it() {
        // The stream ends on the malformed event, but the delta before it was
        // already restored and correct, and the client gets it.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hallo [PERSON_1]\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"trunc\n\n",
            ),
        )
        .await;

        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            served.contains(SECRET),
            "restored text was dropped: {served}"
        );
        assert!(
            served.contains("tessera_restoration_failed"),
            "the failure was not reported: {served}"
        );
        assert!(!served.contains("PERSON_1"), "placeholder served: {served}");
    }

    #[tokio::test]
    async fn a_streaming_error_response_keeps_the_buffered_path() {
        // A 429 is not a stream, whatever it says it is.
        let detector = detector_returning(json!([])).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "3")
                    // `set_body_raw`'s mime, not a separate header: a header
                    // inserted alongside `set_body_json` would be overwritten
                    // by that call's own `application/json` mime, and this
                    // test's whole premise is a response that claims to be a
                    // stream while carrying a non-success status.
                    .set_body_raw(
                        json!({"error": {"message": "slow down"}}).to_string(),
                        "text/event-stream",
                    ),
            )
            .mount(&upstream)
            .await;

        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(served.contains("slow down"), "{served}");
    }

    #[tokio::test]
    async fn a_non_json_upstream_error_keeps_its_status_and_body() {
        let detector = detector_returning(json!([])).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"))
            .mount(&upstream)
            .await;

        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("bad gateway"), "the body was lost: {body}");
    }

    #[tokio::test]
    async fn retry_after_survives_an_upstream_rate_limit() {
        let detector = detector_returning(json!([])).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "30")
                    .set_body_json(json!({"error": {"message": "slow down"}})),
            )
            .mount(&upstream)
            .await;

        let response = router(state(&detector, &upstream))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": "gpt",
                               "messages": [{"role": "user", "content": "Hallo"}]})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["retry-after"], "30");
    }

    #[tokio::test]
    async fn one_providers_key_never_reaches_the_other() {
        // A caller holding both sets of credentials must not have the Anthropic
        // key posted to OpenAI.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

        call_with_headers(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Hallo"}]}),
            &[
                ("authorization", "Bearer sk-openai"),
                ("x-api-key", "sk-ant-secret"),
            ],
        )
        .await;

        let received = &upstream.received_requests().await.unwrap()[0];
        assert_eq!(received.headers["authorization"], "Bearer sk-openai");
        assert!(
            received.headers.get("x-api-key").is_none(),
            "the Anthropic key crossed to OpenAI"
        );
    }

    #[tokio::test]
    async fn a_tool_bearing_request_is_refused() {
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "tools": [{"type": "function"}],
                   "messages": [{"role": "user", "content": "Weber"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_anthropic_shape_is_masked_too() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "text", "text": "Hallo [PERSON_1]"}]}),
        )
        .await;

        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({
                "model": "claude",
                "system": "Weber ist der Mandant",
                "messages": [{"role": "user", "content": [{"type": "text", "text": "Weber fragt"}]}]
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let sent =
            String::from_utf8(upstream.received_requests().await.unwrap()[0].body.clone()).unwrap();
        assert!(!sent.contains(SECRET), "the system field leaked: {sent}");
        assert!(body.contains("Hallo Weber"));
    }

    #[tokio::test]
    async fn one_session_keeps_one_value_on_one_placeholder() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);
        let headers = session_headers("Bearer k1", "conv-1");

        for _ in 0..2 {
            call_with_headers(
                Arc::clone(&state),
                "/v1/chat/completions",
                json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
                &headers,
            )
            .await;
        }

        let received = upstream.received_requests().await.unwrap();
        let first = String::from_utf8(received[0].body.clone()).unwrap();
        let second = String::from_utf8(received[1].body.clone()).unwrap();
        assert!(first.contains("[PERSON_1]"));
        assert!(
            second.contains("[PERSON_1]"),
            "the second turn renamed the same person: {second}"
        );
    }

    #[tokio::test]
    async fn a_guessed_session_id_returns_no_other_callers_value() {
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "[PERSON_1]"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);

        // The first caller puts Weber into the session called "shared".
        call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "shared"),
        )
        .await;

        // A second caller guesses the id but holds a different key, and asks
        // the model to echo the placeholder back.
        let (status, body) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "[PERSON_1] wer?"}]}),
            &session_headers("Bearer k2", "shared"),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.contains(SECRET),
            "another caller's value came back: {body}"
        );
        assert!(body.contains("[PERSON_1]"));
    }

    #[tokio::test]
    async fn a_refused_request_leaves_the_session_untouched() {
        // The first message masks; the second has no "Weber" and the detector
        // refuses it, so the request dies after masking has already happened.
        let detector = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .and(wiremock::matchers::body_string_contains(SECRET))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"spans": person_span(), "layers_run": ["deterministic"]}),
                ),
            )
            .mount(&detector)
            .await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&detector)
            .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);

        let (status, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [
                {"role": "user", "content": "Weber schreibt"},
                {"role": "user", "content": "und dann?"}
            ]}),
            &session_headers("Bearer k1", "conv-1"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        let session = state
            .sessions
            .acquire(&test_key("conv-1", "Bearer k1"))
            .unwrap()
            .session;
        assert!(
            session.mapping.lock().await.is_empty(),
            "a refused request left values in the session"
        );
    }

    #[tokio::test]
    async fn a_failing_request_does_not_evict_a_live_third_party_session() {
        // Session "a" commits a real value through an ordinary successful
        // request. Session "b" then takes the store's second slot with a
        // request that fails during masking, leaving its entry created but
        // empty — `acquire` runs, and creates the entry, before `mask_all`
        // can fail. A third session's request also fails during masking,
        // and needs a slot in a now-full store: it must evict "b"
        // (reclaimable), never "a" (live) — even though "a" is the older
        // of the two by last_seen.
        let detector = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .and(wiremock::matchers::body_string_contains(SECRET))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"spans": person_span(), "layers_run": ["deterministic"]}),
                ),
            )
            .mount(&detector)
            .await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&detector)
            .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state_with(
            &detector,
            &upstream,
            Limits {
                idle: Duration::from_secs(1800),
                max_sessions: 2,
                max_values: 8,
            },
        )
        .0;

        // "a" gets a real, committed value.
        call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "a"),
        )
        .await;

        // "b" has no "Weber" in it, so the detector 503s and the request
        // fails — but its session entry was already created, and stays
        // empty and reclaimable.
        let (status_b, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "und dann?"}]}),
            &session_headers("Bearer k1", "b"),
        )
        .await;
        assert_eq!(status_b, StatusCode::BAD_GATEWAY);

        // "c" also fails during masking, and needs a slot in a store
        // already holding {a, b} at its cap of 2.
        let (status_c, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "und dann?"}]}),
            &session_headers("Bearer k1", "c"),
        )
        .await;
        assert_eq!(status_c, StatusCode::BAD_GATEWAY);

        // Session "a" must still hold Weber's placeholder.
        let session_a = state
            .sessions
            .acquire(&test_key("a", "Bearer k1"))
            .unwrap()
            .session;
        assert_eq!(
            session_a
                .mapping
                .lock()
                .await
                .restore("[PERSON_1]")
                .unwrap(),
            "Weber",
            "a failing request for a different session evicted a's live value"
        );
    }

    #[tokio::test]
    async fn a_value_past_the_cap_is_still_masked_and_still_restored() {
        let two_spans = json!([
            {"entity_type": "PERSON", "start": 0, "end": 5, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
            {"entity_type": "PERSON", "start": 10, "end": 15, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false}
        ]);
        let detector = detector_returning(two_spans).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant",
                   "content": "[PERSON_1] und [PERSON_2]"}}]}),
        )
        .await;
        let state = state_with(
            &detector,
            &upstream,
            Limits {
                idle: Duration::from_secs(1800),
                max_sessions: 8,
                max_values: 1,
            },
        )
        .0;

        let (status, body) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber und Meier"}]}),
            &session_headers("Bearer k1", "conv-1"),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let sent =
            String::from_utf8(upstream.received_requests().await.unwrap()[0].body.clone()).unwrap();
        assert!(!sent.contains(SECRET), "the value past the cap went up raw");
        assert!(
            !sent.contains("Meier"),
            "the value past the cap went up raw"
        );
        assert!(body.contains(SECRET) && body.contains("Meier"));

        let session = state
            .sessions
            .acquire(&test_key("conv-1", "Bearer k1"))
            .unwrap()
            .session;
        assert_eq!(session.mapping.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn no_header_creates_no_session() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);

        let (status, _) = call(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state.sessions.live(),
            0,
            "a request that asked for no session got one anyway"
        );
    }

    #[tokio::test]
    async fn a_session_header_against_a_disabled_gateway_is_refused() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state_with(
            &detector,
            &upstream,
            Limits {
                idle: Duration::ZERO,
                max_sessions: 0,
                max_values: 0,
            },
        )
        .0;

        let (status, body) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "conv-1"),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("disabled"), "{body}");
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            0,
            "a refused request still reached the provider"
        );
    }

    #[tokio::test]
    async fn a_malformed_session_id_is_refused() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

        let (status, _) = call_with_headers(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "conv 1"),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            detector.received_requests().await.unwrap().len(),
            0,
            "a malformed header cost a detection pass"
        );
    }

    #[tokio::test]
    async fn a_saturated_store_refuses_before_the_detector_runs() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state_with(
            &detector,
            &upstream,
            Limits {
                idle: Duration::from_secs(1800),
                max_sessions: 1,
                max_values: 8,
            },
        )
        .0;

        // The only slot belongs to a session another request is inside right
        // now, exactly as it would be mid-`mask_all`.
        let mut held = state
            .sessions
            .acquire(&test_key("conv-1", "Bearer k1"))
            .unwrap();
        let _guard = held
            .guard
            .take()
            .expect("a fresh session is always claimable");

        let (status, body) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "conv-2"),
        )
        .await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("in flight"), "{body}");
        assert_eq!(
            detector.received_requests().await.unwrap().len(),
            0,
            "a refused request cost a detection pass"
        );
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            0,
            "a refused request still reached the provider"
        );
    }

    #[tokio::test]
    async fn a_served_request_leaves_two_records() {
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, _) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let lines = journal(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"], "masked");
        assert_eq!(lines[0]["provider"], "openai");
        assert_eq!(lines[0]["types"]["PERSON"], 1);
        assert_eq!(lines[0]["spans"], 1);
        assert_eq!(lines[1]["result"], "completed");
        assert_eq!(lines[1]["status"], 200);
        assert_eq!(lines[0]["request"], lines[1]["request"]);
    }

    #[tokio::test]
    async fn a_detector_failure_leaves_one_record_and_calls_nobody() {
        let detector = failing_detector().await;
        let upstream = upstream_returning("/v1/chat/completions", json!({})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, _) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        let lines = journal(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["event"], "outcome");
        assert_eq!(lines[0]["upstream"], false);
        assert_eq!(lines[0]["result"], "refused");
        assert_eq!(lines[0]["error"], "detector_status");
        assert!(
            upstream
                .received_requests()
                .await
                .expect("recorded")
                .is_empty(),
            "nothing may reach the provider when the request is refused"
        );
    }

    #[tokio::test]
    async fn each_refusal_records_its_own_class() {
        // The invariant a guard that inferred its outcome would violate
        // silently, so it is exercised per variant rather than once.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({})).await;

        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, _) = call_with_headers(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "hello"}]}),
            &[(SESSION_HEADER, "not a valid id!")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(journal(&path)[0]["error"], "session_bad_id");

        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, _) = call(state, "/v1/chat/completions", json!({"messages": "wrong"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(journal(&path)[0]["error"], "shape_request");
    }

    #[tokio::test]
    async fn a_journal_that_cannot_be_written_refuses_before_the_provider() {
        // Fail-closed end to end: no evidence, no request.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let state = Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5)),
            upstream: reqwest::Client::new(),
            openai_base: upstream.uri(),
            anthropic_base: upstream.uri(),
            sessions: SessionStore::new(test_limits()),
            audit: Arc::new(crate::audit::failing_audit_for_tests()),
        });

        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("audit unavailable"));
        assert!(
            !body.contains('/'),
            "no filesystem detail reaches the client"
        );
        assert!(
            upstream
                .received_requests()
                .await
                .expect("recorded")
                .is_empty(),
            "an unrecorded request must not reach the provider"
        );
    }

    #[tokio::test]
    async fn the_masked_record_precedes_the_provider_call() {
        // Asserted structurally: the upstream mock reads the journal as it
        // answers, so the ordering is observed rather than timed.
        let detector = detector_finding_weber().await;
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        let audit = Arc::new(crate::audit::Audit::open(&path).expect("opens"));

        let seen = Arc::new(std::sync::Mutex::new(0usize));
        let upstream = MockServer::start().await;
        let counter = Arc::clone(&seen);
        let watched = path.clone();
        Mock::given(method("POST"))
            .and(path_matcher("/v1/chat/completions"))
            .respond_with(move |_: &wiremock::Request| {
                *counter.lock().expect("lock") = std::fs::read_to_string(&watched)
                    .map(|text| text.lines().count())
                    .unwrap_or(0);
                ResponseTemplate::new(200)
                    .set_body_json(json!({"choices": [{"message": {"content": "ok"}}]}))
            })
            .mount(&upstream)
            .await;

        let state = Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5)),
            upstream: reqwest::Client::new(),
            openai_base: upstream.uri(),
            anthropic_base: upstream.uri(),
            sessions: SessionStore::new(test_limits()),
            audit,
        });
        call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;

        assert_eq!(
            *seen.lock().expect("lock"),
            1,
            "the masked record must be on disk before the provider is called"
        );
    }

    #[tokio::test]
    async fn a_session_turn_counts_its_own_request_not_the_table() {
        // The detector reports a PERSON at a fixed span regardless of the
        // text, so each turn masks exactly one value of its own — but a
        // different one each time. The session's mapping therefore
        // accumulates to two entries while each request's own count stays at
        // one; a version that read the count off the mapping instead of the
        // request would report two on the second turn.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let bodies = [
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
            json!({"messages": [{"role": "user", "content": "Meier called"}]}),
        ];
        for body in bodies {
            call_with_headers(
                Arc::clone(&state),
                "/v1/chat/completions",
                body,
                &session_headers("sk-tenant", "chat-1"),
            )
            .await;
        }

        let lines = journal(&path);
        let masked: Vec<&Value> = lines
            .iter()
            .filter(|line| line["event"] == "masked")
            .collect();
        assert_eq!(masked.len(), 2);
        assert_eq!(
            masked[1]["types"]["PERSON"], 1,
            "the second turn describes the request, not the session's running total"
        );
        assert_eq!(
            masked[0]["tenant"], masked[1]["tenant"],
            "one credential is one tenant"
        );
        assert_eq!(masked[0]["session"], masked[1]["session"]);
        assert_eq!(masked[0]["tenant"].as_str().expect("a digest").len(), 32);
    }

    #[tokio::test]
    async fn a_request_without_a_session_still_has_a_tenant() {
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        call_with_headers(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "hello"}]}),
            &[("authorization", "sk-tenant")],
        )
        .await;

        let lines = journal(&path);
        assert!(lines[0]["tenant"].is_string());
        assert!(lines[0]["session"].is_null());
    }

    #[tokio::test]
    async fn the_journal_never_carries_the_submitted_value() {
        let detector = detector_finding_weber().await;
        // The provider echoes the placeholder back inside an error body, which
        // is restored to the real value on the way out — the one path where a
        // value exists on the response side too.
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_json(json!({"error": {"message": "[PERSON_1] is rate limited"}})),
            )
            .mount(&upstream)
            .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(
            body.contains(SECRET),
            "the client does get the restored value"
        );

        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(!text.contains(SECRET), "the journal does not");
        assert!(!text.contains("PERSON_1"), "nor a placeholder name");
    }

    #[tokio::test]
    async fn a_whole_stream_records_completed() {
        let detector = detector_finding_weber().await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, _) = call(
            state,
            "/v1/chat/completions",
            json!({"stream": true, "messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let lines = journal(&path);
        assert_eq!(
            lines[0]["stream"], true,
            "the masked record knows it is a stream"
        );
        assert_eq!(lines[1]["result"], "completed");
        assert_eq!(lines[1]["status"], 200);
        assert_eq!(
            lines[1]["upstream"], true,
            "bytes did leave before the stream finished"
        );
    }

    #[tokio::test]
    async fn an_unrestorable_token_records_stream_failed() {
        // The provider invents a placeholder no mapping knows. Bytes have
        // already gone out, so the stream ends rather than the request being
        // refused — and the record says so with the status the client got.
        let detector = detector_finding_weber().await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"[PERSON_9]\"}}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"stream": true, "messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the head was already sent");
        assert!(
            body.contains("error"),
            "the client is told the stream failed"
        );

        let lines = journal(&path);
        assert_eq!(lines[1]["result"], "stream_failed");
        assert_eq!(lines[1]["status"], 200);
        assert_eq!(lines[1]["error"], "stream_unrestorable");
        assert_eq!(lines[1]["upstream"], true);
    }

    /// Build a streaming request and hand back the unread `Response` — the
    /// body's generator has not been polled once, so nothing in it has run.
    async fn streamed_response(state: Arc<AppState>) -> Response {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"stream": true, "messages": [{"role": "user", "content": "hello"}]})
                    .to_string(),
            ))
            .unwrap();
        router(state).oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn the_stream_finishes_the_record_not_the_wrapper() {
        // If the wrapper finalized a streamed request, the outcome line would
        // already exist the instant `handle` returns — before the body is
        // drained, let alone a single event restored.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let response = streamed_response(state).await;
        assert_eq!(response.status(), StatusCode::OK);

        assert_eq!(
            journal(&path).len(),
            1,
            "only the masked line exists before the body is ever read"
        );

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("[DONE]"));

        let lines = journal(&path);
        assert_eq!(lines.len(), 2, "exactly one outcome, written once");
        assert_eq!(lines[1]["event"], "outcome");
        assert_eq!(lines[1]["result"], "completed");
    }

    #[tokio::test]
    async fn a_dropped_stream_is_recorded_as_aborted() {
        // The client vanished before the stream ever ran: no upstream break,
        // no restoration failure, no success either. None of
        // `restore_stream`'s three signalling exits fires, so the record must
        // not assume success just because nothing else was said.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let response = streamed_response(state).await;
        assert_eq!(response.status(), StatusCode::OK);

        // Neither `serve` nor the generator ever calls `completed`: the
        // wrapper's own handle is already gone (`serve` returned), and the
        // generator inside this unread body has not run a single statement.
        drop(response);

        let lines = journal(&path);
        assert_eq!(lines.len(), 2, "the dropped handle still writes its line");
        assert_eq!(lines[1]["result"], "aborted");
    }
}
