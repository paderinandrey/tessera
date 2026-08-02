# Gateway Skeleton Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** a Rust reverse proxy that masks personal data in OpenAI- and Anthropic-shaped requests, restores it in responses, and refuses the request whenever it cannot do either.

**Architecture:** Providers describe *where* text lives as a list of JSON pointers; everything else — masking, forwarding, restoring — is written once against those pointers. That keeps the two body shapes from leaking into the proxy and makes both testable without HTTP.

**Tech Stack:** axum on tokio, reqwest upstream, serde_json for body rewriting, wiremock for tests.

**Spec:** `docs/superpowers/specs/2026-08-03-gateway-skeleton-design.md`

## Global Constraints

- **Fail closed.** A detector error, a detector timeout, a body that does not parse into the expected shape, or a placeholder in the response that is not in the mapping — each refuses the request. Nothing unmasked is ever forwarded, and no placeholder is ever handed to the client in place of a value.
- **No original text in any response body or log line, at any level.** Errors carry reasons.
- The detector is asked for every layer it has; the timeout is configuration, default 30 seconds.
- An identical value always gets the same placeholder within a request.
- `cargo fmt --check`, `cargo clippy -- -D warnings` and `cargo test` must all pass; clippy warnings are errors.
- Commit message style: one-line `Gateway: <what>` with the trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- Run from `gateway/`: `cargo test`.

---

### Task 1: Crate, configuration and CI

**Files:**
- Create: `gateway/Cargo.toml`, `gateway/src/main.rs`, `gateway/src/config.rs`
- Create: `gateway/tessera.example.toml`
- Modify: `.github/workflows/ci.yml`, `.gitignore`, `Makefile`

**Interfaces:**
- Produces: `Config { bind: String, detector_url: String, detector_timeout_secs: u64, openai_base: String, anthropic_base: String }` with `Config::from_toml(&str) -> Result<Config, ConfigError>` and defaults. Tasks 4–5 consume it.

- [ ] **Step 1: Create the crate**

```toml
# gateway/Cargo.toml
[package]
name = "tessera-gateway"
version = "0.1.0"
edition = "2024"
license = "Apache-2.0"

[dependencies]
axum = "0.9"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net"] }
reqwest = { version = "0.13", features = ["json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.9"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = "0.3"

[dev-dependencies]
wiremock = "0.7"
```

If a version does not resolve, take the current major from `cargo add` rather than pinning something older, and record what you used in your report.

- [ ] **Step 2: Write the failing config tests**

```rust
// gateway/src/config.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_usable_without_a_file() {
        let config = Config::from_toml("").expect("empty config is valid");
        assert_eq!(config.bind, "127.0.0.1:8080");
        assert_eq!(config.detector_url, "http://127.0.0.1:8000");
        assert_eq!(config.detector_timeout_secs, 30);
    }

    #[test]
    fn values_override_the_defaults() {
        let config = Config::from_toml(
            r#"
            bind = "0.0.0.0:9090"
            detector_url = "http://detector:8000"
            detector_timeout_secs = 5
            openai_base = "https://api.openai.com"
            "#,
        )
        .expect("valid config");
        assert_eq!(config.bind, "0.0.0.0:9090");
        assert_eq!(config.detector_timeout_secs, 5);
        assert_eq!(config.openai_base, "https://api.openai.com");
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        // A typo in a security control's configuration must not be silently ignored.
        let error = Config::from_toml("detector_timeoutt_secs = 5").unwrap_err();
        assert!(error.to_string().contains("detector_timeoutt_secs"));
    }

    #[test]
    fn a_zero_timeout_is_rejected() {
        assert!(Config::from_toml("detector_timeout_secs = 0").is_err());
    }
}
```

- [ ] **Step 3: Run them to verify they fail**

Run: `cd gateway && cargo test`
Expected: compilation failure — `Config` does not exist.

- [ ] **Step 4: Write the configuration**

