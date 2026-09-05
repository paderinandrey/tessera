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
    #[error("upstream opened more runs of text than this gateway will hold; the stream ends")]
    TooManyRuns,
    #[error(
        "upstream sent an event this gateway cannot parse; the stream ends rather \
         than forwarding text it could not restore"
    )]
    Malformed,
}

impl StreamError {
    /// The fixed vocabulary the journal records. `Mapping`'s inner
    /// `MappingError::Unknown` carries a token and `Unplaceable` a run key;
    /// neither reaches the journal, only the class — and the token no longer
    /// reaches the client either, see `error_event`.
    ///
    /// **Matched with no `_` arm, including inside the two variants that wrap
    /// another enum.** That sentence stood here while `Mapping(_)` and
    /// `Shape(_)` were wildcards over eleven variants between them, which made
    /// it a claim about this enum's own seven and no more: measured at
    /// `41eb85e`, a probe variant added to `MappingError` produced two compile
    /// errors, both in `proxy.rs`, and this function took it silently as
    /// `stream_unrestorable`.
    ///
    /// **The coarseness was a defect, not a deliberate difference.** The
    /// argument for it is that the client's outcome is the same however a
    /// stream fails — bytes have already gone out, so the stream ends — and
    /// that is an argument about the response, not about the evidence. The
    /// journal exists to say what happened, and the buffered path gives these
    /// same errors eleven classes: an unresolvable placeholder, a placeholder
    /// used as a property name, and two of this gateway's own walks disagreeing
    /// are three different investigations, and they were one word.
    ///
    /// `stream_unrestorable` keeps `MappingError::Unknown`, which is the
    /// failure it was named for and the one that actually occurs here: a
    /// placeholder in the stream that no mapping resolves is precisely a
    /// restoration that cannot be done. Renaming it would renumber evidence
    /// already written for no gain.
    pub(crate) fn audit_class(&self) -> &'static str {
        match self {
            StreamError::Mapping(MappingError::Unknown(_)) => "stream_unrestorable",
            StreamError::Mapping(MappingError::BadSpan(_)) => "stream_bad_span",
            StreamError::Mapping(MappingError::TooDeep) => "stream_too_deep",
            StreamError::Mapping(MappingError::TooLarge) => "stream_too_large",
            StreamError::Mapping(MappingError::MaskCountMismatch(_)) => "stream_mask_mismatch",
            StreamError::Mapping(MappingError::PlaceholderKey(_)) => "stream_placeholder_key",
            // Not `stream_unrestorable_document`, though the variant is
            // `Unrestorable` and the parallel would be tidier. The short name
            // above is already this enum's word for `MappingError::Unknown`,
            // so a sibling one word longer would read as "the unknown-token
            // failure, in a document" to the only reader who matters here —
            // somebody holding a journal line and no source. What actually
            // happened is that restoring the document would have dropped a
            // member or renamed a key, so the class says that.
            StreamError::Mapping(MappingError::Unrestorable(_)) => "stream_lossy_document",
            StreamError::Shape(ShapeError::Request(_)) => "stream_shape_request",
            StreamError::Shape(ShapeError::Response(_)) => "stream_shape_response",
            StreamError::Shape(ShapeError::Pointer(_)) => "stream_shape_pointer",
            StreamError::Shape(ShapeError::Unsupported(_, _)) => "stream_shape_unsupported",
            StreamError::Shape(ShapeError::MalformedDocument(_, _)) => {
                "stream_tool_arguments_malformed"
            }
            StreamError::Unplaceable(_) => "stream_unplaceable",
            StreamError::Oversized => "stream_oversized",
            StreamError::Stalled => "stream_stalled",
            StreamError::TooManyRuns => "stream_too_many_runs",
            StreamError::Malformed => "stream_malformed",
        }
    }
}

/// How much of an unfinished event to hold. A response that never sends a blank
/// line would otherwise be buffered whole, which is the memory cost streaming
/// exists to avoid. Provider events are a few hundred bytes.
pub const MAX_EVENT_BYTES: usize = 1 << 20;

/// How much may wait behind an event that has not been released yet. A stream
/// that stalls after one delta and then sends keepalives forever would
/// otherwise grow without bound, which the per-event cap does not cover.
pub const MAX_QUEUED_BYTES: usize = 64 << 10;

