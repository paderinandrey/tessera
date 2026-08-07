//! Restoration of a response that arrives in pieces.
//!
//! The buffered path hands a whole string to `Mapping::restore`. A stream has
//! no whole string: `[PERSON_1]` arrives as `[PER` in one event and `SON_1]` in
//! the next, and the HTTP chunks under those events break at arbitrary byte
//! offsets, including the middle of a UTF-8 character. Restoring per chunk
//! would emit `[PER` to the client and never recognize the token.

use std::collections::BTreeMap;
use std::convert::Infallible;

use axum::body::{Body, Bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::Value;

use crate::mapping::{Mapping, MappingError};
use crate::provider::{read_pointer, write_pointer, Provider, ShapeError, Terminates};

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("{0}")]
    Mapping(#[from] MappingError),
    #[error("{0}")]
    Shape(#[from] ShapeError),
    #[error(
        "restored text had no place in the stream at {0}; the stream ends rather \
         than continuing without it"
    )]
    Unplaceable(String),
    #[error("upstream sent an event larger than this gateway will buffer; the stream ends")]
    Oversized,
    #[error(
        "upstream sent more behind an unfinished event than this gateway will hold; \
         the stream ends"
    )]
    Stalled,
    #[error(
        "upstream sent an event this gateway cannot parse; the stream ends rather \
         than forwarding text it could not restore"
    )]
    Malformed,
}

/// How much of an unfinished event to hold. A response that never sends a blank
/// line would otherwise be buffered whole, which is the memory cost streaming
/// exists to avoid. Provider events are a few hundred bytes.
pub const MAX_EVENT_BYTES: usize = 1 << 20;

/// How much may wait behind an event that has not been released yet. A stream
/// that stalls after one delta and then sends keepalives forever would
/// otherwise grow without bound, which the per-event cap does not cover.
pub const MAX_QUEUED_BYTES: usize = 64 << 10;

/// A `[` that never closes would suspend the stream. Past this many bytes the
/// bracket cannot begin a placeholder, so it is emitted as ordinary text.
///
/// This is a bound on what the masker can issue, not a guess:
/// `mapping::MAX_ENTITY_TYPE` keeps every placeholder under it, so releasing a
/// bracket here can never orphan a real token.
pub const MAX_HELD: usize = 64;

/// Restores placeholders in text arriving piece by piece. A placeholder
/// matching `[TYPE_N]` contains no `[`, so only the text from the last `[` with
/// no `]` after it can begin one; everything before that point is complete and
/// is emitted restored.
pub struct RestoreBuffer<'a> {
    mapping: &'a Mapping,
    held: String,
}

impl<'a> RestoreBuffer<'a> {
    pub fn new(mapping: &'a Mapping) -> Self {
        Self {
            mapping,
            held: String::new(),
        }
    }

