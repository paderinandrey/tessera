# SSE Streaming Restoration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore placeholders in an SSE response stream so the gateway can serve `stream: true` without ever handing a placeholder to the client.

**Architecture:** A new `gateway/src/stream.rs` holds three units — `RestoreBuffer` (pure hold-back and restoration), `SseFramer` (byte-level event framing), and the response pipeline that joins them. The `Provider` trait gains `stream_pointers` so event shapes are described in one place, as request and response shapes already are.

**Tech Stack:** Rust, axum 0.7, tokio, reqwest 0.12 (streaming body), futures-util (stream adapters), serde_json, wiremock 0.6.

## Global Constraints

- An original value must never appear in a log line or an error body, at any level.
- Silence is a leak: a shape that is recognized but unhandled refuses; it never falls through.
- A placeholder must never reach the client in place of a value.
- Placeholder matching is exact against `[TYPE_N]` — upper-case type, underscore, digits. No tolerance for spacing, casing or wrappers.
- `MAX_HELD = 64` characters is the hold-back cap.
- Spec: `docs/superpowers/specs/2026-08-07-sse-streaming-design.md`.

---

### Task 1: `RestoreBuffer`

**Files:**
- Create: `gateway/src/stream.rs`
- Modify: `gateway/src/main.rs` (add `mod stream;`)

**Interfaces:**
- Consumes: `crate::mapping::{Mapping, MappingError}`.
- Produces: `RestoreBuffer::new(&Mapping)`, `push(&mut self, &str) -> Result<String, MappingError>`, `finish(&mut self) -> Result<String, MappingError>`, `const MAX_HELD: usize = 64`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod buffer_tests {
    use super::*;
    use crate::mapping::{Mapping, Span};

    fn mapped() -> Mapping {
        let mut mapping = Mapping::new();
        mapping
            .mask(
                "Weber",
                &[Span { entity_type: "PERSON".into(), start: 0, end: 5 }],
            )
            .unwrap();
        mapping
    }

    #[test]
    fn a_placeholder_split_across_pushes_is_restored_once() {
        let mapping = mapped();
        let mut buffer = RestoreBuffer::new(&mapping);
        let mut out = String::new();
        out.push_str(&buffer.push("Hallo [PER").unwrap());
        out.push_str(&buffer.push("SON_1]!").unwrap());
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "Hallo Weber!");
    }

    #[test]
    fn nothing_before_a_complete_token_is_withheld() {
        // Only the tail from the last unclosed '[' is held; earlier text flows.
        let mapping = mapped();
        let mut buffer = RestoreBuffer::new(&mapping);
        assert_eq!(buffer.push("plenty of text [PER").unwrap(), "plenty of text ");
    }

    #[test]
    fn an_unclosed_bracket_past_the_cap_stops_holding() {
        // "[note" followed by prose must not suspend the stream forever.
        let mapping = mapped();
        let mut buffer = RestoreBuffer::new(&mapping);
        let long = "x".repeat(MAX_HELD + 10);
        let emitted = buffer.push(&format!("[note {long}")).unwrap();
        assert!(emitted.starts_with("[note "), "held instead of emitting: {emitted:?}");
        assert!(emitted.len() >= MAX_HELD);
    }

    #[test]
    fn a_nested_bracket_still_restores_the_inner_token() {
        let mapping = mapped();
        let mut buffer = RestoreBuffer::new(&mapping);
        let mut out = String::new();
        out.push_str(&buffer.push("[see [PERSON").unwrap());
        out.push_str(&buffer.push("_1]]").unwrap());
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "[see Weber]");
    }

    #[test]
    fn an_unknown_placeholder_fails_before_it_is_emitted() {
        let mapping = mapped();
        let mut buffer = RestoreBuffer::new(&mapping);
        assert_eq!(buffer.push("Hallo [PERSON_9").unwrap(), "Hallo ");
        let error = buffer.push("]").unwrap_err();
        assert!(matches!(error, MappingError::Unknown(_)));
    }

    #[test]
    fn character_by_character_matches_the_whole_string() {
        // The mandatory slicing test at its finest granularity.
        let mapping = mapped();
        let source = "Sehr geehrter [PERSON_1], siehe [PERSON_1] und [note].";
        let mut buffer = RestoreBuffer::new(&mapping);
        let mut out = String::new();
        for character in source.chars() {
            out.push_str(&buffer.push(&character.to_string()).unwrap());
        }
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, mapping.restore(source).unwrap());
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --manifest-path gateway/Cargo.toml buffer_tests`
Expected: FAIL — `RestoreBuffer` does not exist.

- [ ] **Step 3: Implement**

```rust
use crate::mapping::{Mapping, MappingError};