```rust
// gateway/src/config.rs
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("detector_timeout_secs must be greater than zero")]
    ZeroTimeout,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_detector_url")]
    pub detector_url: String,
    /// Generous on purpose: full detection costs about a second per 1 200
    /// characters, and a conversation history is longer than that. Exceeding
    /// it refuses the request — it never forwards unmasked text.
    #[serde(default = "default_timeout")]
    pub detector_timeout_secs: u64,
    #[serde(default = "default_openai_base")]
    pub openai_base: String,
    #[serde(default = "default_anthropic_base")]
    pub anthropic_base: String,
}

fn default_bind() -> String {
    "127.0.0.1:8080".to_owned()
}
fn default_detector_url() -> String {
    "http://127.0.0.1:8000".to_owned()
}
fn default_timeout() -> u64 {
    30
}
fn default_openai_base() -> String {
    "https://api.openai.com".to_owned()
}
fn default_anthropic_base() -> String {
    "https://api.anthropic.com".to_owned()
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text)?;
        if config.detector_timeout_secs == 0 {
            return Err(ConfigError::ZeroTimeout);
        }
        Ok(config)
    }
}
```

`deny_unknown_fields` is deliberate: a typo in the configuration of a security control must fail loudly rather than leave a default in place.

- [ ] **Step 5: A minimal binary and the example config**

```rust
// gateway/src/main.rs
mod config;

fn main() {
    println!("tessera-gateway");
}
```

```toml
# gateway/tessera.example.toml
bind = "127.0.0.1:8080"
detector_url = "http://127.0.0.1:8000"
detector_timeout_secs = 30
openai_base = "https://api.openai.com"
anthropic_base = "https://api.anthropic.com"
```

- [ ] **Step 6: Ignore build artefacts and add the make targets**

Add `gateway/target/` to `.gitignore`. Add to the `Makefile` (`.PHONY` too):

```make
gateway-test:
	cd gateway && cargo test

gateway-lint:
	cd gateway && cargo fmt --check && cargo clippy -- -D warnings
```

- [ ] **Step 7: Add the CI job**

In `.github/workflows/ci.yml`:

```yaml
  gateway:
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: gateway
    steps:
      - uses: actions/checkout@v7
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - run: cargo fmt --check
      - run: cargo clippy -- -D warnings
      - run: cargo test
```

- [ ] **Step 8: Verify and commit**

Run: `cd gateway && cargo test && cargo fmt --check && cargo clippy -- -D warnings`

```bash
git add gateway .gitignore Makefile .github/workflows/ci.yml
git commit -m "Gateway: crate, configuration that rejects typos, CI job

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: Placeholders and restoration

**Files:**
- Create: `gateway/src/mapping.rs`
- Modify: `gateway/src/main.rs` (declare the module)

**Interfaces:**
- Produces: `Span { entity_type: String, start: usize, end: usize }` (deserialized from the detector), `Mapping::new()`, `Mapping::mask(&mut self, text: &str, spans: &[Span]) -> String`, `Mapping::restore(&self, text: &str) -> Result<String, MappingError>`. Task 5 uses all three.

- [ ] **Step 1: Write the failing tests**

```rust
// gateway/src/mapping.rs
#[cfg(test)]
mod tests {
    use super::*;

    fn span(entity_type: &str, start: usize, end: usize) -> Span {
        Span { entity_type: entity_type.to_owned(), start, end }
    }

    #[test]
    fn masking_replaces_a_span_with_a_typed_placeholder() {
        let mut mapping = Mapping::new();
        let masked = mapping.mask("Herr Weber schreibt", &[span("PERSON", 5, 10)]);
        assert_eq!(masked, "Herr [PERSON_1] schreibt");
    }

    #[test]
    fn later_spans_do_not_shift_earlier_ones() {
        // Replacing left to right would invalidate every offset after the first.
        let mut mapping = Mapping::new();
        let masked = mapping.mask(
            "Weber und Schmidt",
            &[span("PERSON", 0, 5), span("PERSON", 10, 17)],
        );
        assert_eq!(masked, "[PERSON_1] und [PERSON_2]");
    }

    #[test]
    fn the_same_value_keeps_the_same_placeholder() {
        // Two placeholders for one person would tell the model there are two.
        let mut mapping = Mapping::new();
        let masked = mapping.mask(
            "Weber schrieb an Weber",
            &[span("PERSON", 0, 5), span("PERSON", 17, 22)],
        );
        assert_eq!(masked, "[PERSON_1] schrieb an [PERSON_1]");
    }

    #[test]
    fn numbering_continues_across_calls() {
        // One request carries several texts; they share a mapping.
        let mut mapping = Mapping::new();
        mapping.mask("Weber", &[span("PERSON", 0, 5)]);
        let second = mapping.mask("Schmidt", &[span("PERSON", 0, 7)]);
        assert_eq!(second, "[PERSON_2]");
    }