    /// Append text and return the prefix that is safe to emit, restored.
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
            // Releasing a bracket that ran past the cap can expose a further
            // complete region behind it.
            if self.held.len() <= MAX_HELD {
                break;
            }
        }
        Ok(emitted)
    }

    /// Emit whatever is still held: the text run has ended.
    pub fn finish(&mut self) -> Result<String, MappingError> {
        let ready = std::mem::take(&mut self.held);
        self.mapping.restore(&ready)
    }

    /// Byte length of the prefix that cannot be part of a pending placeholder.
    fn safe_prefix_len(&self) -> usize {
        let Some(candidate) = self.last_unclosed_bracket() else {
            return self.held.len();
        };
        if self.held.len() - candidate > MAX_HELD {
            // Too long to become a placeholder. Release the bracket with the
            // text before it; the next scan looks past it.
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

/// One SSE event: the `event:` name if the stream sent one, the `data:` lines
/// joined as the specification requires, and every other line kept verbatim so
/// `id:`, `retry:` and comments reach the client as the provider sent them.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub name: Option<String>,
    pub data: Option<String>,
    other: Vec<String>,
}

impl SseEvent {
    pub fn new(name: Option<String>, data: Option<String>) -> Self {
        Self {
            name,
            data,
            other: Vec::new(),
        }
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(name) = &self.name {
            out.push_str("event: ");
            out.push_str(name);
            out.push('\n');
        }
        for line in &self.other {
            out.push_str(line);
            out.push('\n');
        }
        if let Some(data) = &self.data {
            for line in data.split('\n') {
                out.push_str("data: ");
                out.push_str(line);
                out.push('\n');
            }
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
    /// How far the delimiter scan has already looked. Without it every push
    /// rescans the whole retained buffer, which the size cap makes affordable
    /// but not free.
    scanned: usize,
}

impl SseFramer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, StreamError> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        loop {
            // A delimiter can straddle the point the last scan reached.
            let from = self.scanned.saturating_sub(3);
            match find_blank_line(&self.buffer, from) {
                Some((end, width)) => {
                    // Checked before the block is copied and parsed: an oversized
                    // event that arrives complete in one chunk must be refused,
                    // not buffered, cloned and handed on.
                    if end > MAX_EVENT_BYTES {
                        return Err(StreamError::Oversized);
                    }
                    let block = self.buffer[..end].to_vec();
                    self.buffer.drain(..end + width);
                    self.scanned = 0;
                    if let Some(event) = parse_event(&block) {
                        events.push(event);
                    }
                }
                None => {
                    if self.buffer.len() > MAX_EVENT_BYTES {
                        return Err(StreamError::Oversized);
                    }
                    self.scanned = self.buffer.len();
                    break;
                }
            }
        }
        Ok(events)
    }

    /// Bytes left over when the body ended. A stream that stops without its
    /// final blank line must not swallow the text it already sent.
    pub fn finish(&mut self) -> Option<SseEvent> {
        self.scanned = 0;
        let block = std::mem::take(&mut self.buffer);
        parse_event(&block)
    }
}

/// Offset and width of the first blank line, in either line-ending convention.
/// Scanning forward matters: a `\r\n\r\n` delimiter contains no `\n\n`, so
/// searching for `\n\n` across the whole buffer first would run past it and
/// merge two events into one.
fn find_blank_line(buffer: &[u8], from: usize) -> Option<(usize, usize)> {
    for index in from..buffer.len() {
        let rest = &buffer[index..];
        if rest.starts_with(b"\r\n\r\n") {
            return Some((index, 4));
        }
        if rest.starts_with(b"\n\n") {
            return Some((index, 2));
        }
    }
    None
}

fn parse_event(block: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(block);
    let mut event = SseEvent::new(None, None);
    let mut data: Vec<&str> = Vec::new();
    let mut seen = false;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        seen = true;
        if let Some(value) = line.strip_prefix("event:") {
            event.name = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        } else {
            event.other.push(line.to_owned());
        }
    }
    if !seen {
        return None;
    }
    if !data.is_empty() {
        event.data = Some(data.join("\n"));
    }
    Some(event)
}

/// Restores an SSE response as it arrives.
///
/// Restored text does not have the length of the masked text, so a delta is
/// rewritten whole rather than patched. That leaves one problem: text still
/// held when the last text-bearing event goes out has nowhere to live. So
/// text-bearing events are emitted one behind, and the first event that carries
/// no text — `finish_reason`, `content_block_stop`, `[DONE]` — ends the run and
/// releases what is held into the event waiting behind it.
pub struct StreamRestorer<'a> {
    provider: &'a dyn Provider,
    mapping: &'a Mapping,
    framer: SseFramer,
    /// One buffer per run of text, keyed by the provider's logical identity —
    /// not by the pointer, which repeats across interleaved choices.
    buffers: BTreeMap<String, Held<'a>>,
    pending: Option<Pending>,
    /// Events that arrived behind the waiting one and end nothing — keepalives,
    /// and event types these protocols grow later. They are held only to keep
    /// the order the provider sent, never to drain a buffer.
    queued: Vec<SseEvent>,
    queued_bytes: usize,
    /// Output that was already safe to serve when a failure stopped the stream.
    salvage: String,
}

/// A run of text in progress, and where its event wrote it last.
struct Held<'a> {
    buffer: RestoreBuffer<'a>,
    pointer: String,
}