/// A `[` that never closes would suspend the stream. Past this many characters
/// the bracket cannot begin a placeholder, so it is emitted as ordinary text.
pub const MAX_HELD: usize = 64;

/// Restores placeholders in text arriving piece by piece. A placeholder matching
/// `[TYPE_N]` contains no `[`, so only the text from the last unclosed `[` can
/// begin one; everything before it is complete and is emitted restored.
pub struct RestoreBuffer<'a> {
    mapping: &'a Mapping,
    held: String,
}

impl<'a> RestoreBuffer<'a> {
    pub fn new(mapping: &'a Mapping) -> Self {
        Self { mapping, held: String::new() }
    }

    pub fn push(&mut self, text: &str) -> Result<String, MappingError> {
        self.held.push_str(text);
        let mut emitted = String::new();
        loop {
            let split = self.safe_prefix_len();
            if split == 0 {
                break;
            }
            let rest = self.held.split_off(split);
            let ready = std::mem::replace(&mut self.held, rest);
            emitted.push_str(&self.mapping.restore(&ready)?);
            // Emitting past the cap may expose a further complete region.
            if self.held.len() <= MAX_HELD {
                break;
            }
        }
        Ok(emitted)
    }

    pub fn finish(&mut self) -> Result<String, MappingError> {
        let ready = std::mem::take(&mut self.held);
        self.mapping.restore(&ready)
    }

    /// Byte length of the prefix that cannot be part of a pending placeholder.
    fn safe_prefix_len(&self) -> usize {
        let candidate = match self.last_unclosed_bracket() {
            Some(index) => index,
            None => return self.held.len(),
        };
        if self.held.len() - candidate > MAX_HELD {
            // The bracket cannot start a placeholder. Release it with the text
            // before it; the next scan looks past it.
            return candidate + 1;
        }
        candidate
    }

