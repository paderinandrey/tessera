use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::config::Config;
use crate::detector::{DetectorClient, DetectorError};
use crate::mapping::{Mapping, MappingError};
use crate::provider::{read_pointer, write_pointer, Anthropic, OpenAi, Provider, ShapeError};

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
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let status = match self {
            // A body we cannot read is the client's to fix, and it is refused
            // rather than forwarded unmasked.
            ProxyError::Shape(ShapeError::Request(_))
            | ProxyError::Shape(ShapeError::Unsupported(_, _)) => StatusCode::BAD_REQUEST,
            _ => StatusCode::BAD_GATEWAY,
        };
        // The reason names the failure. It never carries the submitted text.
        (status, Json(json!({ "error": self.to_string() }))).into_response()
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
}

impl AppState {
    pub fn from_config(config: &Config) -> Self {
        Self {
            detector: DetectorClient::new(
                config.detector_url.clone(),
                Duration::from_secs(config.detector_timeout_secs),
            ),
            upstream: reqwest::Client::new(),
            openai_base: config.openai_base.clone(),
            anthropic_base: config.anthropic_base.clone(),
        }
    }

    fn base_for(&self, provider: &dyn Provider) -> &str {
        match provider.name() {
            "anthropic" => &self.anthropic_base,
            _ => &self.openai_base,
        }
    }
}

async fn handle(
    state: Arc<AppState>,
    provider: &'static dyn Provider,
    headers: HeaderMap,
    body: Value,
) -> Result<Response, ProxyError> {
    // Where is the text? A shape we do not recognize is refused, not forwarded.
    let pointers = provider.request_pointers(&body)?;

    // Detect and mask, one mapping for the whole request so a value keeps one name.
    let mut mapping = Mapping::new();
    let mut masked = body.clone();
    for pointer in &pointers {
        let text = read_pointer(&body, pointer)?;
        let spans = state.detector.detect(&text).await?;
        write_pointer(&mut masked, pointer, &mapping.mask(&text, &spans)?)?;
    }

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
        return Ok(crate::stream::restore_stream(
            response, provider, mapping, returned,
        ));
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
        return Ok(match serde_json::from_slice::<Value>(&raw) {
            Ok(parsed) => (status, returned, Json(mapping.restore_value(&parsed)?)).into_response(),
            Err(_) => {
                let text = String::from_utf8_lossy(&raw);
                (status, returned, mapping.restore(&text)?).into_response()
            }
        });
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
    Ok((returned, Json(restored)).into_response())
}

async fn openai(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match handle(state, &OpenAi, headers, body).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn anthropic(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match handle(state, &Anthropic, headers, body).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
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
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    fn state(detector: &MockServer, upstream: &MockServer) -> Arc<AppState> {
        Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5)),
            upstream: reqwest::Client::new(),
            openai_base: upstream.uri(),
            anthropic_base: upstream.uri(),
        })
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

    async fn upstream_streaming(body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
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
        Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5)),
            upstream: reqwest::Client::new(),
            openai_base: upstream_base.clone(),
            anthropic_base: upstream_base,
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
        let upstream = upstream_streaming(STREAM_BODY).await;

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
        let upstream = upstream_streaming("data: [DONE]\n\n").await;
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
        let upstream = upstream_streaming("data: [DONE]\n\n").await;
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
        let upstream = upstream_streaming(concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hallo [PERSON_9]\"}}]}\n\n",
            "data: [DONE]\n\n",
        ))
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
        let upstream = upstream_streaming(concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hallo [PERSON_1]\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"trunc\n\n",
        ))
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
                    .insert_header("content-type", "text/event-stream")
                    .insert_header("retry-after", "3")
                    .set_body_json(json!({"error": {"message": "slow down"}})),
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
}