/// How many runs of text may be open at once — choices on OpenAI, content
/// blocks on Anthropic. An upstream that keeps opening new indices and never
/// ends them would otherwise add a buffer per event for the life of the
/// response, which neither of the other caps covers.
pub const MAX_ACTIVE_RUNS: usize = 64;

/// A `[` that never closes would suspend the stream. Past this many bytes the
/// bracket cannot begin a placeholder, so it is emitted as ordinary text.
///
/// This is a bound on what the masker can issue, not a guess: a placeholder
/// carries a name from `mapping::ENTITY_TYPES`, which a test there holds to
/// `mapping::MAX_ENTITY_TYPE`, so releasing a bracket here can never orphan a
/// real token.
pub const MAX_HELD: usize = 64;

/// Restores placeholders in text arriving piece by piece. A placeholder
/// matching `[TYPE_N]` contains no `[`, so only the text from the last `[` with
/// no `]` after it can begin one; everything before that point is complete and
/// is emitted restored.
///
/// **What it does not do: the escaping rule.** `push` and `finish` call
/// `Mapping::restore`, which substitutes as text. The buffered path stopped
/// doing that — a `content` that is a serialized document, under
/// `response_format: json_object`, is restored structurally there so a value
/// carrying a `"` lands in a leaf instead of closing the string it was
/// substituted into. This buffer has no way to do the same. That protection is
/// a parse of the whole string; what arrives here is a fragment of one, and
/// `safe_prefix_len` holds text back only far enough not to split a
/// placeholder, so the boundary it releases on is a `[` and has no relation to
/// where a document begins or ends. Restoring the fragment `{"name":"` proves
/// nothing about the document it will become at the client.
///
/// **So the two paths differ here, and neither one of them decides it.** Both
/// halves are answered before the upstream call, by refusing a request shape:
///
/// - the `arguments` case — the one the recursion was written for — cannot
///   reach this buffer at all, because `reject_streamed_tools` refuses
///   `stream: true` on a request carrying tool traffic;
/// - a `content` the caller has declared will be a document cannot either,
///   because `reject_streamed_json_mode` refuses `stream: true` beside a
///   `response_format` of `json_object` or `json_schema` (#36).
///
/// Buffering a whole run before emitting any of it is the thing streaming
/// exists not to do, and teaching this buffer to track JSON structure across
/// fragments is a parser of our own beside the one `serde_json` already has, on
/// the path where a mistake is unrecoverable because the bytes have gone out.
/// Refusing the shape costs neither.
///
/// **And what this buffer now decides for itself.** It restores through
/// `Mapping::restore_in_stream`, which asks `json_string_inert` of every value
/// it substitutes and refuses one that could close a string *when a container
/// has been opened before the token* — `opened` below, the same question
/// `structure_encloses_a_token` asks of a whole string, carried across
/// fragments in a bool.
///
/// That was the thing thought impossible here, and the mistake was in the
/// inference: a delta cannot be **parsed**, which was read as "cannot decide".
/// "Was a bracket seen before this token" needs no parse, no lookahead and no
/// second copy of `serde_json` — only a fact about text already gone past,
/// which is exactly what a stream has.
///
/// It leaves prose alone, which is what a rule on the *value* alone could not
/// do: an apostrophe fails `json_string_inert`, so refusing on that would
/// refuse a streamed reply about anyone called `O'Brien`, and prose is most of
/// what streams. See #55, and `restore_in_stream` for the row-by-row
/// comparison with the buffered path — one row differs, and it is the one where
/// a parse would have let the buffered path succeed rather than refuse.
pub struct RestoreBuffer<'a> {
    mapping: &'a Mapping,
    held: String,
    /// Whether a `{` or `[` has gone past in this run's text, which is the one
    /// thing a stream can know about the document it may be in without parsing
    /// one. Carried across fragments, never reset, and scoped to this run
    /// because a buffer is — see `Mapping::restore_in_stream`.
    opened: bool,
}