/// The text-bearing event waiting one behind, and which runs it carries. A
/// remainder can only be appended to an event that holds the same run: with
/// interleaved choices two different completions share a pointer, and writing
/// into the wrong one would hand a client another client's text.
struct Pending {
    event: SseEvent,
    slots: BTreeMap<String, String>,
}

impl<'a> StreamRestorer<'a> {
    pub fn new(provider: &'a dyn Provider, mapping: &'a Mapping) -> Self {
        Self {
            provider,
            mapping,
            framer: SseFramer::new(),
            buffers: BTreeMap::new(),
            pending: None,
            queued: Vec::new(),
            queued_bytes: 0,
            salvage: String::new(),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<String, StreamError> {
        let mut out = String::new();
        for event in self.framer.push(chunk)? {
            match self.handle(event) {
                Ok(rendered) => out.push_str(&rendered),
                // Earlier events in this same chunk were rendered and are
                // correct. The failure stops the stream; it does not unmake them.
                Err(error) => {
                    self.salvage.push_str(&out);
                    return Err(error);
                }
            }
        }
        Ok(out)
    }

    /// What was already safe to serve when a failure stopped the stream: events
    /// rendered before the failing one, and whatever the one-event delay was
    /// still holding. The hold-back buffers are dropped untouched — they may
    /// contain the very token that could not be restored.
    pub fn salvage(&mut self) -> String {
        let mut out = std::mem::take(&mut self.salvage);
        self.buffers.clear();
        if let Some(pending) = self.pending.take() {
            out.push_str(&pending.event.render());
        }
        out.push_str(&self.release(String::new()));
        out
    }

    pub fn finish(&mut self) -> Result<String, StreamError> {
        let mut out = String::new();
        if let Some(event) = self.framer.finish() {
            out.push_str(&self.handle(event)?);
        }
        let released = self.flush(&Terminates::All)?;
        out.push_str(&self.release(released));
        Ok(out)
    }

    fn handle(&mut self, event: SseEvent) -> Result<String, StreamError> {
        let data = event.data.as_deref().unwrap_or("");
        // Protocol sentinels are not JSON and carry no text. `[DONE]` ends
        // everything; an event with no data at all ends nothing.
        if data.is_empty() {
            return self.hold(event);
        }
        if data == "[DONE]" {
            let released = self.flush(&Terminates::All)?;
            let mut out = self.release(released);
            out.push_str(&event.render());
            return Ok(out);
        }
        // Anything else claiming to be data must parse. A truncated event that
        // still contains `[PERSON_1]` would otherwise be rendered unchanged and
        // hand the token to the client.
        let parsed: Value = serde_json::from_str(data).map_err(|_| StreamError::Malformed)?;

        let slots = self.provider.stream_slots(&parsed)?;
        if slots.is_empty() {
            // No text of its own — but a provider's error envelope quotes what we
            // sent, so every string in it is restored, exactly as the buffered
            // path does with an error body.
            let mut event = event;
            event.data = Some(self.mapping.restore_value(&parsed)?.to_string());
            // Only an event that actually ends a run drains its buffer. A
            // keepalive between two deltas would otherwise release `[PER` as
            // text and let the client reassemble the token from the pieces.
            let terminates = self.provider.stream_terminates(&parsed);
            if terminates == Terminates::Nothing {
                return self.hold(event);
            }
            let released = self.flush(&terminates)?;
            let mut out = self.release(released);
            out.push_str(&event.render());
            return Ok(out);
        }

        let mapping = self.mapping;
        let mut rewritten = parsed.clone();
        let mut carried = BTreeMap::new();
        for slot in &slots {
            let text = read_pointer(&parsed, &slot.pointer)?;
            let held = self
                .buffers
                .entry(slot.key.clone())
                .or_insert_with(|| Held {
                    buffer: RestoreBuffer::new(mapping),
                    pointer: slot.pointer.clone(),
                });
            held.pointer = slot.pointer.clone();
            let safe = held.buffer.push(&text)?;
            write_pointer(&mut rewritten, &slot.pointer, &safe)?;
            carried.insert(slot.key.clone(), slot.pointer.clone());
        }
        let mut event = event;
        event.data = Some(rewritten.to_string());
        let previous = self.pending.replace(Pending {
            event,
            slots: carried,
        });
        let released = previous
            .map(|pending| pending.event.render())
            .unwrap_or_default();
        Ok(self.release(released))
    }

    /// Keep an event that ends nothing. With something already waiting it goes
    /// behind, so the provider's order survives; with nothing waiting there is
    /// nothing to wait for.
    fn hold(&mut self, event: SseEvent) -> Result<String, StreamError> {
        if self.pending.is_none() {
            return Ok(event.render());
        }
        self.queued_bytes += event.render().len();
        if self.queued_bytes > MAX_QUEUED_BYTES {
            return Err(StreamError::Stalled);
        }
        self.queued.push(event);
        Ok(String::new())
    }

    /// Everything held behind the event just released.
    fn release(&mut self, released: String) -> String {
        let mut out = released;
        for event in std::mem::take(&mut self.queued) {
            out.push_str(&event.render());
        }
        self.queued_bytes = 0;
        out
    }

    /// The text run has ended: drain every buffer into the waiting event.
    fn flush(&mut self, terminates: &Terminates) -> Result<String, StreamError> {
        let ends = |key: &String| match terminates {
            Terminates::All => true,
            Terminates::Runs(keys) => keys.contains(key),
            Terminates::Nothing => false,
        };
        let mut remainders: Vec<(String, String)> = Vec::new();
        for (key, held) in self.buffers.iter_mut() {
            if !ends(key) {
                continue;
            }
            let rest = held.buffer.finish()?;
            if !rest.is_empty() {
                remainders.push((key.clone(), rest));
            }
        }
        self.buffers.retain(|key, _| !ends(key));

        let Some(mut pending) = self.pending.take() else {
            // Restored text with nowhere to go is not dropped quietly.
            if let Some((key, _)) = remainders.first() {
                return Err(StreamError::Unplaceable(key.clone()));
            }
            return Ok(String::new());
        };
        if remainders.is_empty() {
            return Ok(pending.event.render());
        }
        let mut parsed: Value =
            serde_json::from_str(pending.event.data.as_deref().unwrap_or(""))
                .map_err(|_| StreamError::Unplaceable("the waiting event".to_owned()))?;
        for (key, rest) in remainders {
            // Only into the event that carries this same run. A pointer that
            // happens to match is not the same completion.
            let pointer = pending
                .slots
                .get(&key)
                .ok_or_else(|| StreamError::Unplaceable(key.clone()))?;
            let existing = read_pointer(&parsed, pointer)
                .map_err(|_| StreamError::Unplaceable(key.clone()))?;
            write_pointer(&mut parsed, pointer, &format!("{existing}{rest}"))?;
        }
        pending.event.data = Some(parsed.to_string());
        Ok(pending.event.render())
    }
}

/// Serve an upstream SSE response, restored as it arrives.
///
/// The mapping moves into the stream: it must outlive the response, and it must
/// not outlive it by a moment longer — it holds the values this request masked.
pub fn restore_stream(
    response: reqwest::Response,
    provider: &'static dyn Provider,
    mapping: Mapping,
    headers: HeaderMap,
) -> Response {
    let body = async_stream::stream! {
        let mut upstream = response.bytes_stream();
        let mut restorer = StreamRestorer::new(provider, &mapping);
        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                // The upstream broke off. Say so rather than let the client read
                // a truncated answer as a complete one — but text already
                // restored and waiting behind the one-event delay is safe to
                // serve, and a failed connection does not make it unsafe.
                Err(error) => {
                    let tail = match restorer.finish() {
                        Ok(tail) => tail,
                        Err(_) => restorer.salvage(),
                    };
                    if !tail.is_empty() {
                        yield Ok(Bytes::from(tail));
                    }
                    yield Ok(Bytes::from(error_event(&error.to_string())));
                    return;
                }
            };
            match restorer.push(&chunk) {
                Ok(out) if out.is_empty() => {}
                Ok(out) => yield Ok(Bytes::from(out)),
                // Restoration failed, so the stream ends — but text rendered
                // before the failing event is correct and is served first.
                Err(error) => {
                    let salvaged = restorer.salvage();
                    if !salvaged.is_empty() {
                        yield Ok(Bytes::from(salvaged));
                    }
                    yield Ok(Bytes::from(error_event(&error.to_string())));
                    return;
                }
            }
        }
        match restorer.finish() {
            Ok(out) if out.is_empty() => {}
            Ok(out) => yield Ok::<_, Infallible>(Bytes::from(out)),
            Err(error) => {
                let salvaged = restorer.salvage();
                if !salvaged.is_empty() {
                    yield Ok(Bytes::from(salvaged));
                }
                yield Ok(Bytes::from(error_event(&error.to_string())));
            }
        }
    };

