use std::time::Duration;

use serde::Deserialize;

use crate::mapping::Span;

#[derive(Debug, thiserror::Error)]
pub enum DetectorError {
    #[error("detector request failed: {0}")]
    Transport(String),
    #[error("detector returned status {0}")]
    Status(u16),
}

/// Every layer a complete run performs.
///
/// The gateway's own copy, for the same reason `mapping::ENTITY_TYPES` is one:
/// asking the detector which layers make a run complete would be worthless
/// against a detector that answers "the ones I ran". `scripts/check_layers.py`
/// fails CI when this list and the detector's `Layer` type disagree.
pub const LAYERS: [&str; 2] = ["deterministic", "ner"];

#[derive(Debug, Deserialize)]
struct DetectResponse {
    spans: Vec<Span>,
    #[serde(default)]
    layers_run: Vec<String>,
    #[serde(default)]
    version: Option<String>,
}

/// Spans, and whether they may be remembered.
///
/// `version` is `Some` only for a complete run from a detector that named the
/// weights and catalogs behind it. Everything else is served and forgotten, so
/// that a cache hit is never worse than a fresh call.
pub(crate) struct Detection {
    pub spans: Vec<Span>,
    pub version: Option<String>,
}

pub struct DetectorClient {
    base_url: String,
    client: reqwest::Client,
    cache: crate::detection_cache::DetectionCache,
}