    #[test]
    fn restoring_puts_the_values_back() {
        let mut mapping = Mapping::new();
        mapping.mask("Weber", &[span("PERSON", 0, 5)]);
        assert_eq!(mapping.restore("Hallo [PERSON_1]!").unwrap(), "Hallo Weber!");
    }

    #[test]
    fn an_unknown_placeholder_breaks_the_request() {
        // A lost mapping must not hand "[PERSON_9]" to the client in place of
        // a name: the request fails instead.
        let mapping = Mapping::new();
        let error = mapping.restore("Hallo [PERSON_9]!").unwrap_err();
        assert!(matches!(error, MappingError::Unknown(_)));
    }

    #[test]
    fn text_without_placeholders_passes_through() {
        assert_eq!(Mapping::new().restore("nothing here").unwrap(), "nothing here");
    }

    #[test]
    fn masking_is_offset_correct_on_multibyte_text() {
        // The detector counts characters; Rust slices bytes.
        let mut mapping = Mapping::new();
        let masked = mapping.mask("Grüße an Weber", &[span("PERSON", 9, 14)]);
        assert_eq!(masked, "Grüße an [PERSON_1]");
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd gateway && cargo test`
Expected: compilation failure — `Mapping` does not exist.

- [ ] **Step 3: Write the implementation**

```rust
// gateway/src/mapping.rs
use std::collections::HashMap;

use serde::Deserialize;

/// A span as the detector reports it: offsets are in characters, not bytes.
#[derive(Debug, Clone, Deserialize)]
pub struct Span {
    pub entity_type: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MappingError {
    #[error("no mapping for placeholder {0}; the request is refused rather than served with it")]
    Unknown(String),
}

#[derive(Debug, Default)]
pub struct Mapping {
    by_value: HashMap<String, String>,
    by_placeholder: HashMap<String, String>,
    next: usize,
}

impl Mapping {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_placeholder.is_empty()
    }

    pub fn mask(&mut self, text: &str, spans: &[Span]) -> String {
        // Character indices, because that is what the detector reports.
        let chars: Vec<char> = text.chars().collect();
        let mut ordered: Vec<&Span> = spans.iter().collect();
        ordered.sort_by_key(|span| span.start);

        let mut result = String::with_capacity(text.len());
        let mut cursor = 0usize;
        for span in ordered {
            if span.start < cursor || span.end > chars.len() || span.start >= span.end {
                // Overlapping or out-of-range spans are the detector's job to
                // resolve; skipping is safer than producing torn text.
                continue;
            }
            result.extend(&chars[cursor..span.start]);
            let value: String = chars[span.start..span.end].iter().collect();
            result.push_str(&self.placeholder_for(&span.entity_type, value));
            cursor = span.end;
        }
        result.extend(&chars[cursor..]);
        result
    }

    fn placeholder_for(&mut self, entity_type: &str, value: String) -> String {
        if let Some(existing) = self.by_value.get(&value) {
            return existing.clone();
        }
        self.next += 1;
        let placeholder = format!("[{entity_type}_{}]", self.next);
        self.by_value.insert(value.clone(), placeholder.clone());
        self.by_placeholder.insert(placeholder.clone(), value);
        placeholder
    }

    pub fn restore(&self, text: &str) -> Result<String, MappingError> {
        let mut result = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(open) = rest.find('[') {
            let (before, from_open) = rest.split_at(open);
            result.push_str(before);
            let Some(close) = from_open.find(']') else {
                result.push_str(from_open);
                return Ok(result);
            };
            let candidate = &from_open[..=close];
            if is_placeholder(candidate) {
                let value = self
                    .by_placeholder
                    .get(candidate)
                    .ok_or_else(|| MappingError::Unknown(candidate.to_owned()))?;
                result.push_str(value);
            } else {
                result.push_str(candidate);
            }
            rest = &from_open[close + 1..];
        }
        result.push_str(rest);
        Ok(result)
    }
}

/// `[TYPE_N]`: upper-case type, underscore, digits.
fn is_placeholder(candidate: &str) -> bool {
    let inner = candidate.trim_start_matches('[').trim_end_matches(']');
    let Some((entity_type, number)) = inner.rsplit_once('_') else {
        return false;
    };
    !entity_type.is_empty()
        && entity_type.chars().all(|c| c.is_ascii_uppercase() || c == '_')
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}
```

- [ ] **Step 4: Run them to verify they pass**

Run: `cd gateway && cargo test && cargo clippy -- -D warnings && cargo fmt`

- [ ] **Step 5: Commit**

```bash
git add gateway/src
git commit -m "Gateway: placeholders keep one value one name, lost mapping refuses

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: Provider shapes as JSON pointers

**Files:**
- Create: `gateway/src/provider.rs`
- Modify: `gateway/src/main.rs`

**Interfaces:**
- Produces: `Provider` trait with `request_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError>` and `response_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError>`, plus `upstream_path(&self) -> &'static str`; `OpenAi` and `Anthropic` implementations; `read_pointer` / `write_pointer` helpers. Task 5 consumes them.

**Why pointers:** describing *where* text lives, rather than extracting and re-injecting it, means masking and restoration are written once and both providers are pure data. It also makes the shape mismatch — the fail-closed case — a single check.

- [ ] **Step 1: Write the failing tests**

```rust
// gateway/src/provider.rs
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_finds_string_content() {
        let body = json!({"messages": [{"role": "user", "content": "Weber"}]});
        assert_eq!(OpenAi.request_pointers(&body).unwrap(), vec!["/messages/0/content"]);
    }

    #[test]
    fn openai_finds_text_parts() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "Weber"},
            {"type": "image_url", "image_url": {"url": "http://x"}}
        ]}]});
        assert_eq!(OpenAi.request_pointers(&body).unwrap(), vec!["/messages/0/content/0/text"]);
    }

    #[test]
    fn openai_reads_the_response_content() {
        let body = json!({"choices": [{"message": {"role": "assistant", "content": "Hallo"}}]});
        assert_eq!(
            OpenAi.response_pointers(&body).unwrap(),
            vec!["/choices/0/message/content"]
        );
    }

    #[test]
    fn anthropic_finds_the_system_field_and_the_messages() {
        let body = json!({
            "system": "Du bist hilfreich",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Weber"}]}]
        });
        assert_eq!(
            Anthropic.request_pointers(&body).unwrap(),
            vec!["/system", "/messages/0/content/0/text"]
        );
    }

    #[test]
    fn anthropic_reads_the_response_blocks() {
        let body = json!({"content": [{"type": "text", "text": "Hallo"}]});
        assert_eq!(Anthropic.response_pointers(&body).unwrap(), vec!["/content/0/text"]);
    }

    #[test]
    fn a_body_without_messages_is_a_shape_error() {
        // Fail closed: an unparsed body must not be forwarded unmasked.
        assert!(OpenAi.request_pointers(&json!({"model": "gpt"})).is_err());
        assert!(Anthropic.request_pointers(&json!({"model": "claude"})).is_err());
    }

    #[test]
    fn pointers_round_trip_through_read_and_write() {
        let mut body = json!({"messages": [{"content": "Weber"}]});
        assert_eq!(read_pointer(&body, "/messages/0/content").unwrap(), "Weber");
        write_pointer(&mut body, "/messages/0/content", "[PERSON_1]").unwrap();
        assert_eq!(body["messages"][0]["content"], "[PERSON_1]");
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd gateway && cargo test`
Expected: compilation failure.

- [ ] **Step 3: Write the implementation**

```rust
// gateway/src/provider.rs
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum ShapeError {
    #[error("request body is not in the expected {0} shape")]
    Request(&'static str),
    #[error("upstream response is not in the expected {0} shape")]
    Response(&'static str),
    #[error("no value at {0}")]
    Pointer(String),
}

/// Where the text lives. Providers describe locations; masking and restoration
/// are written once against them, so a new shape adds no new rewriting code.
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn upstream_path(&self) -> &'static str;
    fn request_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError>;
    fn response_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError>;
}

pub struct OpenAi;
pub struct Anthropic;

fn content_pointers(prefix: &str, content: &Value, out: &mut Vec<String>) {
    match content {
        Value::String(_) => out.push(prefix.to_owned()),
        Value::Array(parts) => {
            for (index, part) in parts.iter().enumerate() {
                if part.get("text").and_then(Value::as_str).is_some() {
                    out.push(format!("{prefix}/{index}/text"));
                }
            }
        }
        _ => {}
    }
}

impl Provider for OpenAi {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn upstream_path(&self) -> &'static str {
        "/v1/chat/completions"
    }

    fn request_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError> {
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .ok_or(ShapeError::Request("openai"))?;
        let mut pointers = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            if let Some(content) = message.get("content") {
                content_pointers(&format!("/messages/{index}/content"), content, &mut pointers);
            }
        }
        Ok(pointers)
    }

    fn response_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError> {
        let choices = body
            .get("choices")
            .and_then(Value::as_array)
            .ok_or(ShapeError::Response("openai"))?;
        let mut pointers = Vec::new();
        for (index, choice) in choices.iter().enumerate() {
            if let Some(content) = choice.pointer("/message/content") {
                content_pointers(
                    &format!("/choices/{index}/message/content"),
                    content,
                    &mut pointers,
                );
            }
        }
        Ok(pointers)
    }
}

impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn upstream_path(&self) -> &'static str {
        "/v1/messages"
    }

    fn request_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError> {
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .ok_or(ShapeError::Request("anthropic"))?;
        let mut pointers = Vec::new();
        if let Some(system) = body.get("system") {
            content_pointers("/system", system, &mut pointers);
        }
        for (index, message) in messages.iter().enumerate() {
            if let Some(content) = message.get("content") {
                content_pointers(&format!("/messages/{index}/content"), content, &mut pointers);
            }
        }
        Ok(pointers)
    }

    fn response_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError> {
        let blocks = body
            .get("content")
            .and_then(Value::as_array)
            .ok_or(ShapeError::Response("anthropic"))?;
        let mut pointers = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            if block.get("text").and_then(Value::as_str).is_some() {
                pointers.push(format!("/content/{index}/text"));
            }
        }
        Ok(pointers)
    }
}

pub fn read_pointer(body: &Value, pointer: &str) -> Result<String, ShapeError> {
    body.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ShapeError::Pointer(pointer.to_owned()))
}

pub fn write_pointer(body: &mut Value, pointer: &str, text: &str) -> Result<(), ShapeError> {
    let slot = body
        .pointer_mut(pointer)
        .ok_or_else(|| ShapeError::Pointer(pointer.to_owned()))?;
    *slot = Value::String(text.to_owned());
    Ok(())
}
```

- [ ] **Step 4: Verify and commit**

Run: `cd gateway && cargo test && cargo clippy -- -D warnings && cargo fmt`

```bash
git add gateway/src
git commit -m "Gateway: providers describe where text lives, not how to rewrite it

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: The detector client

**Files:**
- Create: `gateway/src/detector.rs`
- Modify: `gateway/src/main.rs`, `gateway/Cargo.toml` (dev-dependency `tokio` with `macros`, `rt-multi-thread` is already there)

**Interfaces:**
- Consumes: `Span` from `mapping`.
- Produces: `DetectorClient::new(base_url: String, timeout: Duration)`, `async fn detect(&self, text: &str) -> Result<Vec<Span>, DetectorError>`.

- [ ] **Step 1: Write the failing tests**

```rust
// gateway/src/detector.rs
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
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cd gateway && cargo test`

- [ ] **Step 3: Write the client**

```rust
// gateway/src/detector.rs
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

    pub async fn detect(&self, text: &str) -> Result<Vec<Span>, DetectorError> {
        let response = self
            .client
            .post(format!("{}/detect", self.base_url))
            .json(&serde_json::json!({ "text": text }))
            // The error carries the transport failure, never the text.
            .send()
            .await
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
```

Note the omitted `layers` field: the gateway asks for everything the detector has, per the design.

- [ ] **Step 4: Verify and commit**

Run: `cd gateway && cargo test && cargo clippy -- -D warnings && cargo fmt`

```bash
git add gateway
git commit -m "Gateway: detector client, failure and timeout are errors not silence

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: The proxy handler

**Files:**
- Create: `gateway/src/proxy.rs`
- Modify: `gateway/src/main.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–4.
- Produces: `AppState { detector: DetectorClient, upstream: reqwest::Client, config: Config }`, `pub fn router(state: Arc<AppState>) -> axum::Router`, handlers for both provider paths.

- [ ] **Step 1: Write the failing tests**

```rust
// gateway/src/proxy.rs
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SECRET: &str = "Weber";

    async fn detector_returning(spans: serde_json::Value) -> MockServer {
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

    #[tokio::test]
    async fn the_upstream_never_sees_the_original() {
        let detector = detector_returning(
            json!([{"entity_type": "PERSON", "start": 0, "end": 5, "confidence": 1.0,
                    "recognizer": "ner:fake", "tier": 2, "boosted": false}]),
        )
        .await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"role": "assistant", "content": "Hallo [PERSON_1]"}}]
            })))
            .mount(&upstream)
            .await;

        let response = call_openai(&detector, &upstream, json!({
            "model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]
        }))
        .await;

        assert_eq!(response.status(), 200);
        let received = &upstream.received_requests().await.unwrap()[0];
        let sent = String::from_utf8(received.body.clone()).unwrap();
        assert!(!sent.contains(SECRET), "the original reached the upstream: {sent}");
        assert!(sent.contains("[PERSON_1]"));
    }

    #[tokio::test]
    async fn the_client_gets_the_original_back() {
        // ... same setup; assert the response body contains "Weber" and no placeholder
    }

    #[tokio::test]
    async fn a_detector_failure_refuses_the_request() {
        // The upstream mock records nothing: fail closed means never forwarded.
    }

    #[tokio::test]
    async fn an_unparsable_body_refuses_the_request() {
        // {"model": "gpt"} with no messages: 400, and the upstream saw nothing.
    }

    #[tokio::test]
    async fn a_lost_mapping_refuses_the_response() {
        // The upstream returns "[PERSON_9]", which no mapping knows: the client
        // gets an error rather than a placeholder in place of a name.
    }

    #[tokio::test]
    async fn errors_never_carry_the_original_text() {
        // Force a detector failure with the secret in the body; assert the
        // error body contains neither the secret nor any part of it.
    }

    #[tokio::test]
    async fn the_anthropic_shape_is_masked_too() {
        // system + messages content blocks; assert both were masked upstream.
    }
}
```

Fill each stub in fully as you implement — every one names a rule from the spec, and a stub that stays a comment is a rule nobody checks.

- [ ] **Step 2: Run them to verify they fail**

Run: `cd gateway && cargo test`

- [ ] **Step 3: Write the handler**

The shape, with the fail-closed rule visible in the flow:

```rust
async fn handle(
    state: Arc<AppState>,
    provider: &dyn Provider,
    body: Value,
) -> Result<Response, ProxyError> {
    // 1. Where is the text? A shape we do not recognize is refused, not forwarded.
    let pointers = provider.request_pointers(&body)?;

    // 2. Detect and mask, one mapping for the whole request.
    let mut mapping = Mapping::new();
    let mut masked = body.clone();
    for pointer in &pointers {
        let text = read_pointer(&body, pointer)?;
        let spans = state.detector.detect(&text).await?;
        write_pointer(&mut masked, pointer, &mapping.mask(&text, &spans))?;
    }

    // 3. Forward only what is masked.
    let upstream = state.forward(provider, &masked).await?;

    // 4. Restore, and refuse rather than hand a placeholder to the client.
    let mut restored = upstream.clone();
    for pointer in provider.response_pointers(&upstream)? {
        let text = read_pointer(&upstream, &pointer)?;
        write_pointer(&mut restored, &pointer, &mapping.restore(&text)?)?;
    }
    Ok(Json(restored).into_response())
}
```

`ProxyError` implements `IntoResponse`, mapping a shape error to 400, a detector error to 502, and a mapping loss to 502. Its body is `{"error": "<reason>"}` — the reason names the failure, never the text.

- [ ] **Step 4: Wire the router and the binary**

`router()` mounts `POST /v1/chat/completions` and `POST /v1/messages`. `main` reads the config file named by the first argument (or the defaults), builds the state, and serves.

- [ ] **Step 5: Verify and commit**

Run: `cd gateway && cargo test && cargo clippy -- -D warnings && cargo fmt --check`

```bash
git add gateway
git commit -m "Gateway: proxy handler, fail closed at every step

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Documentation

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update the architecture block**

`gateway/` is no longer "planned": describe it as the reverse proxy, non-streaming for now.

- [ ] **Step 2: Add a Gateway section**

Cover: what it does, how to point a client at it, the configuration file, and — plainly — that a detector failure, an unrecognized body or a lost mapping refuses the request rather than forwarding or degrading. Note that the full detection layer runs by default and what that costs, linking the latency section.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "Gateway: document the proxy and its fail-closed behaviour

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## After the plan

Push `feat/gateway-skeleton`, open a PR to `main`, comment `@codex review`, and keep the fix → tag → wait loop going until Codex reviews the current HEAD with no findings attached — as an issue comment saying `Didn't find any major issues`, or as a review on the current commit carrying no inline comments.