    /// Index of the last `[` with no `]` after it.
    fn last_unclosed_bracket(&self) -> Option<usize> {
        let open = self.held.rfind('[')?;
        if self.held[open..].contains(']') {
            None
        } else {
            Some(open)
        }
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --manifest-path gateway/Cargo.toml buffer_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gateway/src/stream.rs gateway/src/main.rs
git commit -m "feat(gateway): hold-back buffer for placeholders split across chunks"
```

---

### Task 2: `SseFramer`

**Files:**
- Modify: `gateway/src/stream.rs`

**Interfaces:**
- Produces: `SseEvent { name: Option<String>, data: String }`, `SseFramer::new()`, `push(&mut self, &[u8]) -> Vec<SseEvent>`, `finish(&mut self) -> Option<SseEvent>`, `SseEvent::render(&self) -> String`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod framer_tests {
    use super::*;

    const BODY: &str = "event: content_block_delta\ndata: {\"a\":1}\n\ndata: [DONE]\n\n";

    #[test]
    fn complete_events_are_yielded_with_name_and_data() {
        let mut framer = SseFramer::new();
        let events = framer.push(BODY.as_bytes());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name.as_deref(), Some("content_block_delta"));
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].name, None);
        assert_eq!(events[1].data, "[DONE]");
    }

    #[test]
    fn a_partial_event_is_held_until_it_completes() {
        let mut framer = SseFramer::new();
        assert!(framer.push(b"data: {\"a\"").is_empty());
        assert!(framer.push(b":1}").is_empty());
        assert_eq!(framer.push(b"\n\n").len(), 1);
    }

    #[test]
    fn a_byte_at_a_time_yields_the_same_events() {
        // A chunk boundary inside a multi-byte character must not corrupt it.
        let body = "data: {\"t\":\"Grüße\"}\n\n";
        let mut framer = SseFramer::new();
        let mut events = Vec::new();
        for byte in body.as_bytes() {
            events.extend(framer.push(&[*byte]));
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"t\":\"Grüße\"}");
    }

    #[test]
    fn crlf_delimited_events_are_framed() {
        let mut framer = SseFramer::new();
        assert_eq!(framer.push(b"data: x\r\n\r\n").len(), 1);
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        let mut framer = SseFramer::new();
        let events = framer.push(b"data: one\ndata: two\n\n");
        assert_eq!(events[0].data, "one\ntwo");
    }

    #[test]
    fn trailing_bytes_without_a_blank_line_are_still_delivered() {
        // A body that ends without the final blank line must not swallow text.
        let mut framer = SseFramer::new();
        assert!(framer.push(b"data: tail").is_empty());
        assert_eq!(framer.finish().unwrap().data, "tail");
    }

    #[test]
    fn rendering_round_trips_a_named_event() {
        let event = SseEvent { name: Some("ping".into()), data: "{}".into() };
        assert_eq!(event.render(), "event: ping\ndata: {}\n\n");
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --manifest-path gateway/Cargo.toml framer_tests`
Expected: FAIL — `SseFramer` does not exist.

- [ ] **Step 3: Implement**

```rust
/// One SSE event: the `event:` name if the stream sent one, and the `data:`
/// lines joined as the specification requires.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub name: Option<String>,
    pub data: String,
}

impl SseEvent {
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(name) = &self.name {
            out.push_str("event: ");
            out.push_str(name);
            out.push('\n');
        }
        for line in self.data.split('\n') {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        out
    }
}

/// Frames a byte stream into events. HTTP chunks break anywhere, including the
/// middle of a UTF-8 character, so framing happens on bytes and decoding only
/// once an event is whole.
#[derive(Default)]
pub struct SseFramer {
    buffer: Vec<u8>,
}

impl SseFramer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some((block, rest)) = split_block(&self.buffer) {
            self.buffer = rest;
            if let Some(event) = parse_event(&block) {
                events.push(event);
            }
        }
        events
    }

    pub fn finish(&mut self) -> Option<SseEvent> {
        let block = std::mem::take(&mut self.buffer);
        parse_event(&block)
    }
}

/// Split off the first event block, delimited by a blank line in either
/// line-ending convention.
fn split_block(buffer: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    for (index, window) in buffer.windows(2).enumerate() {
        if window == b"\n\n" {
            return Some((buffer[..index].to_vec(), buffer[index + 2..].to_vec()));
        }
    }
    for (index, window) in buffer.windows(4).enumerate() {
        if window == b"\r\n\r\n" {
            return Some((buffer[..index].to_vec(), buffer[index + 4..].to_vec()));
        }
    }
    None
}

fn parse_event(block: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(block);
    let mut name = None;
    let mut data: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("event:") {
            name = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    if name.is_none() && data.is_empty() {
        return None;
    }
    Some(SseEvent { name, data: data.join("\n") })
}
```

Note on `split_block`: check `\n\n` first, then `\r\n\r\n`; a `\r\n\r\n` sequence contains no bare `\n\n`, so the orders do not collide.

- [ ] **Step 4: Run the tests**

Run: `cargo test --manifest-path gateway/Cargo.toml framer_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gateway/src/stream.rs
git commit -m "feat(gateway): frame SSE events from arbitrarily split bytes"
```

---

### Task 3: `Provider::stream_pointers`

**Files:**
- Modify: `gateway/src/provider.rs`

**Interfaces:**
- Produces: `fn stream_pointers(&self, event: &Value) -> Result<Vec<String>, ShapeError>` on the `Provider` trait, implemented for `OpenAi` and `Anthropic`.

- [ ] **Step 1: Write the failing tests**

Append to `provider.rs`'s existing `mod tests`:

```rust
    #[test]
    fn openai_finds_the_delta_content() {
        let event = json!({"choices": [{"index": 0, "delta": {"content": "hi"}}]});
        assert_eq!(OpenAi.stream_pointers(&event).unwrap(), ["/choices/0/delta/content"]);
    }

    #[test]
    fn openai_finds_every_choice_in_a_chunk() {
        let event = json!({"choices": [
            {"delta": {"content": "a"}},
            {"delta": {"content": "b"}}
        ]});
        assert_eq!(
            OpenAi.stream_pointers(&event).unwrap(),
            ["/choices/0/delta/content", "/choices/1/delta/content"]
        );
    }

    #[test]
    fn openai_yields_nothing_for_a_finish_chunk() {
        let event = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
        assert!(OpenAi.stream_pointers(&event).unwrap().is_empty());
    }

    #[test]
    fn openai_refuses_a_non_string_delta_content() {
        // A shape we recognize but cannot read is refused, never forwarded.
        let event = json!({"choices": [{"delta": {"content": {"parts": []}}}]});
        assert!(OpenAi.stream_pointers(&event).is_err());
    }

    #[test]
    fn openai_refuses_a_streamed_tool_call() {
        let event = json!({"choices": [{"delta": {"tool_calls": [{"index": 0}]}}]});
        assert!(OpenAi.stream_pointers(&event).is_err());
    }

    #[test]
    fn anthropic_finds_the_text_delta() {
        let event = json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}});
        assert_eq!(Anthropic.stream_pointers(&event).unwrap(), ["/delta/text"]);
    }

    #[test]
    fn anthropic_finds_the_text_of_an_opening_block() {
        let event = json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}});
        assert_eq!(Anthropic.stream_pointers(&event).unwrap(), ["/content_block/text"]);
    }

    #[test]
    fn anthropic_refuses_a_streamed_tool_block() {
        let event = json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "input": {}}});
        assert!(Anthropic.stream_pointers(&event).is_err());
    }

    #[test]
    fn anthropic_refuses_an_input_json_delta() {
        let event = json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{"}});
        assert!(Anthropic.stream_pointers(&event).is_err());
    }

    #[test]
    fn unknown_event_types_carry_no_text() {
        // `ping` and event types added later must not break a stream.
        assert!(Anthropic.stream_pointers(&json!({"type": "ping"})).unwrap().is_empty());
        assert!(OpenAi.stream_pointers(&json!({"object": "x"})).unwrap().is_empty());
    }
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --manifest-path gateway/Cargo.toml provider::tests`
Expected: FAIL — no method `stream_pointers`.

- [ ] **Step 3: Implement**

Add to the trait:

```rust
    /// Where the text lives inside one streamed event. An event type we do not
    /// know carries no pointers and is forwarded as it came: both protocols add
    /// event types over time, and `ping` must not break a stream.
    fn stream_pointers(&self, event: &Value) -> Result<Vec<String>, ShapeError>;
```

`OpenAi`:

```rust
    fn stream_pointers(&self, event: &Value) -> Result<Vec<String>, ShapeError> {
        let Some(choices) = event.get("choices").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        let mut pointers = Vec::new();
        for (index, choice) in choices.iter().enumerate() {
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            // Tool arguments stream as their own field. Masking them is a later
            // slice; until then they are refused rather than passed through.
            if delta.get("tool_calls").is_some_and(|v| !v.is_null())
                || delta.get("function_call").is_some_and(|v| !v.is_null())
            {
                return Err(ShapeError::Unsupported("openai", "tool_calls"));
            }
            match delta.get("content") {
                None | Some(Value::Null) => {}
                Some(Value::String(_)) => {
                    pointers.push(format!("/choices/{index}/delta/content"))
                }
                Some(_) => return Err(ShapeError::Response("openai")),
            }
        }
        Ok(pointers)
    }
```

`Anthropic`:

```rust
    fn stream_pointers(&self, event: &Value) -> Result<Vec<String>, ShapeError> {
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_delta") => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => match delta.get("text") {
                        Some(Value::String(_)) => Ok(vec!["/delta/text".to_owned()]),
                        _ => Err(ShapeError::Response("anthropic")),
                    },
                    // `input_json_delta` streams tool arguments past the masker.
                    Some("input_json_delta") => {
                        Err(ShapeError::Unsupported("anthropic", "tool_use"))
                    }
                    _ => Err(ShapeError::Response("anthropic")),
                }
            }
            Some("content_block_start") => {
                let block = event.get("content_block").unwrap_or(&Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => match block.get("text") {
                        Some(Value::String(_)) => Ok(vec!["/content_block/text".to_owned()]),
                        _ => Err(ShapeError::Response("anthropic")),
                    },
                    Some("tool_use") => Err(ShapeError::Unsupported("anthropic", "tool_use")),
                    _ => Err(ShapeError::Response("anthropic")),
                }
            }
            _ => Ok(Vec::new()),
        }
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --manifest-path gateway/Cargo.toml`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gateway/src/provider.rs
git commit -m "feat(gateway): describe where text lives in a streamed event"
```

---

### Task 4: The streaming response pipeline

**Files:**
- Modify: `gateway/src/stream.rs`, `gateway/src/proxy.rs`
- Add dependency: `futures-util` in `gateway/Cargo.toml`

**Interfaces:**
- Consumes: `RestoreBuffer`, `SseFramer`, `SseEvent`, `Provider::stream_pointers`, `Mapping`.
- Produces: `pub fn restore_stream(response: reqwest::Response, provider: &'static dyn Provider, mapping: Mapping) -> Response` in `stream.rs`, and a `StreamRestorer` driving one event at a time so the logic is testable without a network.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod restorer_tests {
    use super::*;
    use crate::mapping::{Mapping, Span};
    use crate::provider::OpenAi;

    fn mapped() -> Mapping {
        let mut mapping = Mapping::new();
        mapping
            .mask("Weber", &[Span { entity_type: "PERSON".into(), start: 0, end: 5 }])
            .unwrap();
        mapping
    }

    fn text_of(rendered: &str) -> String {
        // Concatenate every delta the client would have seen.
        let mut out = String::new();
        for block in rendered.split("\n\n").filter(|b| !b.is_empty()) {
            for line in block.split('\n') {
                let Some(data) = line.strip_prefix("data: ") else { continue };
                if data == "[DONE]" {
                    continue;
                }
                let value: serde_json::Value = serde_json::from_str(data).unwrap();
                if let Some(text) = value
                    .pointer("/choices/0/delta/content")
                    .and_then(|v| v.as_str())
                {
                    out.push_str(text);
                }
            }
        }
        out
    }

    const BODY: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hallo [PER\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"SON_1], bis \"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"bald [PERSON_1]\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    fn run(chunk_size: usize) -> String {
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let mut rendered = String::new();
        for chunk in BODY.as_bytes().chunks(chunk_size) {
            rendered.push_str(&restorer.push(chunk).unwrap());
        }
        rendered.push_str(&restorer.finish().unwrap());
        rendered
    }

    #[test]
    fn every_slicing_granularity_produces_the_same_text() {
        // The mandatory test: a restoration that works on natural chunk
        // boundaries and fails on a one-byte split is the bug this slice exists
        // to remove.
        let expected = "Hallo Weber, bis bald Weber";
        for chunk_size in 1..=BODY.len() {
            assert_eq!(text_of(&run(chunk_size)), expected, "chunk size {chunk_size}");
        }
    }

    #[test]
    fn no_placeholder_ever_reaches_the_client() {
        for chunk_size in 1..=BODY.len() {
            assert!(!run(chunk_size).contains("PERSON_1"), "chunk size {chunk_size}");
        }
    }

    #[test]
    fn the_terminal_events_survive() {
        let rendered = run(BODY.len());
        assert!(rendered.contains("finish_reason"));
        assert!(rendered.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn an_unknown_placeholder_terminates_the_stream() {
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hallo [PERSON_9]\"}}]}\n\n";
        let mut rendered = String::new();
        rendered.push_str(&restorer.push(body.as_bytes()).unwrap_or_default());
        let error = restorer.push(b"data: [DONE]\n\n").unwrap_err();
        assert!(matches!(error, MappingError::Unknown(_)));
        assert!(!rendered.contains("PERSON_9"));
    }

    #[test]
    fn an_unknown_event_type_passes_through() {
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let rendered = restorer.push(b"event: ping\ndata: {}\n\n").unwrap();
        let tail = restorer.finish().unwrap();
        assert!(format!("{rendered}{tail}").contains("event: ping"));
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --manifest-path gateway/Cargo.toml restorer_tests`
Expected: FAIL — `StreamRestorer` does not exist.

- [ ] **Step 3: Implement `StreamRestorer`**

```rust
use std::collections::HashMap;

use crate::provider::{read_pointer, write_pointer, Provider};

/// Restores an SSE response as it arrives. Restored text does not have the
/// length of the masked text, so a delta is rewritten whole; text still held at
/// the last text-bearing event has nowhere to go once that event is sent, so
/// text-bearing events are emitted one behind.
pub struct StreamRestorer<'a> {
    provider: &'a dyn Provider,
    mapping: &'a Mapping,
    framer: SseFramer,
    buffers: HashMap<String, RestoreBuffer<'a>>,
    pending: Option<SseEvent>,
}

impl<'a> StreamRestorer<'a> {
    pub fn new(provider: &'a dyn Provider, mapping: &'a Mapping) -> Self {
        Self {
            provider,
            mapping,
            framer: SseFramer::new(),
            buffers: HashMap::new(),
            pending: None,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<String, StreamError> {
        let events = self.framer.push(chunk);
        let mut out = String::new();
        for event in events {
            out.push_str(&self.handle(event)?);
        }
        Ok(out)
    }

    pub fn finish(&mut self) -> Result<String, StreamError> {
        let mut out = String::new();
        if let Some(event) = self.framer.finish() {
            out.push_str(&self.handle(event)?);
        }
        out.push_str(&self.flush()?);
        Ok(out)
    }

    fn handle(&mut self, event: SseEvent) -> Result<String, StreamError> {
        // `[DONE]` and anything that is not JSON carries no text.
        let Ok(parsed) = serde_json::from_str::<Value>(&event.data) else {
            let mut out = self.flush()?;
            out.push_str(&event.render());
            return Ok(out);
        };
        let pointers = self.provider.stream_pointers(&parsed)?;
        if pointers.is_empty() {
            let mut out = self.flush()?;
            out.push_str(&event.render());
            return Ok(out);
        }

        let mut rewritten = parsed.clone();
        for pointer in &pointers {
            let text = read_pointer(&parsed, pointer)?;
            let safe = self
                .buffers
                .entry(pointer.clone())
                .or_insert_with(|| RestoreBuffer::new(self.mapping))
                .push(&text)?;
            write_pointer(&mut rewritten, pointer, &safe)?;
        }
        let previous = self.pending.replace(SseEvent {
            name: event.name,
            data: rewritten.to_string(),
        });
        Ok(previous.map(|event| event.render()).unwrap_or_default())
    }

    /// The text run has ended: drain every buffer into the pending event and
    /// emit it.
    fn flush(&mut self) -> Result<String, StreamError> {
        let mut remainders: Vec<(String, String)> = Vec::new();
        for (pointer, buffer) in self.buffers.iter_mut() {
            let rest = buffer.finish()?;
            if !rest.is_empty() {
                remainders.push((pointer.clone(), rest));
            }
        }
        self.buffers.clear();

        let Some(event) = self.pending.take() else {
            // Text with nowhere to go is not dropped: it is refused.
            if let Some((pointer, _)) = remainders.first() {
                return Err(StreamError::Unplaceable(pointer.clone()));
            }
            return Ok(String::new());
        };
        let mut parsed: Value = serde_json::from_str(&event.data)
            .map_err(|_| StreamError::Unplaceable("pending".to_owned()))?;
        for (pointer, rest) in remainders {
            let existing = read_pointer(&parsed, &pointer)
                .map_err(|_| StreamError::Unplaceable(pointer.clone()))?;
            write_pointer(&mut parsed, &pointer, &format!("{existing}{rest}"))?;
        }
        Ok(SseEvent { name: event.name, data: parsed.to_string() }.render())
    }
}
```

with

```rust
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("{0}")]
    Mapping(#[from] MappingError),
    #[error("{0}")]
    Shape(#[from] ShapeError),
    #[error("restored text had no place in the stream at {0}; the stream is ended rather than served without it")]
    Unplaceable(String),
}
```

Note: the test asserting `MappingError::Unknown` matches on `StreamError::Mapping(MappingError::Unknown(_))`; write it that way.

- [ ] **Step 4: Run the unit tests**

Run: `cargo test --manifest-path gateway/Cargo.toml restorer_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gateway/src/stream.rs
git commit -m "feat(gateway): restore an SSE response one event behind"
```

- [ ] **Step 6: Write the failing wiring test**

In `gateway/src/proxy.rs`'s test module, using the existing wiremock helpers:

```rust
    #[tokio::test]
    async fn a_streaming_response_reaches_the_client_restored() {
        let server = MockServer::start().await;
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hallo [PER\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"SON_1]\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let response = post_chat(
            &server,
            json!({
                "model": "gpt-4o",
                "stream": true,
                "messages": [{"role": "user", "content": "Weber schreibt"}]
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "text/event-stream"
        );
        let served = body_text(response).await;
        assert!(served.contains("Weber"), "not restored: {served}");
        assert!(!served.contains("PERSON_1"), "placeholder served: {served}");
    }

    #[tokio::test]
    async fn a_streaming_request_is_forwarded_with_the_flag_intact() {
        // The pre-flight refusal is gone; the upstream must still be told to
        // stream, or it answers with a whole body the client will not parse.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_partial_json(json!({"stream": true})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("data: [DONE]\n\n"),
            )
            .mount(&server)
            .await;
        let response = post_chat(
            &server,
            json!({"model": "gpt-4o", "stream": true, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
```

If `post_chat` and `body_text` helpers do not exist in the test module, write them next to the existing tests, matching how those tests build the router and read a response body.

- [ ] **Step 7: Run them and watch them fail**

Run: `cargo test --manifest-path gateway/Cargo.toml proxy`
Expected: FAIL — the request is refused with 400.

- [ ] **Step 8: Wire it into `handle`**

Remove the pre-flight refusal and the `ProxyError::Streaming` variant. After the upstream call, before reading the body:

```rust
    // A stream is restored as it arrives; a non-success status is not a stream
    // and keeps the buffered path below.
    let streaming = status.is_success()
        && response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
    if streaming {
        return Ok(crate::stream::restore_stream(response, provider, mapping, returned));
    }
```

`restore_stream` wraps the byte stream, holding the `Mapping` alive alongside the restorer, and ends the stream with the error event on failure:

```rust
pub fn restore_stream(
    response: reqwest::Response,
    provider: &'static dyn Provider,
    mapping: Mapping,
    headers: HeaderMap,
) -> Response {
    let body = async_stream::stream! {
        let mut upstream = response.bytes_stream();
        let mut restorer = StreamRestorer::new(provider, &mapping);
        loop {
            match upstream.next().await {
                Some(Ok(chunk)) => match restorer.push(&chunk) {
                    Ok(out) if out.is_empty() => continue,
                    Ok(out) => yield Ok::<_, std::convert::Infallible>(Bytes::from(out)),
                    Err(error) => {
                        yield Ok(Bytes::from(error_event(&error)));
                        return;
                    }
                },
                Some(Err(error)) => {
                    yield Ok(Bytes::from(error_event_message(&error.to_string())));
                    return;
                }
                None => break,
            }
        }
        match restorer.finish() {
            Ok(out) if !out.is_empty() => yield Ok(Bytes::from(out)),
            Ok(_) => {}
            Err(error) => yield Ok(Bytes::from(error_event(&error))),
        }
    };
    let mut response = Response::new(axum::body::Body::from_stream(body));
    *response.headers_mut() = headers;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    response
}

/// The error the client sees when restoration fails after bytes have gone out.
/// The message names the failure and, at most, a placeholder — never a value.
fn error_event(error: &StreamError) -> String {
    error_event_message(&error.to_string())
}

fn error_event_message(message: &str) -> String {
    SseEvent {
        name: Some("error".to_owned()),
        data: json!({"error": {"type": "tessera_restoration_failed", "message": message}})
            .to_string(),
    }
    .render()
}
```

The `&'static dyn Provider` bound comes from the router already holding `&OpenAi` / `&Anthropic` as unit structs; if the borrow does not typecheck, pass the provider by value as an enum or `Arc<dyn Provider>` rather than widening lifetimes elsewhere. The `Mapping` is moved into the stream so it outlives the response.

Add to `gateway/Cargo.toml`: `futures-util = "0.3"` and `async-stream = "0.3"`.

- [ ] **Step 9: Run the whole suite**

Run: `cargo test --manifest-path gateway/Cargo.toml && cargo clippy --manifest-path gateway/Cargo.toml --all-targets -- -D warnings && cargo fmt --manifest-path gateway/Cargo.toml --check`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add gateway/
git commit -m "feat(gateway): serve streaming responses restored"
```

---

### Task 5: Documentation

**Files:**
- Modify: `gateway/README.md`, `README.md`

- [ ] **Step 1: Record the behaviour**

State that `stream: true` is served for both providers; that a placeholder split across events is joined; that matching is exact, with the reason; that an unrestorable token ends the stream with an `error` event rather than serving the placeholder; and that streamed tool calls are still refused.

- [ ] **Step 2: Run the suite once more**

Run: `cargo test --manifest-path gateway/Cargo.toml`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add README.md gateway/README.md
git commit -m "docs: streaming behaviour and its limits"
```
