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

#[derive(Debug, Deserialize)]
struct DetectResponse {
    spans: Vec<Span>,
}

pub struct DetectorClient {
    base_url: String,
    client: reqwest::Client,
}

impl DetectorClient {
    pub fn new(base_url: String, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client builds with a timeout");
        Self { base_url, client }
    }

    /// Every layer the detector has: the gateway does not narrow detection.
    pub async fn detect(&self, text: &str) -> Result<Vec<Span>, DetectorError> {
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
        Ok(parsed.spans)
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5));
        let spans = client.detect("Weber schreibt").await.unwrap();
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5));
        assert!(client.detect("Weber").await.is_err());
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
        let client = DetectorClient::new(server.uri(), Duration::from_millis(50));
        assert!(client.detect("Weber").await.is_err());
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5));
        client.detect("Weber").await.unwrap();
        let sent = &server.received_requests().await.unwrap()[0];
        let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body["text"], "Weber");
        assert!(
            body.get("layers").is_none(),
            "the gateway asks for every layer"
        );
    }
}