impl<'a> RestoreBuffer<'a> {
    pub fn new(mapping: &'a Mapping) -> Self {
        Self {
            mapping,
            held: String::new(),
            opened: false,
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
            emitted.push_str(&self.mapping.restore_in_stream(&ready, &mut self.opened)?);
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
        self.mapping.restore_in_stream(&ready, &mut self.opened)
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

    /// Events framed from this chunk, and the failure that stopped framing if
    /// one did. The two are returned together: an oversized event does not
    /// unmake the complete ones that preceded it in the same chunk.
    pub fn push(&mut self, chunk: &[u8]) -> (Vec<SseEvent>, Option<StreamError>) {
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
                        return (events, Some(StreamError::Oversized));
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
                        return (events, Some(StreamError::Oversized));
                    }
                    self.scanned = self.buffer.len();
                    break;
                }
            }
        }
        (events, None)
    }

    /// Bytes left over when the body ended. A stream that stops without its
    /// final blank line must not swallow the text it already sent.
    pub fn finish(&mut self) -> Option<SseEvent> {
        self.scanned = 0;
        let block = std::mem::take(&mut self.buffer);
        parse_event(&block)
    }
}

/// Length of the line terminator starting at `index`, if one does. SSE allows
/// CR, LF and CRLF, and `\r\n` is one terminator rather than two.
fn terminator_len(buffer: &[u8], index: usize) -> Option<usize> {
    match buffer.get(index)? {
        b'\r' if buffer.get(index + 1) == Some(&b'\n') => Some(2),
        b'\r' | b'\n' => Some(1),
        _ => None,
    }
}

/// Offset and width of the first blank line — a terminator immediately followed
/// by another. Scanning forward matters: a `\r\n\r\n` delimiter contains no
/// `\n\n`, so searching for one convention across the whole buffer first would
/// run past a delimiter written in another and merge two events into one.
fn find_blank_line(buffer: &[u8], from: usize) -> Option<(usize, usize)> {
    for index in from..buffer.len() {
        let Some(first) = terminator_len(buffer, index) else {
            continue;
        };
        let next = index + first;
        let Some(second) = terminator_len(buffer, next) else {
            continue;
        };
        // A trailing lone `\r` may still turn out to be `\r\n`, which changes
        // the width by one but not where the block ends. Waiting for the byte
        // that decides it would delay every event on a CR-only stream; taking
        // the shorter reading leaves at worst a stray `\n` at the head of the
        // next block, which reads as an empty line and is skipped.
        return Some((index, first + second));
    }
    None
}