    let mut response = Response::new(Body::from_stream(body));
    *response.headers_mut() = headers;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    response
}

/// What the client sees when restoration fails after bytes have already gone
/// out. The message names the failure and, at most, a placeholder — never a
/// value: `MappingError::Unknown` carries the token, not what it stood for.
fn error_event(message: &str) -> String {
    SseEvent::new(
        Some("error".to_owned()),
        Some(
            serde_json::json!({
                "error": {"type": "tessera_restoration_failed", "message": message}
            })
            .to_string(),
        ),
    )
    .render()
}

#[cfg(test)]
mod restorer_tests {
    use super::*;
    use crate::mapping::Span;
    use crate::provider::OpenAi;

    fn mapped() -> Mapping {
        let mut mapping = Mapping::new();
        mapping
            .mask(
                "Weber",
                &[Span {
                    entity_type: "PERSON".into(),
                    start: 0,
                    end: 5,
                }],
            )
            .unwrap();
        mapping
    }

    /// Concatenate every delta the client would have seen.
    fn text_of(rendered: &str) -> String {
        let mut out = String::new();
        for line in rendered.split('\n') {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(text) = value
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
            {
                out.push_str(text);
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
        for chunk_size in 1..=BODY.len() {
            assert_eq!(
                text_of(&run(chunk_size)),
                "Hallo Weber, bis bald Weber",
                "chunk size {chunk_size}"
            );
        }
    }

    #[test]
    fn no_placeholder_ever_reaches_the_client() {
        for chunk_size in 1..=BODY.len() {
            assert!(
                !run(chunk_size).contains("PERSON_1"),
                "chunk size {chunk_size}"
            );
        }
    }

    #[test]
    fn the_terminal_events_survive() {
        let rendered = run(BODY.len());
        assert!(rendered.contains("finish_reason"));
        assert!(rendered.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn an_unknown_placeholder_ends_the_stream_before_it_is_served() {
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let rendered = restorer
            .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hallo [PERSON\"}}]}\n\n")
            .unwrap();
        let error = restorer
            .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"_9]\"}}]}\n\n")
            .unwrap_err();
        assert!(matches!(
            error,
            StreamError::Mapping(MappingError::Unknown(_))
        ));
        assert!(!rendered.contains("PERSON"));
    }

    /// Every delta the client would have seen for one choice, in order.
    fn text_for_choice(rendered: &str, index: u64) -> String {
        let mut out = String::new();
        for line in rendered.split('\n') {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            let Some(choices) = value.get("choices").and_then(Value::as_array) else {
                continue;
            };
            for choice in choices {
                if choice.get("index").and_then(Value::as_u64) != Some(index) {
                    continue;
                }
                if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
        }
        out
    }

    #[test]
    fn interleaved_choices_do_not_share_a_hold_back_buffer() {
        // With `n > 1` each chunk carries one choice at array position 0. Keying
        // the buffer on the pointer would splice two completions together and
        // emit corrupted text instead of failing safely.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"A [PER\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"B [PER\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"SON_1] one\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"SON_1] two\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());

        assert_eq!(text_for_choice(&rendered, 0), "A Weber one");
        assert_eq!(text_for_choice(&rendered, 1), "B Weber two");
    }

    #[test]
    fn a_remainder_is_never_written_into_another_choices_event() {
        // Choice 0 ends mid-token while choice 1 is the event waiting behind.
        // The two share a pointer but not a completion, so the stream ends
        // rather than hand one client the other's text.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        restorer
            .push(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"A [PER\"}}]}\n\n")
            .unwrap();
        restorer
            .push(b"data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"B\"}}]}\n\n")
            .unwrap();
        let error = restorer.push(b"data: [DONE]\n\n").unwrap_err();
        assert!(matches!(error, StreamError::Unplaceable(_)), "{error}");
    }

    /// Concatenate every Anthropic text delta the client would have seen.
    fn anthropic_text(rendered: &str) -> String {
        let mut out = String::new();
        for line in rendered.split('\n') {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            for pointer in ["/delta/text", "/content_block/text"] {
                if let Some(text) = value.pointer(pointer).and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
        }
        out
    }

    #[test]
    fn a_keepalive_between_deltas_does_not_release_half_a_placeholder() {
        // Anthropic sends `ping` mid-generation. Treating it as the end of the
        // text run would emit `[PER` as ordinary text and `SON_1]` after it, and
        // the client would reassemble the token this gateway exists to hide.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let body = concat!(
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
             \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hallo [PER\"}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
             \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"SON_1]!\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());

        assert_eq!(anthropic_text(&rendered), "Hallo Weber!");
        assert!(rendered.contains("event: ping"), "keepalive dropped");
        assert!(
            rendered.contains("event: message_stop"),
            "truncated: {rendered}"
        );
    }

    #[test]
    fn a_keepalive_keeps_its_place_in_the_order() {
        // It arrived after the delta being held, and it goes out after it.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let body = concat!(
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
             \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"one \"}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
             \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"two\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());
        let first = rendered.find("one ").unwrap();
        let ping = rendered.find("event: ping").unwrap();
        let second = rendered.find("two").unwrap();
        assert!(first < ping && ping < second, "reordered: {rendered}");
    }

    #[test]
    fn stopping_one_block_leaves_another_block_held() {
        // `content_block_stop` ends its own run, not every run in flight.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let body = concat!(
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
             \"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"held [PER\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
             \"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"SON_1]\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        );
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());
        assert_eq!(anthropic_text(&rendered), "held Weber");
    }

    #[test]
    fn an_openai_chunk_without_a_finish_reason_ends_nothing() {
        // A usage-only chunk arriving mid-run must not drain the buffer.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hallo [PER\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"total_tokens\":7}}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"SON_1]\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());
        assert_eq!(text_for_choice(&rendered, 0), "Hallo Weber");
    }

    #[test]
    fn a_malformed_data_event_ends_the_stream() {
        // A truncated event still carrying a placeholder must not be rendered
        // back unchanged.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let error = restorer
            .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hallo [PERSON_1]\n\n")
            .unwrap_err();
        assert!(matches!(error, StreamError::Malformed), "{error}");
    }

    #[test]
    fn a_provider_error_event_is_restored_before_it_is_forwarded() {
        // An upstream error quotes what we sent it. The buffered path restores
        // every string in the envelope; a stream must not be the exception.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let rendered = restorer
            .push(
                b"event: error\ndata: {\"type\":\"error\",\"error\":\
{\"type\":\"invalid_request_error\",\"message\":\"bad input [PERSON_1]\"}}\n\n",
            )
            .unwrap();
        assert!(rendered.contains("bad input Weber"), "{rendered}");
        assert!(!rendered.contains("PERSON_1"), "{rendered}");
    }

    #[test]
    fn an_oversized_event_that_arrives_complete_is_refused() {
        // The cap must bound a whole event, not only an unfinished one: a
        // delimiter in the same chunk would otherwise let it through.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let mut body = b"data: ".to_vec();
        body.extend(std::iter::repeat_n(b'x', MAX_EVENT_BYTES + 1));
        body.extend_from_slice(b"\n\n");
        assert!(matches!(
            restorer.push(&body).unwrap_err(),
            StreamError::Oversized
        ));
    }

    #[test]
    fn a_failure_does_not_unmake_the_events_already_rendered() {
        // Two good events and a malformed one in a single chunk. The stream
        // ends, but the text that was already correct is still served.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let chunk = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"one \"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"two \"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"broken\n\n",
        );
        assert!(restorer.push(chunk.as_bytes()).is_err());
        let salvaged = restorer.salvage();
        assert_eq!(text_for_choice(&salvaged, 0), "one two ");
    }

    #[test]
    fn salvage_never_drains_a_hold_back_buffer() {
        // The buffer may hold the very token that could not be restored.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        restorer
            .push(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"safe [PER\"}}]}\n\n")
            .unwrap();
        let salvaged = restorer.salvage();
        assert_eq!(text_for_choice(&salvaged, 0), "safe ");
        assert!(
            !salvaged.contains("[PER"),
            "held text was released: {salvaged}"
        );
    }

    #[test]
    fn endless_keepalives_behind_a_held_event_end_the_stream() {
        // The per-event cap does not cover a stream that stalls after one delta
        // and then pings forever.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        restorer
            .push(
                b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            )
            .unwrap();
        let ping = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";
        let mut error = None;
        for _ in 0..(MAX_QUEUED_BYTES / ping.len() + 2) {
            if let Err(failure) = restorer.push(ping) {
                error = Some(failure);
                break;
            }
        }
        assert!(matches!(error, Some(StreamError::Stalled)), "{error:?}");
    }

    #[test]
    fn a_keepalive_run_that_ends_in_text_does_not_accumulate() {
        // Releasing the queue must reset its budget, or a long stream would trip
        // the cap on keepalives it already delivered.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let delta = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n";
        let ping = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";
        for _ in 0..2000 {
            restorer.push(delta).unwrap();
            restorer.push(ping).unwrap();
        }
    }

    #[test]
    fn an_event_that_never_ends_stops_the_stream() {
        // Buffering the whole response is the cost streaming exists to avoid.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let flood = vec![b'x'; MAX_EVENT_BYTES + 1];
        assert!(matches!(
            restorer.push(&flood).unwrap_err(),
            StreamError::Oversized
        ));
    }

    #[test]
    fn an_unknown_event_type_passes_through() {
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let rendered = restorer.push(b"event: ping\ndata: {}\n\n").unwrap();
        assert!(rendered.contains("event: ping"));
    }

    #[test]
    fn a_streamed_tool_call_ends_the_stream() {
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let error = restorer
            .push(b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0}]}}]}\n\n")
            .unwrap_err();
        assert!(matches!(error, StreamError::Shape(_)));
    }

    #[test]
    fn a_body_that_stops_mid_placeholder_still_serves_what_it_held() {
        // An upstream that dies mid-token must not swallow the text before it.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        restorer
            .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hallo [no\"}}]}\n\n")
            .unwrap();
        let tail = restorer.finish().unwrap();
        assert!(tail.contains("Hallo [no"), "text was swallowed: {tail}");
    }
}