impl DetectorClient {
    pub fn new(base_url: String, timeout: Duration, cache_entries: usize) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client builds with a timeout");
        Self {
            base_url,
            client,
            cache: crate::detection_cache::DetectionCache::new(cache_entries),
        }
    }

    /// Every layer the detector has: the gateway does not narrow detection.
    ///
    /// The credential is not used to authenticate anything — the gateway
    /// authenticates nobody — only to keep one tenant's cached results from
    /// answering another's request, which would report through response time
    /// that the two sent the same text.
    pub async fn detect(
        &self,
        text: &str,
        credential: Option<&[u8]>,
    ) -> Result<Vec<Span>, DetectorError> {
        if let Some(spans) = self.cache.get(credential, text) {
            return Ok(spans);
        }
        let detection = self.detect_full(text).await?;
        if let Some(version) = &detection.version {
            self.cache
                .insert(version, credential, text, &detection.spans);
        }
        Ok(detection.spans)
    }

    pub(crate) async fn detect_full(&self, text: &str) -> Result<Detection, DetectorError> {
        let response = self
            .client
            .post(format!("{}/detect", self.base_url))
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await
            // The error carries the transport failure, never the text.
            .map_err(|error| DetectorError::Transport(error.to_string()))?;
        if !response.status().is_success() {
            return Err(DetectorError::Status(response.status().as_u16()));
        }
        let parsed: DetectResponse = response
            .json()
            .await
            .map_err(|error| DetectorError::Transport(error.to_string()))?;
        let complete = LAYERS
            .iter()
            .all(|layer| parsed.layers_run.iter().any(|run| run == layer));
        let version = parsed
            .version
            .filter(|version| !version.is_empty())
            .filter(|_| complete);
        Ok(Detection {
            spans: parsed.spans,
            version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn spans_come_back_from_the_service() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [{"entity_type": "PERSON", "start": 0, "end": 5,
                           "confidence": 0.9, "recognizer": "ner:gliner", "tier": 2,
                           "boosted": false}],
                "layers_run": ["deterministic", "ner"]
            })))
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        let spans = client.detect("Weber schreibt", None).await.unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].entity_type, "PERSON");
    }

    #[tokio::test]
    async fn a_failing_detector_is_an_error_not_an_empty_result() {
        // An empty span list would look like "nothing to mask".
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(503).set_body_json(
                serde_json::json!({"detail": "layer(s) ner unavailable: no weights"}),
            ))
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        assert!(client.detect("Weber", None).await.is_err());
    }

    #[tokio::test]
    async fn a_slow_detector_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(300))
                    .set_body_json(serde_json::json!({"spans": [], "layers_run": []})),
            )
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_millis(50), 16);
        assert!(client.detect("Weber", None).await.is_err());
    }

    #[tokio::test]
    async fn the_submitted_text_is_sent_as_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"spans": [], "layers_run": []})),
            )
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        client.detect("Weber", None).await.unwrap();
        let sent = &server.received_requests().await.unwrap()[0];
        let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body["text"], "Weber");
        assert!(
            body.get("layers").is_none(),
            "the gateway asks for every layer"
        );
    }

    #[tokio::test]
    async fn a_complete_run_reports_a_version() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [],
                "layers_run": ["deterministic", "ner"],
                "version": "abc123"
            })))
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        let detection = client.detect_full("Weber").await.unwrap();
        assert_eq!(detection.version.as_deref(), Some("abc123"));
    }

    #[tokio::test]
    async fn a_partial_run_reports_no_version() {
        // Serving it is correct; remembering it is not. A deterministic-only
        // result cached while NER is down would be replayed after NER is back.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [],
                "layers_run": ["deterministic"],
                "version": "abc123"
            })))
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        let detection = client.detect_full("Weber").await.unwrap();
        assert!(detection.version.is_none());
    }

    #[tokio::test]
    async fn a_detector_that_reports_no_version_is_never_cacheable() {
        // An older detector predating the field. Keying everything it returns
        // under one empty version would be worse than not caching at all.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [],
                "layers_run": ["deterministic", "ner"]
            })))
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        let detection = client.detect_full("Weber").await.unwrap();
        assert!(detection.version.is_none());
    }

    #[tokio::test]
    async fn a_repeated_text_does_not_reach_the_detector_twice() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [], "layers_run": ["deterministic", "ner"], "version": "v1"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        let credential: Option<&[u8]> = Some(b"Bearer a");
        client.detect("Weber", credential).await.unwrap();
        client.detect("Weber", credential).await.unwrap();
        // `expect(1)` is asserted when the server drops.
    }

    #[tokio::test]
    async fn a_partial_run_is_asked_again_every_time() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [], "layers_run": ["deterministic"], "version": "v1"
            })))
            .expect(2)
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        let credential: Option<&[u8]> = Some(b"Bearer a");
        client.detect("Weber", credential).await.unwrap();
        client.detect("Weber", credential).await.unwrap();
    }

    #[tokio::test]
    async fn a_disabled_cache_asks_every_time() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [], "layers_run": ["deterministic", "ner"], "version": "v1"
            })))
            .expect(2)
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 0);
        let credential: Option<&[u8]> = Some(b"Bearer a");
        client.detect("Weber", credential).await.unwrap();
        client.detect("Weber", credential).await.unwrap();
    }

    #[tokio::test]
    async fn a_failing_detector_is_never_remembered() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(503).set_body_json(
                serde_json::json!({"detail": "layer(s) ner unavailable: no weights"}),
            ))
            .expect(2)
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        let credential: Option<&[u8]> = Some(b"Bearer a");
        assert!(client.detect("Weber", credential).await.is_err());
        assert!(client.detect("Weber", credential).await.is_err());
    }

    #[tokio::test]
    async fn another_credential_is_asked_again() {
        // `detection_cache.rs::another_credential_does_not_see_the_entry` proves
        // the store keys by credential; this proves `detect` actually hands it
        // the caller's credential on both the read and the write, rather than,
        // say, a normalized or truncated copy that collapses every tenant into
        // one bucket.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [], "layers_run": ["deterministic", "ner"], "version": "v1"
            })))
            .expect(2)
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        let a: Option<&[u8]> = Some(b"Bearer a");
        let b: Option<&[u8]> = Some(b"Bearer b");
        client.detect("Weber", a).await.unwrap();
        client.detect("Weber", b).await.unwrap();
    }

    #[tokio::test]
    async fn a_different_text_is_asked_again() {
        // The companion to `a_repeated_text_does_not_reach_the_detector_twice`:
        // that test would still pass if `detect` keyed the cache on something
        // constant instead of the text, because it only ever sends one text.
        // This one sends two, so a constant key would wrongly answer the
        // second from the first's entry — which `Mapping::mask` would then
        // apply to the wrong text.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [], "layers_run": ["deterministic", "ner"], "version": "v1"
            })))
            .expect(2)
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16);
        let credential: Option<&[u8]> = Some(b"Bearer a");
        client.detect("Weber", credential).await.unwrap();
        client.detect("Schmidt", credential).await.unwrap();
    }
}