/// Split a block into lines on any of the three terminators.
fn lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut index) = (0, 0);
    while index < bytes.len() {
        match terminator_len(bytes, index) {
            Some(width) => {
                out.push(&text[start..index]);
                index += width;
                start = index;
            }
            None => index += 1,
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

fn parse_event(block: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(block);
    // A stream may open with a byte order mark. An SSE client ignores it, so a
    // `data:` line hidden behind one is still data — kept as an unknown line it
    // would be rendered back verbatim and reach the client unrestored.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut event = SseEvent::new(None, None);
    let mut data: Vec<&str> = Vec::new();
    let mut seen = false;
    for line in lines(text) {
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
        let (events, framing_error) = self.framer.push(chunk);
        let mut out = String::new();
        for event in events {
            match self.handle(event) {
                Ok(rendered) => out.push_str(&rendered),
                // Earlier events in this same chunk were rendered and are
                // correct. The failure stops the stream; it does not unmake them.
                Err(error) => return Err(self.keep(out, error)),
            }
        }
        match framing_error {
            Some(error) => Err(self.keep(out, error)),
            None => Ok(out),
        }
    }

    /// Set output aside for `salvage` and hand the failure on. It goes in front
    /// of anything already there: `out` is what was rendered before the
    /// operation that failed, and that operation may have stashed the event it
    /// was holding.
    fn keep(&mut self, out: String, error: StreamError) -> StreamError {
        self.salvage.insert_str(0, &out);
        error
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
            match self.handle(event) {
                Ok(rendered) => out.push_str(&rendered),
                Err(error) => return Err(self.keep(out, error)),
            }
        }
        match self.flush(&Terminates::All) {
            Ok(released) => {
                out.push_str(&self.release(released));
                Ok(out)
            }
            // The final flush can fail on a run it cannot place. What was
            // rendered before it is still correct.
            Err(error) => Err(self.keep(out, error)),
        }
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
        // Everything in the event that is not the streamed text is restored
        // whole, exactly as a pointer-less event is. The slot path rewrites the
        // deltas and nothing else, so any other string a provider puts here —
        // today or in a version written after this code — would otherwise be
        // forwarded verbatim, placeholder and all. Blanking the slots first
        // keeps their held-back text out of it: that text is the buffer's to
        // restore, and restoring it twice would be wrong.
        let mut scrubbed = parsed.clone();
        for slot in &slots {
            write_pointer(&mut scrubbed, &slot.pointer, "")?;
        }
        let mut rewritten = mapping.restore_value(&scrubbed)?;
        let mut carried = BTreeMap::new();
        for slot in &slots {
            // An upstream that keeps opening runs and never ends them would add
            // a buffer per event for the life of the response.
            if !self.buffers.contains_key(&slot.key) && self.buffers.len() >= MAX_ACTIVE_RUNS {
                return Err(StreamError::TooManyRuns);
            }
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
        // The event as it stands is already rewritten and correct. If a
        // remainder cannot be placed the stream ends, but that event was safe
        // before the remainder existed and is safe still.
        let without_remainders = pending.event.render();
        match place(&mut pending, remainders) {
            Ok(()) => Ok(pending.event.render()),
            Err(error) => {
                self.salvage.push_str(&without_remainders);
                Err(error)
            }
        }
    }
}

/// Append each run's remainder to the event waiting behind it — and only into an
/// event that carries that same run. With interleaved choices two completions
/// share a pointer, and writing into the wrong one would hand a client another
/// client's text.
fn place(pending: &mut Pending, remainders: Vec<(String, String)>) -> Result<(), StreamError> {
    let mut parsed: Value = serde_json::from_str(pending.event.data.as_deref().unwrap_or(""))
        .map_err(|_| StreamError::Unplaceable("the waiting event".to_owned()))?;
    for (key, rest) in remainders {
        let pointer = pending
            .slots
            .get(&key)
            .ok_or_else(|| StreamError::Unplaceable(key.clone()))?;
        let existing =
            read_pointer(&parsed, pointer).map_err(|_| StreamError::Unplaceable(key.clone()))?;
        write_pointer(&mut parsed, pointer, &format!("{existing}{rest}"))?;
    }
    pending.event.data = Some(parsed.to_string());
    Ok(())
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
    record: crate::audit::Record,
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
                    // Recorded before anything is yielded: a generator parked
                    // at a `yield` runs no further statement if it is dropped
                    // there, so a signal placed after the last one would never
                    // fire for a client that vanishes right as it arrives.
                    record.stream_failed("stream_broken");
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
                    record.stream_failed(error.audit_class());
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
            Ok(out) => {
                // Recorded before the yield for the same reason as the two
                // failure exits above: this is the only place a whole stream's
                // success is ever signalled.
                record.completed(200);
                if !out.is_empty() {
                    yield Ok::<_, Infallible>(Bytes::from(out));
                }
            }
            Err(error) => {
                record.stream_failed(error.audit_class());
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
/// out. The message names the failure and nothing else — not a value, and not
/// a placeholder either. This comment used to promise only the first: a
/// placeholder was judged safe to show because it is not the value it stood
/// for. It is still the gateway's own token, and a client is never otherwise
/// supposed to see one, so `MappingError::Unknown` no longer puts it in the
/// message. `Unplaceable` still names a run key, which is a position in the
/// provider's own envelope rather than anything of ours or the caller's.
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
mod audit_class_tests {
    use super::*;

    #[test]
    fn a_streamed_failure_is_recorded_as_precisely_as_a_buffered_one() {
        // I4. `Mapping(_)` and `Shape(_)` were wildcards over eleven variants,
        // so an unresolvable placeholder, a placeholder used as a property
        // name, a document past the depth bound and two of this gateway's own
        // walks disagreeing were one word — `stream_unrestorable` — for every
        // streamed response, while the buffered path gave the same errors
        // eleven classes. The doc comment above said the opposite, and a probe
        // variant on `MappingError` compiled here while failing in `proxy.rs`
        // twice.
        let classes = [
            StreamError::Mapping(MappingError::Unknown("[PERSON_1]".to_owned())).audit_class(),
            StreamError::Mapping(MappingError::BadSpan("overlapping")).audit_class(),
            StreamError::Mapping(MappingError::TooDeep).audit_class(),
            StreamError::Mapping(MappingError::TooLarge).audit_class(),
            StreamError::Mapping(MappingError::MaskCountMismatch("walks")).audit_class(),
            StreamError::Mapping(MappingError::PlaceholderKey("[PERSON_1]".to_owned()))
                .audit_class(),
            StreamError::Mapping(MappingError::Unrestorable("two members of the same name"))
                .audit_class(),
            StreamError::Shape(ShapeError::Request("messages")).audit_class(),
            StreamError::Shape(ShapeError::Response("choices")).audit_class(),
            StreamError::Shape(ShapeError::Pointer("/a".to_owned())).audit_class(),
            StreamError::Shape(ShapeError::Unsupported("openai", "logprobs")).audit_class(),
            StreamError::Shape(ShapeError::MalformedDocument("openai", "/a".to_owned()))
                .audit_class(),
            StreamError::Unplaceable("0".to_owned()).audit_class(),
            StreamError::Oversized.audit_class(),
            StreamError::Stalled.audit_class(),
            StreamError::TooManyRuns.audit_class(),
            StreamError::Malformed.audit_class(),
        ];
        let mut seen: Vec<&str> = classes.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            classes.len(),
            "two streamed failures an auditor must tell apart are one word: {classes:?}"
        );
        assert_eq!(
            StreamError::Mapping(MappingError::Unknown("[PERSON_1]".to_owned())).audit_class(),
            "stream_unrestorable",
            "the class this path was named for keeps its name, so evidence \
             already written still reads"
        );
    }
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
    fn an_oversized_event_does_not_unmake_the_one_before_it() {
        // The framer must hand back what it already framed, not only the failure.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let mut chunk =
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"good \"}}]}\n\ndata: "
                .to_vec();
        chunk.extend(std::iter::repeat_n(b'x', MAX_EVENT_BYTES + 1));
        chunk.extend_from_slice(b"\n\n");

        assert!(matches!(
            restorer.push(&chunk).unwrap_err(),
            StreamError::Oversized
        ));
        assert_eq!(text_for_choice(&restorer.salvage(), 0), "good ");
    }

    #[test]
    fn a_failing_final_flush_does_not_unmake_what_it_already_released() {
        // The last event arrives without its blank line, releasing the event
        // behind it; the flush then cannot place choice 0's tail. The released
        // text is still correct.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        restorer
            .push(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"A [PER\"}}]}\n\n")
            .unwrap();
        restorer
            .push(b"data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"B\"}}]}\n\n")
            .unwrap();
        // No trailing blank line: the framer only yields this on finish.
        restorer
            .push(b"data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"C\"}}]}")
            .unwrap();

        assert!(matches!(
            restorer.finish().unwrap_err(),
            StreamError::Unplaceable(_)
        ));
        assert_eq!(text_for_choice(&restorer.salvage(), 1), "BC");
    }

    #[test]
    fn endlessly_opening_new_runs_ends_the_stream() {
        // Neither the per-event cap nor the queue cap covers a buffer per index.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let mut error = None;
        for index in 0..(MAX_ACTIVE_RUNS + 2) {
            let event = format!(
                "data: {{\"choices\":[{{\"index\":{index},\"delta\":{{\"content\":\"[PER\"}}}}]}}\n\n"
            );
            if let Err(failure) = restorer.push(event.as_bytes()) {
                error = Some(failure);
                break;
            }
        }
        assert!(matches!(error, Some(StreamError::TooManyRuns)), "{error:?}");
    }

    #[test]
    fn runs_that_end_free_their_place() {
        // A long stream of blocks opened and closed in turn must not trip the cap.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        for index in 0..(MAX_ACTIVE_RUNS * 3) {
            let delta = format!(
                "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\
\"index\":{index},\"delta\":{{\"type\":\"text_delta\",\"text\":\"x\"}}}}\n\n"
            );
            restorer.push(delta.as_bytes()).unwrap();
            let stop = format!(
                "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\
\"index\":{index}}}\n\n"
            );
            restorer.push(stop.as_bytes()).unwrap();
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
    fn a_stream_opening_with_a_byte_order_mark_is_still_restored() {
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let body = "\u{feff}data: {\"choices\":[{\"index\":0,\"delta\":\
{\"content\":\"Hallo [PERSON_1]\"}}]}\n\ndata: [DONE]\n\n";
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());
        assert_eq!(text_for_choice(&rendered, 0), "Hallo Weber");
        assert!(!rendered.contains("PERSON_1"), "{rendered}");
    }

    #[test]
    fn a_sibling_field_of_the_delta_is_restored_too() {
        // The slot path rewrites the delta and nothing else. Anything else in
        // the event is restored whole, so a field this code has never heard of
        // cannot carry a placeholder out.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\
\"annotation\":\"about [PERSON_1]\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());
        assert!(rendered.contains("about Weber"), "{rendered}");
        assert!(!rendered.contains("PERSON_1"), "{rendered}");
    }

    #[test]
    fn restoring_the_rest_does_not_touch_the_held_back_text() {
        // The delta's own text belongs to the buffer; restoring it here as well
        // would emit a token the hold-back was still assembling.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a [PER\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"SON_1] b\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());
        assert_eq!(text_for_choice(&rendered, 0), "a Weber b");
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
        let (events, error) = framer.push(chunk);
        assert!(error.is_none(), "framing failed: {error:?}");
        events
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
    fn every_line_ending_convention_frames_the_same_events() {
        // SSE allows CR, LF and CRLF, and a blank line is any two in a row.
        for (label, body) in [
            ("lf", "data: one\n\ndata: two\n\n".to_owned()),
            ("cr", "data: one\r\rdata: two\r\r".to_owned()),
            ("crlf", "data: one\r\n\r\ndata: two\r\n\r\n".to_owned()),
            ("mixed", "data: one\n\r\ndata: two\r\n\n".to_owned()),
        ] {
            let mut framer = SseFramer::new();
            let events = unwrap_push(&mut framer, body.as_bytes());
            assert_eq!(events.len(), 2, "{label}");
            assert_eq!(events[0].data.as_deref(), Some("one"), "{label}");
            assert_eq!(events[1].data.as_deref(), Some("two"), "{label}");
        }
    }

    #[test]
    fn cr_separated_fields_inside_one_event_are_read() {
        let mut framer = SseFramer::new();
        let events = unwrap_push(&mut framer, b"event: ping\rdata: {}\r\r");
        assert_eq!(events[0].name.as_deref(), Some("ping"));
        assert_eq!(events[0].data.as_deref(), Some("{}"));
    }

    #[test]
    fn a_delimiter_split_across_chunks_leaves_no_stray_bytes() {
        // `\r\r\n` arriving a piece at a time: whichever reading the framer
        // takes, no byte of the delimiter may end up inside an event.
        let mut framer = SseFramer::new();
        let mut events = unwrap_push(&mut framer, b"data: one\r");
        events.extend(unwrap_push(&mut framer, b"\r"));
        events.extend(unwrap_push(&mut framer, b"\ndata: two\n\n"));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data.as_deref(), Some("one"));
        assert_eq!(events[1].data.as_deref(), Some("two"));
    }

    #[test]
    fn a_cr_stream_a_byte_at_a_time_frames_the_same() {
        let body = "data: one\r\rdata: two\r\r";
        let mut framer = SseFramer::new();
        let mut events = Vec::new();
        for byte in body.as_bytes() {
            events.extend(unwrap_push(&mut framer, &[*byte]));
        }
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].data.as_deref(), Some("two"));
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
    fn a_byte_order_mark_does_not_hide_the_first_data_line() {
        // Behind a BOM the line is still data; treating it as an unknown field
        // would render it back untouched.
        let mut framer = SseFramer::new();
        let events = unwrap_push(&mut framer, "\u{feff}data: {\"a\":1}\n\n".as_bytes());
        assert_eq!(events[0].data.as_deref(), Some("{\"a\":1}"));
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

    /// A mapping whose one value carries a character that can close a string.
    /// `O'Brien` rather than `x","admin":true` on purpose: the apostrophe is
    /// what makes this an ordinary name and not a crafted payload, and it fails
    /// `json_string_inert` exactly as a quote does because a permissive reader
    /// honours it exactly as well.
    fn mapped_to_a_non_inert_value() -> Mapping {
        let mut mapping = Mapping::new();
        mapping
            .mask(
                "O'Brien",
                &[Span {
                    entity_type: "PERSON".into(),
                    start: 0,
                    end: 7,
                }],
            )
            .unwrap();
        mapping
    }

    #[test]
    fn a_value_that_could_close_a_string_is_refused_inside_a_streamed_structure() {
        // #55. The streamed path substituted as text unconditionally, so this
        // wrote `{"name":"O'Brien"}` into the client's document with the
        // apostrophe closing the string it landed in — and the bytes were gone
        // before anything could reconsider. A delta cannot be parsed, which was
        // read as "cannot decide", but "was a container opened before this
        // token" needs no parse.
        let mapping = mapped_to_a_non_inert_value();
        let mut buffer = RestoreBuffer::new(&mapping);
        let error = buffer
            .push(r#"{"name":"[PERSON_1]"}"#)
            .expect_err("a non-inert value inside an opened structure must not be substituted");
        assert!(matches!(error, MappingError::Unrestorable(_)), "{error:?}");
    }

    #[test]
    fn the_structure_and_the_token_may_arrive_in_different_fragments() {
        // The whole reason this is a flag and not a scan: the `{` and the token
        // land in separate deltas, so a rule that looked only at the fragment
        // in hand would see `[PERSON_1]"}` with nothing opened and substitute.
        let mapping = mapped_to_a_non_inert_value();
        let mut buffer = RestoreBuffer::new(&mapping);
        assert_eq!(buffer.push(r#"{"name":"#).unwrap(), r#"{"name":"#);
        let error = buffer
            .push(r#""[PERSON_1]"}"#)
            .expect_err("the container opened two fragments ago still encloses this token");
        assert!(matches!(error, MappingError::Unrestorable(_)), "{error:?}");
    }

    #[test]
    fn the_same_value_in_prose_is_substituted() {
        // The row that keeps this from being "refuse every apostrophe". No
        // container was opened, so there is no string for the value to close,
        // and prose is most of what streams.
        let mapping = mapped_to_a_non_inert_value();
        let mut buffer = RestoreBuffer::new(&mapping);
        let mut out = buffer.push("Guten Tag [PERSON_1], ").unwrap();
        out.push_str(&buffer.push("wie geht es Ihnen?").unwrap());
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "Guten Tag O'Brien, wie geht es Ihnen?");
    }

    #[test]
    fn an_inert_value_inside_a_structure_is_substituted() {
        // The other row that keeps the rule narrow: a container is not the
        // problem, a value that can escape its string is. `Weber` cannot.
        let mapping = mapped();
        let mut buffer = RestoreBuffer::new(&mapping);
        let mut out = buffer.push(r#"{"name":"[PERSON_1]"}"#).unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, r#"{"name":"Weber"}"#);
    }

    #[test]
    fn a_container_opened_after_the_token_does_not_enclose_it() {
        // `structure_encloses_a_token` asks at the token and not after the
        // loop, and this carries that reading across fragments: a `{` that
        // arrives later says nothing about a value already emitted.
        //
        // **Both fragments in one push**, deliberately. Split across two, this
        // passes under a rule that pre-scans each fragment for brackets before
        // restoring it — a mutation that survived when the test was written
        // that way, because the two pieces never shared a run. One push is what
        // makes the *position within the run* the thing being asserted.
        let mapping = mapped_to_a_non_inert_value();
        let mut buffer = RestoreBuffer::new(&mapping);
        let mut out = buffer.push(r#"[PERSON_1] wrote {"a":1} and "#).unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, r#"O'Brien wrote {"a":1} and "#);

        // And once it has been opened it stays opened, in the same run: a
        // second token after the brace is refused.
        let mut buffer = RestoreBuffer::new(&mapping);
        let error = buffer
            .push(r#"a {"b":1} then [PERSON_1]"#)
            .expect_err("a container opened earlier in the same run still encloses this token");
        assert!(matches!(error, MappingError::Unrestorable(_)), "{error:?}");
    }

    #[test]
    fn a_value_the_buffered_allowlist_calls_dangerous_still_streams() {
        // **The test that separates the two predicates**, and the measurement
        // that made it necessary. A first version of this rule reused
        // `json_string_inert`, the buffered path's allowlist. On the public
        // corpus that rejects 3.1% of annotated values — and the offending
        // characters are `/` and `&`: every German tax number, whose canonical
        // form is `419/130/29933`, and company names like
        // `Boerner AG & Co. KGaA`.
        //
        // Neither can close a string in any reader. The allowlist excludes them
        // because on the buffered path being wrong costs a parse, which that
        // path was happy to do — its own comment prices `/` that way, "it came
        // out because it was cheap". Here being wrong costs a killed stream, so
        // the question is the narrower one: `can_leave_a_string`.
        //
        // Under the allowlist this test fails on both values.
        for value in ["419/130/29933", "Boerner AG & Co. KGaA"] {
            let mut mapping = Mapping::new();
            mapping
                .mask(
                    value,
                    &[Span {
                        entity_type: "ORG".into(),
                        start: 0,
                        end: value.chars().count(),
                    }],
                )
                .unwrap();
            let mut buffer = RestoreBuffer::new(&mapping);
            let mut out = buffer.push(r#"{"x":"[ORG_1]"}"#).unwrap();
            out.push_str(&buffer.finish().unwrap());
            assert_eq!(
                out,
                format!(r#"{{"x":"{value}"}}"#),
                "a value that cannot close a string ended the stream"
            );
        }
    }

    #[test]
    fn every_way_out_of_a_string_is_refused() {
        // The other side: the enumeration this rule is built from — a
        // delimiter, the escape, a character the format forbids raw — asserted
        // member by member, so a narrowing that drops one is a failing test
        // rather than a silent injection.
        for value in ["a\"b", "a'b", "a`b", "a\\b", "a\nb", "a\u{1}b"] {
            let mut mapping = Mapping::new();
            mapping
                .mask(
                    value,
                    &[Span {
                        entity_type: "ORG".into(),
                        start: 0,
                        end: value.chars().count(),
                    }],
                )
                .unwrap();
            let mut buffer = RestoreBuffer::new(&mapping);
            assert!(
                buffer.push(r#"{"x":"[ORG_1]"}"#).is_err(),
                "a value carrying {value:?} was substituted into a streamed structure"
            );
        }
    }

    #[test]
    fn one_run_s_structure_does_not_bind_another() {
        // The flag is per `RestoreBuffer`, and `stream::handle` keys one per
        // text run — the granularity at which the buffered path restores a
        // slot. A `{` in one choice's content must not refuse a token in
        // another's.
        let mapping = mapped_to_a_non_inert_value();
        let mut first = RestoreBuffer::new(&mapping);
        assert!(first.push(r#"{"name":"[PERSON_1]"}"#).is_err());
        let mut second = RestoreBuffer::new(&mapping);
        assert_eq!(second.push("hallo [PERSON_1]").unwrap(), "hallo O'Brien");
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
        //
        // The type is the longest one the gateway *declares*: an invented name
        // of any length masks as [REDACTED_n] now, so measuring one would
        // measure the fallback and say nothing about the bound.
        let entity_type = crate::mapping::ENTITY_TYPES
            .iter()
            .max_by_key(|entity_type| entity_type.len())
            .expect("the vocabulary is not empty");
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask(
                "Weber",
                &[Span {
                    entity_type: (*entity_type).to_owned(),
                    start: 0,
                    end: 5,
                }],
            )
            .unwrap();
        // The number is a usize, and this mapping issued 1. The placeholder the
        // cap has to survive is the one a long-running session issues, so the
        // widest a counter can print is what is measured.
        let widest = "[_]".len() + entity_type.len() + usize::MAX.to_string().len();
        assert!(widest <= MAX_HELD, "{widest} bytes for [{entity_type}_N]");

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
