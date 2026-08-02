use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
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
            ProxyError::Shape(ShapeError::Request(_)) => StatusCode::BAD_REQUEST,
            _ => StatusCode::BAD_GATEWAY,
        };
        // The reason names the failure. It never carries the submitted text.
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

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
    provider: &dyn Provider,
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
        write_pointer(&mut masked, pointer, &mapping.mask(&text, &spans))?;
    }

    // Only what is masked leaves the process.
    let upstream: Value = state
        .upstream
        .post(format!(
            "{}{}",
            state.base_for(provider),
            provider.upstream_path()
        ))
        .json(&masked)
        .send()
        .await
        .map_err(|error| ProxyError::Upstream(error.to_string()))?
        .json()
        .await
        .map_err(|error| ProxyError::Upstream(error.to_string()))?;

    // Restore, and refuse rather than hand a placeholder to the client.
    let mut restored = upstream.clone();
    for pointer in provider.response_pointers(&upstream)? {
        let text = read_pointer(&upstream, &pointer)?;
        write_pointer(&mut restored, &pointer, &mapping.restore(&text)?)?;
    }
    Ok(Json(restored).into_response())
}

async fn openai(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    match handle(state, &OpenAi, body).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn anthropic(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    match handle(state, &Anthropic, body).await {
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
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(route)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
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