#[cfg(test)]
mod framer_tests {
    use super::*;

    fn unwrap_push(framer: &mut SseFramer, chunk: &[u8]) -> Vec<SseEvent> {
        framer.push(chunk).unwrap()
    }

    const BODY: &str = "event: content_block_delta\ndata: {\"a\":1}\n\ndata: [DONE]\n\n";

    #[test]
    fn complete_events_are_yielded_with_name_and_data() {
        let mut framer = SseFramer::new();
        let events = unwrap_push(&mut framer, BODY.as_bytes());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name.as_deref(), Some("content_block_delta"));
        assert_eq!(events[0].data.as_deref(), Some("{\"a\":1}"));
        assert_eq!(events[1].name, None);
        assert_eq!(events[1].data.as_deref(), Some("[DONE]"));
    }

    #[test]
    fn a_partial_event_is_held_until_it_completes() {
        let mut framer = SseFramer::new();
        assert!(unwrap_push(&mut framer, b"data: {\"a\"").is_empty());
        assert!(unwrap_push(&mut framer, b":1}").is_empty());
        assert_eq!(unwrap_push(&mut framer, b"\n\n").len(), 1);
    }

    #[test]
    fn a_byte_at_a_time_yields_the_same_events() {
        // A chunk boundary inside a multi-byte character must not corrupt it.
        let body = "data: {\"t\":\"Grüße\"}\n\n";
        let mut framer = SseFramer::new();
        let mut events = Vec::new();
        for byte in body.as_bytes() {
            events.extend(unwrap_push(&mut framer, &[*byte]));
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data.as_deref(), Some("{\"t\":\"Grüße\"}"));
    }

    #[test]
    fn crlf_delimited_events_are_framed_separately() {
        // Searching the whole buffer for "\n\n" first would merge these two.
        let mut framer = SseFramer::new();
        let events = unwrap_push(&mut framer, b"data: one\r\n\r\ndata: two\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data.as_deref(), Some("one"));
        assert_eq!(events[1].data.as_deref(), Some("two"));
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        let mut framer = SseFramer::new();
        let events = unwrap_push(&mut framer, b"data: one\ndata: two\n\n");
        assert_eq!(events[0].data.as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn trailing_bytes_without_a_blank_line_are_still_delivered() {
        // A body that ends without the final blank line must not swallow text.
        let mut framer = SseFramer::new();
        assert!(unwrap_push(&mut framer, b"data: tail").is_empty());
        assert_eq!(framer.finish().unwrap().data.as_deref(), Some("tail"));
    }

    #[test]
    fn other_fields_survive_the_round_trip() {
        // Dropping `id:` would break a client's resume.
        let mut framer = SseFramer::new();
        let events = unwrap_push(&mut framer, b"id: 7\nevent: ping\ndata: {}\n\n");
        assert_eq!(events[0].render(), "event: ping\nid: 7\ndata: {}\n\n");
    }

    #[test]
    fn an_event_without_data_renders_no_data_line() {
        let mut framer = SseFramer::new();
        let events = unwrap_push(&mut framer, b"event: ping\n\n");
        assert_eq!(events[0].render(), "event: ping\n\n");
    }
}

#[cfg(test)]
mod buffer_tests {
    use super::*;
    use crate::mapping::Span;

    fn mapped() -> Mapping {
        let mut mapping = Mapping::new();
        mapping
            .mask(
                "Weber",
                &[Span {
                    entity_type: "PERSON".into(),
                    start: 0,
                    end: 5,
                }],
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
        assert_eq!(
            buffer.push("plenty of text [PER").unwrap(),
            "plenty of text "
        );
    }

    #[test]
    fn an_unclosed_bracket_past_the_cap_stops_holding() {
        // "[note" followed by prose must not suspend the stream forever.
        let mapping = mapped();
        let mut buffer = RestoreBuffer::new(&mapping);
        let long = "x".repeat(MAX_HELD + 10);
        let emitted = buffer.push(&format!("[note {long}")).unwrap();
        assert!(
            emitted.starts_with("[note "),
            "held instead of emitting: {emitted:?}"
        );
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

    #[test]
    fn the_longest_placeholder_the_masker_can_issue_still_fits_the_cap() {
        // The cap must bound what masking issues, or a legitimate token would be
        // released as text and reach the client unrestored.
        let entity_type = "A".repeat(crate::mapping::MAX_ENTITY_TYPE);
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask(
                "Weber",
                &[Span {
                    entity_type: entity_type.clone(),
                    start: 0,
                    end: 5,
                }],
            )
            .unwrap();
        assert!(masked.len() <= MAX_HELD, "{} bytes", masked.len());

        let mut buffer = RestoreBuffer::new(&mapping);
        let mut out = String::new();
        for character in masked.chars() {
            out.push_str(&buffer.push(&character.to_string()).unwrap());
        }
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "Weber");
    }

    #[test]
    fn a_multibyte_character_survives_being_held() {
        // Slicing on bytes must never split a character in the held text.
        let mapping = mapped();
        let mut buffer = RestoreBuffer::new(&mapping);
        let mut out = String::new();
        out.push_str(&buffer.push("Grüße an [PERSON").unwrap());
        out.push_str(&buffer.push("_1]").unwrap());
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "Grüße an Weber");
    }
}
