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
    pub fn new(
        base_url: String,
        timeout: Duration,
        cache_entries: usize,
        max_spans_per_entry: usize,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client builds with a timeout");
        Self {
            base_url,
            client,
            cache: crate::detection_cache::DetectionCache::new(cache_entries, max_spans_per_entry),
        }
    }

    /// Every layer the detector has: the gateway does not narrow detection.
    ///
    /// The credential is not used to authenticate anything — the gateway
    /// authenticates nobody — only to keep one tenant's cached results from
    /// answering another's request, which would report through response time
    /// that the two sent the same text.
    /// Whether `detect` would answer this text without a call.
    ///
    /// **The admission bounds ask this, and they ask it of the same cache the
    /// call will consult**, so a text charged as work is a text that will cost
    /// work. `max_tool_chars` bounds how long a caller waits for the whole
    /// request — its own documentation says so, and says it is not a timeout
    /// budget — and a text the cache answers costs no wait at all.
    ///
    /// A hit here does not promise a hit later: an eviction between this and
    /// `detect` turns a free text into a paid one, and the request is served
    /// anyway. That direction is the safe one — the worst case is a request
    /// that waits longer than the bound predicted, which is what every request
    /// did before this existed. The other direction, a text charged and then
    /// answered free, only under-uses the budget.
    ///
    /// Deliberately not `&mut self` and deliberately not refreshing the LRU
    /// clock: this is a question about cost, not a use of the entry, and
    /// promoting an entry because a request *asked about* it would let the
    /// bounds decide what stays cached.
    pub fn would_be_cached(&self, text: &str, credential: Option<&[u8]>) -> bool {
        self.cache.contains(credential, text)
    }

    pub async fn detect(
        &self,
        text: &str,
        credential: Option<&[u8]>,
    ) -> Result<Vec<Span>, DetectorError> {
        if let Some(spans) = self.cache.get(credential, text) {
            return Ok(spans);
        }
        let detection = self.detect_full(text).await?;
        // A malformed complete response — empty, inverted, out of range or
        // overlapping spans — must be served (the caller already has what
        // the detector returned) but never remembered: `crate::mapping::mask`
        // would refuse every future request answered from this entry, and a
        // cache hit refreshes the LRU clock, so one transient bad response
        // would become a permanent per-text refusal rather than a single
        // failed request. `check_spans` asks the exact question `mask` will
        // ask later, over the same text and spans, so this can only decline
        // to cache what `mask` would actually reject — never more, never
        // less.
        if let Some(version) = &detection.version {
            if crate::mapping::check_spans(text, &detection.spans).is_ok() {
                self.cache
                    .insert(version, credential, text, &detection.spans);
            }
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

    /// The span cap tests that are not about the cap itself pass this, so a
    /// handful of spans never accidentally brushes against it.
    const UNCAPPED: usize = usize::MAX;

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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
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
        let client = DetectorClient::new(server.uri(), Duration::from_millis(50), 16, UNCAPPED);
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 0, UNCAPPED);
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
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
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
        let credential: Option<&[u8]> = Some(b"Bearer a");
        client.detect("Weber", credential).await.unwrap();
        client.detect("Schmidt", credential).await.unwrap();
    }

    #[tokio::test]
    async fn an_oversized_detection_is_served_in_full_but_never_cached() {
        // Declining to cache must never mean declining to serve: the caller
        // already has the spans this call found and gets every one of them
        // back, whether or not the cache remembers them for next time. A
        // test that only checked "not cached" would pass just as well
        // against a broken implementation that refused the request instead
        // of serving it — this checks both halves in one place.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [
                    {"entity_type": "PERSON", "start": 0, "end": 5, "confidence": 1.0,
                     "recognizer": "ner:fake", "tier": 2, "boosted": false},
                    {"entity_type": "PERSON", "start": 6, "end": 11, "confidence": 1.0,
                     "recognizer": "ner:fake", "tier": 2, "boosted": false},
                    {"entity_type": "PERSON", "start": 12, "end": 19, "confidence": 1.0,
                     "recognizer": "ner:fake", "tier": 2, "boosted": false}
                ],
                "layers_run": ["deterministic", "ner"], "version": "v1"
            })))
            .expect(2)
            .mount(&server)
            .await;
        // Three spans, capped at two: this detection is over the limit.
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, 2);
        let credential: Option<&[u8]> = Some(b"Bearer a");

        let first = client
            .detect("Weber Meier Schmidt", credential)
            .await
            .unwrap();
        assert_eq!(
            first.len(),
            3,
            "an oversized detection was truncated rather than served in full"
        );

        let second = client
            .detect("Weber Meier Schmidt", credential)
            .await
            .unwrap();
        assert_eq!(second.len(), 3);
        // `expect(2)` is asserted when `server` drops: a cached hit would
        // have answered the second call without reaching it at all.
    }

    #[tokio::test]
    async fn a_malformed_complete_response_is_not_cached() {
        // `Mapping::mask` (mapping.rs) refuses overlapping, empty/inverted or
        // out-of-range spans. Before `check_spans` gated `insert`, `detect`
        // cached a malformed complete response anyway: every later request
        // for the same tenant and text answered from that entry, `mask`
        // refused it again, and the hit refreshed the LRU clock so the
        // entry was never evicted — one transient bad response became a
        // permanent per-text refusal. The overlapping pair here
        // (PERSON 0..5, IBAN 3..8 over "Weber schreibt") is the same shape
        // `mapping::tests::an_overlapping_span_refuses` uses, so the two
        // tests exercise one shared predicate rather than two that could
        // disagree.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [
                    {"entity_type": "PERSON", "start": 0, "end": 5, "confidence": 1.0,
                     "recognizer": "ner:fake", "tier": 2, "boosted": false},
                    {"entity_type": "IBAN", "start": 3, "end": 8, "confidence": 1.0,
                     "recognizer": "ner:fake", "tier": 1, "boosted": false}
                ],
                "layers_run": ["deterministic", "ner"], "version": "v1"
            })))
            .expect(2)
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
        let credential: Option<&[u8]> = Some(b"Bearer a");

        let first = client.detect("Weber schreibt", credential).await.unwrap();
        assert_eq!(
            first.len(),
            2,
            "a malformed detection is still served in full"
        );

        let second = client.detect("Weber schreibt", credential).await.unwrap();
        assert_eq!(second.len(), 2);
        // `expect(2)` is asserted when `server` drops: a cached hit — good
        // or bad — would have answered the second call without reaching it.
    }

    #[tokio::test]
    async fn a_well_formed_complete_response_still_caches() {
        // The mirror of `a_malformed_complete_response_is_not_cached`: the
        // new guard must decline exactly what `mask` would reject, not
        // caching in general. Same text, spans that do not overlap.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "spans": [
                    {"entity_type": "PERSON", "start": 0, "end": 5, "confidence": 1.0,
                     "recognizer": "ner:fake", "tier": 2, "boosted": false},
                    {"entity_type": "IBAN", "start": 6, "end": 14, "confidence": 1.0,
                     "recognizer": "ner:fake", "tier": 1, "boosted": false}
                ],
                "layers_run": ["deterministic", "ner"], "version": "v1"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = DetectorClient::new(server.uri(), Duration::from_secs(5), 16, UNCAPPED);
        let credential: Option<&[u8]> = Some(b"Bearer a");

        client.detect("Weber schreibt", credential).await.unwrap();
        client.detect("Weber schreibt", credential).await.unwrap();
        // `expect(1)` is asserted when `server` drops: a cache hit answered
        // the second call.
    }
}
