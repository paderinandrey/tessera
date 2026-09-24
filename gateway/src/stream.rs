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
use crate::provider::{read_pointer, write_pointer, Provider, Run, ShapeError, Terminates};

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
        "upstream sent a tool-argument document larger than this gateway will \
         accumulate; the stream ends"
    )]
    ToolDocumentTooLarge,
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
            // Unreachable on this path — nothing masks on the way back — but
            // matched rather than wildcarded, which is the rule this function
            // states about itself and the reason it caught a probe variant
            // silently before.
            StreamError::Mapping(MappingError::LiteralAlreadyIssued(_)) => {
                "stream_literal_already_issued"
            }
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
            StreamError::ToolDocumentTooLarge => "stream_tool_document_too_large",
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

/// How much of one tool-argument document to accumulate before giving up on it.
///
/// A tool block is held whole rather than streamed: half a document is not a
/// document, so there is no safe prefix to release and the usual hold-back
/// buffer does not apply. That makes it the one run here with no natural
/// ceiling — `MAX_EVENT_BYTES` bounds a single event and `MAX_QUEUED_BYTES`
/// bounds what waits behind one, and a document spread thinly across many
/// events is neither.
///
/// 256 KiB is above what a model can emit in one block: it is roughly 64k
/// tokens, which is the output ceiling of the models this gateway is pointed
/// at. **Worst case is this times `MAX_ACTIVE_RUNS`** — 16 MiB of accumulator
/// for one response that opens every run it is allowed and closes none. That is
/// the price of holding documents whole, and it is bounded, which streaming
/// them is not.
pub const MAX_TOOL_DOCUMENT_BYTES: usize = 256 << 10;

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
/// halves are answered before this buffer sees them, and no longer by the same
/// means:
///
/// - the `arguments` case — the one the recursion was written for — never
///   reaches this buffer, because tool arguments open a `Held::Document` run
///   instead. They are accumulated whole and restored structurally when the
///   run closes, which is the same door the buffered path uses (#87). It used
///   to be `reject_streamed_tools` refusing the request outright, and that
///   function no longer exists;
/// - a `content` the caller has declared will be a document cannot reach it
///   either, because `reject_streamed_json_mode` refuses `stream: true` beside
///   a `response_format` of `json_object` or `json_schema` (#36).
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
    /// What this run has gone past — a container opened, a string entered —
    /// which is what a stream can know about the document it may be in without
    /// parsing one. Carried across fragments and scoped to this run because a
    /// buffer is. See `Mapping::restore_in_stream` and `StreamStructure`.
    structure: crate::mapping::StreamStructure,
}

impl<'a> RestoreBuffer<'a> {
    /// A buffer for a caller that declared nothing.
    ///
    /// `#[cfg(test)]` because production always has an answer — `declared`
    /// returns `Unknown` for a request with no header, so the serving path
    /// passes a format either way. This is the convenience for the several
    /// dozen tests that are about something else.
    #[cfg(test)]
    pub fn new(mapping: &'a Mapping) -> Self {
        Self::declaring(mapping, crate::mapping::ClientFormat::Unknown)
    }

    /// A buffer for a response whose caller has said how it will read it.
    ///
    /// `new` is `Unknown`, which is the strict rule, so every existing caller
    /// and every test that does not care is unchanged — the widening is opt-in
    /// by construction rather than by a default somebody has to remember to
    /// override.
    pub fn declaring(mapping: &'a Mapping, format: crate::mapping::ClientFormat) -> Self {
        Self {
            mapping,
            held: String::new(),
            structure: crate::mapping::StreamStructure::declaring(format),
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
            emitted.push_str(
                &self
                    .mapping
                    .restore_in_stream(&ready, &mut self.structure)?,
            );
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
        self.mapping.restore_in_stream(&ready, &mut self.structure)
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
    /// Threaded to every buffer this restorer opens. Set once from the request
    /// and never from anything the response says.
    format: crate::mapping::ClientFormat,
    /// Hands out `Held::Document::group`. One value per event that opens
    /// document runs, so the runs one chunk opened share a carrier and are
    /// rendered together.
    groups: u64,
}

/// A run in progress, and where its event wrote it last.
///
/// The two kinds differ in what can safely be served before the run ends. A
/// `Text` run has a safe prefix — everything before the last unclosed `[` — and
/// streams it. A `Document` run has none: half a JSON document is not a
/// document, so nothing of it goes out until its block closes and it can be
/// parsed and restored structurally.
enum Held<'a> {
    Text {
        buffer: RestoreBuffer<'a>,
        pointer: String,
    },
    Document {
        /// The fragments as the upstream sent them, unrestored. A placeholder
        /// may be split across two of them, which is half the reason this path
        /// could not substitute as it went.
        raw: String,
        pointer: String,
        /// Which runs share this carrier, because one event opened them all.
        ///
        /// OpenAI opens parallel calls in a single chunk, so that chunk — and
        /// so the carrier — describes every one of them: their `id`, `type` and
        /// `function.name` together. Rendering a carrier per run replays each
        /// call's identity once per sibling, and a client that concatenates a
        /// call's string fields across chunks reads the name `firstfirst` and
        /// dispatches to nothing. So the group is rendered once, with every
        /// member's document written into it.
        group: u64,
        /// The most recent event of this run, envelope already restored and its
        /// fragment blanked. The whole document is written into it when the
        /// block closes, so the client receives one event carrying the lot
        /// rather than a synthesized one this module would have to invent a
        /// shape for.
        carrier: SseEvent,
    },
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
    /// A restorer for a caller that declared nothing — see `RestoreBuffer::new`
    /// for why this is test-only.
    #[cfg(test)]
    pub fn new(provider: &'a dyn Provider, mapping: &'a Mapping) -> Self {
        Self::declaring(provider, mapping, crate::mapping::ClientFormat::Unknown)
    }

    /// A restorer for a response whose caller has said how it will read it.
    pub fn declaring(
        provider: &'a dyn Provider,
        mapping: &'a Mapping,
        format: crate::mapping::ClientFormat,
    ) -> Self {
        Self {
            provider,
            mapping,
            framer: SseFramer::new(),
            buffers: BTreeMap::new(),
            pending: None,
            queued: Vec::new(),
            queued_bytes: 0,
            salvage: String::new(),
            format,
            groups: 0,
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
    /// contain the very token that could not be restored — and so are the
    /// document accumulators, which hold fragments that were never restored at
    /// all and would carry placeholders out verbatim.
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
        let provider = self.provider.name();

        // **A fragment of a tool-argument document, which is not restored
        // here.** A delta is not a document, so there is nothing to parse at
        // the moment of substitution and nothing of it is safe to serve; it is
        // accumulated and restored whole when `content_block_stop` closes this
        // index. The envelope is restored now, because it is this event's own
        // and the accumulator has no use for it.
        if slots.iter().any(|slot| slot.run == Run::Document) {
            // **One event may carry several documents and must not mix kinds.**
            // OpenAI streams parallel tool calls as one `tool_calls` array, so
            // two fragments of two different documents arrive together and both
            // have to accumulate. Text in the same event is the shape that
            // cannot be served: it would have to stream now while the documents
            // are held back, and then the event that finally carries a document
            // would carry that text a second time.
            //
            // **This is event-wide rather than per choice, and that is the
            // stricter reading on purpose.** With `n > 1` one could imagine a
            // chunk whose first choice emits `content` while its second emits
            // `tool_calls`. It is not a shape this protocol produces — each
            // chunk carries one choice at array position 0, which
            // `interleaved_choices_get_distinct_keys_at_the_same_position` and
            // the key design itself both rest on — so scoping the check per
            // choice would buy nothing reachable and would cost the simple
            // rule that an event is emitted once or held once. If OpenAI ever
            // batches choices, this refuses rather than emitting the text twice
            // or dropping a document, which is the side to be wrong on.
            if slots.iter().any(|slot| slot.run == Run::Text) {
                return Err(ShapeError::Response(provider).into());
            }
            // Blanked once, for **every** document in the event rather than the
            // one being handled: each run keeps this as its carrier, and a
            // carrier still holding a sibling's fragment would emit that
            // fragment again when this run closes.
            let mut scrubbed = parsed.clone();
            for slot in &slots {
                write_pointer(&mut scrubbed, &slot.pointer, "")?;
            }
            let mut carrier = event;
            carrier.data = Some(mapping.restore_value(&scrubbed)?.to_string());

            // One group for every run this event opens. A run it merely
            // continues keeps the group it was opened with, so a chunk opening
            // one call beside an older one does not drag the older one into
            // this event's carrier.
            let group = self.groups;
            let mut opened = false;

            for slot in &slots {
                let fragment = read_pointer(&parsed, &slot.pointer)?;
                let carrier = carrier.clone();

                match self.buffers.get_mut(&slot.key) {
                    Some(Held::Document { raw, .. }) => {
                        if raw.len() + fragment.len() > MAX_TOOL_DOCUMENT_BYTES {
                            return Err(StreamError::ToolDocumentTooLarge);
                        }
                        raw.push_str(&fragment);
                        // **The carrier and its pointer are the run's first
                        // event, and they are not replaced.** On OpenAI a
                        // call's identity — `id`, `type` and `function.name` —
                        // arrives in the same chunk as its first `arguments`
                        // fragment and never again, so a run that kept its
                        // latest event handed the client a tool call with no
                        // name to dispatch on. Measured: the document restored
                        // correctly and the call was unusable.
                        //
                        // Nothing in a later fragment is lost by this. OpenAI's
                        // carry `index` and `arguments` alone; Anthropic's
                        // carry an envelope identical to the first. The pointer
                        // stays with the carrier because it addresses *that*
                        // event — a call's array position can differ between
                        // chunks, so the latest pointer need not resolve in the
                        // first event.
                    }
                    // One key, two kinds. The upstream changed what a run is
                    // mid-flight, and neither reading of the fragments is right.
                    Some(Held::Text { .. }) => return Err(ShapeError::Response(provider).into()),
                    None => {
                        if self.buffers.len() >= MAX_ACTIVE_RUNS {
                            return Err(StreamError::TooManyRuns);
                        }
                        if fragment.len() > MAX_TOOL_DOCUMENT_BYTES {
                            return Err(StreamError::ToolDocumentTooLarge);
                        }
                        opened = true;
                        self.buffers.insert(
                            slot.key.clone(),
                            Held::Document {
                                raw: fragment,
                                pointer: slot.pointer.clone(),
                                group,
                                carrier,
                            },
                        );
                    }
                }
            }
            if opened {
                self.groups += 1;
            }
            return Ok(String::new());
        }

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
        let format = self.format;
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
                .or_insert_with(|| Held::Text {
                    buffer: RestoreBuffer::declaring(mapping, format),
                    pointer: slot.pointer.clone(),
                });
            let Held::Text { buffer, pointer } = held else {
                // See the same arm above: one key cannot be both kinds of run.
                return Err(ShapeError::Response(provider).into());
            };
            pointer.clone_from(&slot.pointer);
            let safe = buffer.push(&text)?;
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
            Terminates::Under(prefixes) => prefixes.iter().any(|p| key.starts_with(p)),
            Terminates::Nothing => false,
        };
        let mapping = self.mapping;
        let provider = self.provider.name();
        let mut remainders: Vec<(String, String)> = Vec::new();
        // Documents that closed with this event, rendered whole. They go out
        // behind whatever was already waiting and in front of the event that
        // ended them, which is the order the upstream sent their fragments in.
        let mut documents = String::new();
        for (key, held) in self.buffers.iter_mut() {
            if !ends(key) {
                continue;
            }
            if let Held::Text { buffer, .. } = held {
                let rest = buffer.finish()?;
                if !rest.is_empty() {
                    remainders.push((key.clone(), rest));
                }
            }
        }

        // **Documents are drained by group, not by run.** One OpenAI chunk
        // opens every parallel call at once, so those runs share a carrier that
        // describes all of them — their `id`, `type` and `function.name`
        // together. A carrier rendered per run replays each call's identity
        // once per sibling, and a client that concatenates a call's string
        // fields across chunks reads the name `firstfirst` and dispatches to
        // nothing. So a group is rendered once, with every member's document
        // written into it at its own pointer.
        let mut groups: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        let mut group_of: BTreeMap<String, u64> = BTreeMap::new();
        for (key, held) in self.buffers.iter() {
            if let Held::Document { group, .. } = held {
                group_of.insert(key.clone(), *group);
                if ends(key) {
                    groups.entry(*group).or_default().push(key.clone());
                }
            }
        }
        for (group, members) in &groups {
            // **A group ends whole or the stream ends.** Rendering the carrier
            // for some members now and the rest later would replay the
            // identities this grouping exists to send once.
            //
            // **Unreachable with both protocols as they stand, and untested for
            // that reason** — Anthropic opens one document slot per event, so
            // its groups are single runs, and OpenAI's `finish_reason` ends
            // every run under its choice in one flush. A group cannot span two
            // choices either, for the same reason the check above is event-wide:
            // one chunk carries one choice, so the runs an event opens are all
            // that choice's and all end together. It is here because the
            // grouping is what makes it unreachable: a provider added later
            // that closes calls one at a time would meet this instead of
            // silently replaying a name, and the direction of that failure is
            // the one this module takes everywhere else.
            let whole = group_of
                .iter()
                .filter(|(_, g)| *g == group)
                .all(|(key, _)| members.contains(key));
            if !whole {
                return Err(StreamError::Unplaceable(members[0].clone()));
            }
            let mut data: Option<Value> = None;
            let mut event: Option<SseEvent> = None;
            for key in members {
                let Some(Held::Document {
                    raw,
                    pointer,
                    carrier,
                    ..
                }) = self.buffers.get(key)
                else {
                    continue;
                };
                // **What says a document is complete is its own run closing,
                // not its text parsing.** A run still held when the message
                // ends never saw the event that closes it, and
                // `Terminates::All` — `message_stop`, `[DONE]`, or an `error`
                // the upstream sent mid-generation — is not that signal for any
                // of them. Truncation usually leaves JSON that will not parse
                // and the refusal below would catch it, but *usually* is the
                // whole objection: the one truncation that happens to parse
                // would be served as a finished tool call, and a tool call is
                // an action the client's agent takes rather than text it
                // displays. So the test is the signal itself.
                if matches!(terminates, Terminates::All) {
                    return Err(ShapeError::MalformedDocument(provider, pointer.clone()).into());
                }
                // **The parse is a round trip, and a round trip can lose.** The
                // buffered path learned this: re-serializing a document
                // collapses two members of the same name into one and respells
                // a number the parse does not reproduce, and the result is a
                // tool call the client's agent executes with arguments the
                // model did not write. It answers with the same two rules, and
                // so does this.
                //
                // **Nothing to restore, so nothing to re-serialize.** Most
                // arguments carry no token of ours at all, and those go back
                // byte for byte however their numbers are spelled — no parse,
                // no round trip, no question.
                // **Parsed first, and the decision is made on the parse.**
                // `carries_a_placeholder` over the raw text misses a token the
                // model escaped: `{"name":"[PERSON_\u0031]"}` holds no literal
                // `[PERSON_1]` in its bytes, and the client's own parser
                // reconstructs one — so a gate reading the bytes forwarded our
                // token instead of restoring it. Measured.
                //
                // Parsing is not what the byte-for-byte path avoids;
                // *re-serializing* is. So the parse decides and the raw bytes
                // are still what goes out when there is nothing to put back.
                let document: Value = serde_json::from_str(raw)
                    .map_err(|_| ShapeError::MalformedDocument(provider, pointer.clone()))?;
                // **The union of both readings, because each sees a token the
                // other cannot.** The parse reveals one the model escaped —
                // `[PERSON_\u0031]` is not in the bytes and is in the value.
                // The raw text reveals one the parse *discards*: with two
                // members of the same name serde keeps the last, so a token in
                // the first is gone from the document while still being in the
                // document the client would have read. Either sighting means
                // this is not a document to pass through untouched.
                let token_in_bytes = crate::mapping::carries_a_placeholder(raw);
                let token_in_value = !crate::mapping::placeholder_literals(&document).is_empty();
                let served = if token_in_bytes || token_in_value {
                    // There is a token to put back, so this document *will* be
                    // re-serialized — and a round trip that loses is refused
                    // rather than served changed, exactly as `write_document`'s
                    // caller refuses it on the buffered path.
                    // The round-trip scan reads the *text*, because duplicate
                    // members are invisible once parsed.
                    if let Some(cause) = crate::mapping::round_trip_loses(raw) {
                        return Err(MappingError::Unrestorable(cause).into());
                    }
                    mapping.restore_value(&document)?.to_string()
                } else {
                    raw.clone()
                };
                let into = match &mut data {
                    Some(value) => value,
                    None => {
                        event = Some(carrier.clone());
                        data.insert(
                            serde_json::from_str(carrier.data.as_deref().unwrap_or(""))
                                .map_err(|_| StreamError::Malformed)?,
                        )
                    }
                };
                write_pointer(into, pointer, &served)?;
            }
            if let (Some(mut event), Some(data)) = (event, data) {
                event.data = Some(data.to_string());
                documents.push_str(&event.render());
            }
        }
        self.buffers.retain(|key, _| !ends(key));

        let Some(mut pending) = self.pending.take() else {
            // Restored text with nowhere to go is not dropped quietly.
            if let Some((key, _)) = remainders.first() {
                return Err(StreamError::Unplaceable(key.clone()));
            }
            return Ok(documents);
        };
        if remainders.is_empty() {
            let mut out = pending.event.render();
            out.push_str(&documents);
            return Ok(out);
        }
        // The event as it stands is already rewritten and correct. If a
        // remainder cannot be placed the stream ends, but that event was safe
        // before the remainder existed and is safe still — and so is any
        // document that closed alongside it.
        let without_remainders = pending.event.render();
        match place(&mut pending, remainders) {
            Ok(()) => {
                let mut out = pending.event.render();
                out.push_str(&documents);
                Ok(out)
            }
            Err(error) => {
                self.salvage.push_str(&without_remainders);
                self.salvage.push_str(&documents);
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
    format: crate::mapping::ClientFormat,
    record: crate::audit::Record,
) -> Response {
    let body = async_stream::stream! {
        let mut upstream = response.bytes_stream();
        let mut restorer = StreamRestorer::declaring(provider, &mapping, format);
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
            StreamError::Mapping(MappingError::LiteralAlreadyIssued("[PERSON_1]".to_owned()))
                .audit_class(),
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
            StreamError::ToolDocumentTooLarge.audit_class(),
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
    use serde_json::json;

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

    /// One SSE event, named and carrying `data`.
    fn sse(event: &str, data: &str) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    /// The tool document the client reassembles for one block: every
    /// `partial_json` fragment it was sent, in order, concatenated.
    fn anthropic_tool_json(rendered: &str, index: u64) -> String {
        let mut out = String::new();
        for line in rendered.split('\n') {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if value.get("index").and_then(Value::as_u64) != Some(index) {
                continue;
            }
            if let Some(fragment) = value.pointer("/delta/partial_json").and_then(Value::as_str) {
                out.push_str(fragment);
            }
        }
        out
    }

    /// A mapping whose one value carries a `"`. Substituted as text into a tool
    /// document it would close the string it lands in; restored structurally it
    /// is escaped on the way out.
    fn mapped_quoting() -> Mapping {
        let mut mapping = Mapping::new();
        mapping
            .mask(
                "Weber \"Bo\" AG",
                &[Span {
                    entity_type: "PERSON".into(),
                    start: 0,
                    end: 13,
                }],
            )
            .unwrap();
        mapping
    }

    /// The events of one tool block, with its argument document cut into the
    /// given fragments.
    fn tool_block(index: u64, fragments: &[&str]) -> String {
        let mut body = sse(
            "content_block_start",
            &format!(
                "{{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":\
                 {{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"send_mail\",\
                 \"input\":{{}}}}}}"
            ),
        );
        for fragment in fragments {
            let escaped = Value::String((*fragment).to_owned()).to_string();
            body.push_str(&sse(
                "content_block_delta",
                &format!(
                    "{{\"type\":\"content_block_delta\",\"index\":{index},\"delta\":\
                     {{\"type\":\"input_json_delta\",\"partial_json\":{escaped}}}}}"
                ),
            ));
        }
        body
    }

    fn block_stop(index: u64) -> String {
        sse(
            "content_block_stop",
            &format!("{{\"type\":\"content_block_stop\",\"index\":{index}}}"),
        )
    }

    #[test]
    fn a_tool_document_split_across_deltas_is_restored_whole() {
        // The reason the streamed path refused tool traffic at all: a
        // placeholder is split across two `input_json_delta`s *and* lands
        // inside a half-written JSON value at the same time, so there is
        // nothing to parse at the moment of substitution. Accumulating the
        // block and restoring it when it closes answers both at once.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let body = tool_block(0, &["{\"note\":\"[PER", "SON_1]\"}"]) + &block_stop(0);
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());

        let document: Value = serde_json::from_str(&anthropic_tool_json(&rendered, 0))
            .expect("the client's reassembled document must parse");
        assert_eq!(document, json!({"note": "Weber"}));
        assert!(
            !rendered.contains("[PERSON_1]") && !rendered.contains("[PER\\\""),
            "a fragment of the token reached the client: {rendered}"
        );
    }

    #[test]
    fn a_restored_value_that_could_close_a_string_is_escaped_into_the_tool_document() {
        // What restoring a document buys over substituting into a fragment, and
        // the whole reason this path refused rather than substituted. `Weber
        // "Bo" AG` written as text into `{"note":"[PERSON_1]"}` closes `note`
        // and puts `Bo` where the client's agent reads a member.
        //
        // **The escaping is `restore_in_string_strictly`'s, not
        // serialization's** — `restore_value` reaches it for every string leaf,
        // and it carries the rule precisely because a leaf can itself be a
        // serialized document. Measured by mutation: making this an
        // `input_json_delta` a `Run::Text` instead sends the fragments through
        // `RestoreBuffer`, which substitutes leniently, and this assertion is
        // the one that fails. Flattening the document and restoring *that*
        // string does not fail it, because it is the same rule one level up.
        use crate::provider::Anthropic;
        let mapping = mapped_quoting();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let body = tool_block(0, &["{\"note\":\"[PERSON_1]\"}"]) + &block_stop(0);
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());

        let document: Value = serde_json::from_str(&anthropic_tool_json(&rendered, 0))
            .expect("the client's reassembled document must parse");
        assert_eq!(document, json!({"note": "Weber \"Bo\" AG"}));
        assert_eq!(
            document.as_object().expect("an object").len(),
            1,
            "the value added a member: {document}"
        );
    }

    #[test]
    fn an_unclosed_tool_block_releases_nothing() {
        // The accumulator is not a hold-back buffer with a safe prefix: half a
        // document is not a document, and no prefix of it is safe to serve. A
        // block the upstream never closes ends the stream with none of it
        // written.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let body = tool_block(0, &["{\"note\":\"[PERSON_1]"]);
        let rendered = restorer.push(body.as_bytes()).unwrap();
        assert_eq!(
            anthropic_tool_json(&rendered, 0),
            "",
            "a fragment was served before the block closed: {rendered}"
        );
        let finished = restorer.finish();
        assert!(
            matches!(
                finished,
                Err(StreamError::Shape(ShapeError::MalformedDocument(
                    "anthropic",
                    _
                )))
            ),
            "an unclosed tool document was not refused as one: {finished:?}"
        );
    }

    #[test]
    fn one_block_index_cannot_be_both_kinds_of_run() {
        // Both arms of the `Held` mismatch, which nothing else reaches. A
        // `content_block_start` of type `text` opens a run that streams; an
        // `input_json_delta` at the same index then claims that run is a
        // document. Whichever the upstream meant, one of the two readings is
        // wrong about every fragment already handled — the text run has served
        // its safe prefix, and a document has none — so there is no reading to
        // continue with.
        use crate::provider::Anthropic;
        let mapping = mapped();

        let mut text_first = StreamRestorer::new(&Anthropic, &mapping);
        let outcome = text_first.push(
            (sse(
                "content_block_delta",
                "{\"type\":\"content_block_delta\",\"index\":0,\"delta\":\
                 {\"type\":\"text_delta\",\"text\":\"Hallo\"}}",
            ) + &tool_block(0, &["{\"note\":\"x\"}"]))
                .as_bytes(),
        );
        assert!(
            matches!(
                outcome,
                Err(StreamError::Shape(ShapeError::Response("anthropic")))
            ),
            "a document claimed a text run: {outcome:?}"
        );

        let mut document_first = StreamRestorer::new(&Anthropic, &mapping);
        let outcome = document_first.push(
            (tool_block(0, &["{\"note\":"])
                + &sse(
                    "content_block_delta",
                    "{\"type\":\"content_block_delta\",\"index\":0,\"delta\":\
                     {\"type\":\"text_delta\",\"text\":\"Hallo\"}}",
                ))
                .as_bytes(),
        );
        assert!(
            matches!(
                outcome,
                Err(StreamError::Shape(ShapeError::Response("anthropic")))
            ),
            "a text run claimed a document: {outcome:?}"
        );
    }

    #[test]
    fn a_tool_block_the_message_ended_without_closing_is_not_served() {
        // The sharper half of `an_unclosed_tool_block_releases_nothing`, and
        // the case that one passes without testing. There the truncation left
        // JSON that will not parse, so the refusal could come from the parse
        // and the guard would never be exercised. Here the fragments stop at a
        // point where they *do* parse — `{"note":"x"}` is a whole document —
        // and `message_stop` arrives with the block still open.
        //
        // Serving it would hand the client's agent a finished tool call the
        // model never finished writing. What says a document is complete is
        // `content_block_stop` at its own index; JSON validity is a different
        // question that happens to agree most of the time.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let body = tool_block(0, &["{\"note\":\"x\"}"])
            + &sse("message_stop", "{\"type\":\"message_stop\"}");
        let outcome = restorer.push(body.as_bytes());
        assert!(
            matches!(
                outcome,
                Err(StreamError::Shape(ShapeError::MalformedDocument(
                    "anthropic",
                    _
                )))
            ),
            "a tool call the message never closed was served: {outcome:?}"
        );
    }

    #[test]
    fn a_tool_document_that_does_not_parse_ends_the_stream() {
        // Serving it unrestored would hand the client `[PERSON_1]`, and
        // restoring it as text is the substitution this whole path exists to
        // avoid. Neither is served.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let body = tool_block(0, &["{\"note\": \"[PERSON_1]\" ,,}"]) + &block_stop(0);
        let outcome = restorer.push(body.as_bytes());
        assert!(
            matches!(
                outcome,
                Err(StreamError::Shape(ShapeError::MalformedDocument(
                    "anthropic",
                    _
                )))
            ),
            "an unparseable tool document was not refused as one: {outcome:?}"
        );
    }

    #[test]
    fn a_tool_document_the_round_trip_would_change_is_refused() {
        // The rule the buffered path already had, arriving here late. Restoring
        // means re-serializing, and a re-serialization collapses two members of
        // the same name into one. The client'"'"'s agent then executes a call whose
        // arguments the model did not write — silently, because the document it
        // receives is well formed.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let body = tool_block(0, &["{\"to\":\"[PERSON_1]\",\"to\":\"second\"}"]) + &block_stop(0);
        let outcome = restorer.push(body.as_bytes());
        assert!(
            matches!(
                outcome,
                Err(StreamError::Mapping(MappingError::Unrestorable(_)))
            ),
            "a document the round trip changes was served: {outcome:?}"
        );
    }

    #[test]
    fn an_escaped_placeholder_is_still_restored() {
        // The hole the byte-for-byte path opened. A model that JSON-escapes any
        // character of a token — `[PERSON_\u0031]` — leaves bytes that hold no
        // literal `[PERSON_1]`, while the client's own parser reconstructs one.
        // A gate reading the bytes therefore forwarded this gateway's token to
        // the client instead of restoring the value, in a tool argument their
        // agent dispatches on.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let body = tool_block(0, &["{\"to\":\"[PERSON_\\u0031]\"}"]) + &block_stop(0);
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());

        let document: Value = serde_json::from_str(&anthropic_tool_json(&rendered, 0))
            .expect("the client's reassembled document must parse");
        assert_eq!(document, json!({"to": "Weber"}));
        assert!(
            !rendered.contains("PERSON_1") && !rendered.contains("0031"),
            "the token reached the client, escaped or otherwise: {rendered}"
        );
    }

    #[test]
    fn a_tool_document_with_nothing_to_restore_goes_back_byte_for_byte() {
        // No token of ours in it, so there is nothing to put back and no reason
        // to parse it — and a document that is never parsed is never
        // re-serialized, so its duplicate members and its 20-digit number
        // survive exactly as the model wrote them. Most tool calls are this
        // one, which is why refusing on the round trip alone would have been
        // far too broad.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let awkward = "{\"id\":12345678901234567890,\"to\":\"a\",\"to\":\"b\"}";
        let body = tool_block(0, &[awkward]) + &block_stop(0);
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());
        assert_eq!(
            anthropic_tool_json(&rendered, 0),
            awkward,
            "the document was round-tripped though nothing needed restoring"
        );
    }

    #[test]
    fn a_tool_document_past_the_bound_ends_the_stream() {
        // An accumulator is unbounded by nature: the per-event cap does not
        // cover a document spread across many events, and `MAX_QUEUED_BYTES`
        // does not either, because these deltas are suppressed rather than
        // queued.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let filler = "x".repeat(MAX_TOOL_DOCUMENT_BYTES + 1);
        let body = tool_block(0, &[&format!("{{\"note\":\"{filler}\"}}")]);
        let outcome = restorer.push(body.as_bytes());
        assert!(
            matches!(outcome, Err(StreamError::ToolDocumentTooLarge)),
            "an unbounded tool document was accumulated"
        );
    }

    #[test]
    fn a_placeholder_in_a_tool_document_key_ends_the_stream() {
        // `restore_value`'s rule, on the path that had not reached it. Keys are
        // never masked going up, so a placeholder in key position is the model
        // writing one it saw in the text; restoring it renames the property the
        // client's tool reads its argument from, and leaving it hands our own
        // token over. Both change dispatch.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let body = tool_block(0, &["{\"[PERSON_1]\":\"x\"}"]) + &block_stop(0);
        let outcome = restorer.push(body.as_bytes());
        assert!(
            matches!(
                outcome,
                Err(StreamError::Mapping(MappingError::PlaceholderKey(_)))
            ),
            "a placeholder in key position was not refused as one: {outcome:?}"
        );
    }

    #[test]
    fn a_text_block_and_a_tool_block_are_restored_independently() {
        // One message carries both, and they are separate runs: the text block
        // streams as it always did while the tool block accumulates, and
        // neither drains the other. `content_block_stop` ends its own index —
        // the rule `stopping_one_block_leaves_another_block_held` already
        // pinned, now with a run of each kind in flight.
        use crate::provider::Anthropic;
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&Anthropic, &mapping);
        let mut body = sse(
            "content_block_delta",
            "{\"type\":\"content_block_delta\",\"index\":0,\"delta\":\
             {\"type\":\"text_delta\",\"text\":\"Hallo [PER\"}}",
        );
        body.push_str(&tool_block(1, &["{\"note\":\"[PERSON_1]\"}"]));
        body.push_str(&block_stop(1));
        body.push_str(&sse(
            "content_block_delta",
            "{\"type\":\"content_block_delta\",\"index\":0,\"delta\":\
             {\"type\":\"text_delta\",\"text\":\"SON_1]!\"}}",
        ));
        body.push_str(&block_stop(0));
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());

        assert_eq!(anthropic_text(&rendered), "Hallo Weber!");
        let document: Value = serde_json::from_str(&anthropic_tool_json(&rendered, 1))
            .expect("the client's reassembled document must parse");
        assert_eq!(document, json!({"note": "Weber"}));
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
    fn a_streamed_tool_call_is_restored_when_its_choice_finishes() {
        // What #87's OpenAI half is for. The token is split across two
        // `arguments` fragments *and* sits inside a half-written JSON value, so
        // there is nothing to parse at the moment of substitution — and unlike
        // Anthropic there is no event that closes the call. `finish_reason` in
        // a later chunk is the only close there is, and it names no tool.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
             \"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"send\",\
             \"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
             \"function\":{\"arguments\":\"{\\\"to\\\":\\\"[PER\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\
             \"function\":{\"arguments\":\"SON_1]\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());

        let document: Value = serde_json::from_str(&openai_tool_json(&rendered, 0))
            .expect("the client's reassembled document must parse");
        assert_eq!(document, json!({"to": "Weber"}));
        assert!(
            !rendered.contains("[PERSON_1]") && !rendered.contains("[PER"),
            "a fragment of the token reached the client: {rendered}"
        );
        assert!(
            rendered.contains("call_1") && rendered.contains("\"send\""),
            "the call's identity did not reach the client: {rendered}"
        );
    }

    #[test]
    fn two_tool_calls_in_one_chunk_do_not_splice() {
        // OpenAI streams parallel calls as one `tool_calls` array, so two
        // fragments of two different documents arrive in the same event and
        // both accumulate. The carrier each run keeps has **every** document
        // blanked, not just its own — a carrier still holding a sibling's
        // fragment would emit that fragment again when this run closed, and the
        // client would reassemble one call's argument into the other.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let body = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[\
             {\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"first\",\
             \"arguments\":\"{\\\"to\\\":\\\"[PER\"}},\
             {\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"second\",\
             \"arguments\":\"{\\\"cc\\\":\\\"[PER\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[\
             {\"index\":0,\"function\":{\"arguments\":\"SON_1]\\\"}\"}},\
             {\"index\":1,\"function\":{\"arguments\":\"SON_1]\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        );
        let mut rendered = restorer.push(body.as_bytes()).unwrap();
        rendered.push_str(&restorer.finish().unwrap());

        let first: Value = serde_json::from_str(&openai_tool_json(&rendered, 0))
            .expect("the first call's document must parse");
        let second: Value = serde_json::from_str(&openai_tool_json(&rendered, 1))
            .expect("the second call's document must parse");
        assert_eq!(first, json!({"to": "Weber"}));
        assert_eq!(second, json!({"cc": "Weber"}));

        // **And each call's identity reaches the client exactly once.** A
        // client concatenates every string field of a call across chunks, so a
        // name delivered twice is `firstfirst` and dispatches to nothing. Each
        // run keeping its own clone of a carrier that describes *both* calls
        // replays the other's `id` and `name` when it closes — which the
        // argument assertions above cannot see, because they only read
        // `arguments`.
        for name in ["first", "second"] {
            assert_eq!(
                rendered.matches(&format!("\"{name}\"")).count(),
                1,
                "the call name {name} reached the client more than once: {rendered}"
            );
        }
        for id in ["\"a\"", "\"b\""] {
            assert_eq!(
                rendered.matches(id).count(),
                1,
                "the call id {id} reached the client more than once: {rendered}"
            );
        }
    }

    #[test]
    fn a_choice_that_finishes_does_not_end_another_choices_tool_call() {
        // The prefix has a separator for a reason. `choice/1/` must not reach
        // `choice/10/tool/0`, and this is the end-to-end half of the assertion
        // the provider test makes on `Terminates` alone.
        let mapping = mapped();
        let mut restorer = StreamRestorer::new(&OpenAi, &mapping);
        let open = |choice: u64, fragment: &str| {
            format!(
                "data: {}\n\n",
                json!({"choices": [{"index": choice, "delta": {"tool_calls": [
                    {"index": 0, "id": "x", "function": {"name": "f", "arguments": fragment}}]}}]})
            )
        };
        // Choice 1's document is whole, so finishing it must serve it. Choice
        // 10's is a fragment, so if the prefix reached it the flush would fail
        // here instead of later — which is the other half of what this checks.
        let mut body = open(1, "{\"to\":\"[PERSON_1]\"}");
        body.push_str(&open(10, "{\"to\":\"[PER"));
        // Choice 1 finishes; choice 10 has not, and its run must survive.
        body.push_str(&format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 1, "delta": {}, "finish_reason": "tool_calls"}]})
        ));
        let rendered = restorer.push(body.as_bytes()).unwrap();
        assert!(
            rendered.contains("Weber"),
            "choice 1's call was not restored when it finished: {rendered}"
        );
        // Choice 10's document never closed, so finishing the stream refuses it
        // rather than serving half a document — which is also the proof that it
        // was still open rather than drained by choice 1's prefix.
        let finished = restorer.finish();
        assert!(
            matches!(
                finished,
                Err(StreamError::Shape(ShapeError::MalformedDocument(
                    "openai",
                    _
                )))
            ),
            "choice 10's run was ended by choice 1's finish_reason: {finished:?}"
        );
    }

    /// The tool document the client reassembles for one call index.
    fn openai_tool_json(rendered: &str, tool: u64) -> String {
        let mut out = String::new();
        for line in rendered.split('\n') {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            let Some(choices) = event.get("choices").and_then(Value::as_array) else {
                continue;
            };
            for choice in choices {
                let Some(calls) = choice
                    .pointer("/delta/tool_calls")
                    .and_then(Value::as_array)
                else {
                    continue;
                };
                for call in calls {
                    if call.get("index").and_then(Value::as_u64) != Some(tool) {
                        continue;
                    }
                    if let Some(piece) = call.pointer("/function/arguments").and_then(Value::as_str)
                    {
                        out.push_str(piece);
                    }
                }
            }
        }
        out
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

    /// A mapping whose one value carries the delimiter of the string it will be
    /// substituted into.
    ///
    /// **It used to be `O'Brien`, and the change is the point.** An apostrophe
    /// cannot close a double-quoted string in any reader this is written for,
    /// and the earlier rule refused it anyway — it knew a string was in play and
    /// not which kind. Once the lexer tracks which delimiter opened the string,
    /// the ordinary case goes through and the test has to carry a real hazard.
    fn mapped_to_a_non_inert_value() -> Mapping {
        let value = r#"Weber" and ""#;
        let mut mapping = Mapping::new();
        mapping
            .mask(
                value,
                &[Span {
                    entity_type: "PERSON".into(),
                    start: 0,
                    end: value.chars().count(),
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
        assert_eq!(out, r#"Guten Tag Weber" and ", wie geht es Ihnen?"#);
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
        assert_eq!(out, r#"Weber" and " wrote {"a":1} and "#);

        // And while it is *open* it encloses what follows, in the same run.
        // This used to use `{"b":1}` — closed — and asserted a refusal, which
        // was the flag never coming down rather than the property it names. A
        // closed container returns to prose now, and the container here stays
        // open so the assertion is about enclosure again.
        let mut buffer = RestoreBuffer::new(&mapping);
        let error = buffer
            .push(r#"a {"b":1, "c": [PERSON_1]"#)
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
        // The enumeration this rule is built from — a delimiter, the escape, a
        // character the format forbids raw — asserted member by member, so a
        // narrowing that drops one is a failing test rather than an injection.
        //
        // **Each delimiter against its own kind of string.** They are not
        // interchangeable: an apostrophe is a literal inside `"…"` and a quote
        // is a literal inside `'…'`, and refusing either everywhere is what made
        // an ordinary Irish surname end a stream.
        for (delimiter, carrier) in [
            ('"', r#"{"x":"[ORG_1]"}"#),
            ('\'', "{x:'[ORG_1]'}"),
            ('`', "({x:`[ORG_1]`})"),
        ] {
            let value = format!("a{delimiter}b");
            let mapping = mapped_to(&[(value.as_str(), "ORG")]);
            let mut buffer = RestoreBuffer::new(&mapping);
            assert!(
                buffer.push(carrier).is_err(),
                "{value:?} was substituted into {carrier}"
            );

            // And it is inert inside a string of another kind.
            let elsewhere = if delimiter == '"' {
                "{x:'[ORG_1]'}"
            } else {
                r#"{"x":"[ORG_1]"}"#
            };
            let mut buffer = RestoreBuffer::new(&mapping);
            assert!(
                buffer.push(elsewhere).is_ok(),
                "{value:?} was refused inside {elsewhere}, which it cannot close"
            );
        }

        // The escape and the forbidden-raw characters close any string.
        for value in ["a\\\\b", "a\nb", "a\u{1}b", "a\u{2028}b", "a\u{2029}b"] {
            let mapping = mapped_to(&[(value, "ORG")]);
            let mut buffer = RestoreBuffer::new(&mapping);
            assert!(
                buffer.push(r#"{"x":"[ORG_1]"}"#).is_err(),
                "a value carrying {value:?} was substituted into a streamed structure"
            );
        }
    }

    fn mapped_to(values: &[(&str, &str)]) -> Mapping {
        let mut mapping = Mapping::new();
        for (value, entity_type) in values {
            mapping
                .mask(
                    value,
                    &[Span {
                        entity_type: (*entity_type).into(),
                        start: 0,
                        end: value.chars().count(),
                    }],
                )
                .unwrap();
        }
        mapping
    }

    #[test]
    fn a_bracket_a_value_restores_to_opens_a_structure_too() {
        // **The hole the first version left, and it needs no crafted input to
        // reach — only two ordinary detections.** Text runs updated the flag and
        // substituted values did not, so a value restoring to `{` emitted an
        // opener nothing recorded, and the next token substituted freely into
        // the object the first one had just opened. Mapped values carry no
        // character restriction at all. Found in review of #57.
        // **No quotes anywhere in the carrier text**, deliberately. A first
        // draft used `[ORG_1]"name":"[PERSON_2]"}`, where the run between the
        // tokens carries an odd number of quotes and sets `in_string` on its
        // own — so it passed with the value's bracket ignored, and a mutation
        // removing exactly this line survived it. The bracket has to be the only
        // opener for the test to be about the bracket.
        let mapping = mapped_to(&[("{", "ORG"), ("a'b", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&mapping);
        let error = buffer
            .push("[ORG_1] then [PERSON_2]")
            .expect_err("the first value opened the structure the second is written into");
        assert!(matches!(error, MappingError::Unrestorable(_)), "{error:?}");

        // And without that first value it is prose, which is what keeps this
        // about the opener rather than about the second value.
        let prose = mapped_to(&[("a'b", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&prose);
        assert_eq!(buffer.push("then [PERSON_1]").unwrap(), "then a'b");
    }

    #[test]
    fn a_top_level_string_is_a_document_without_a_container() {
        // A streamed reply whose whole content is `"[PERSON_1]"` is a valid JSON
        // document — a top-level string — with no `{` or `[` anywhere. The
        // buffered path escapes it because `serde_json` parses a bare string as
        // a document; this path saw no container and substituted raw, producing
        // `"Martina "Weber""` while reporting success. Found in review of #57.
        let mapping = mapped_to(&[(r#"Martina "Weber""#, "PERSON")]);
        let mut buffer = RestoreBuffer::new(&mapping);
        let error = buffer
            .push(r#""[PERSON_1]""#)
            .expect_err("an unbalanced quote puts the token inside a string");
        assert!(matches!(error, MappingError::Unrestorable(_)), "{error:?}");

        // And a *balanced* one does not: prose that quoted something earlier is
        // still prose, and this is where the rule stops.
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(buffer.push(r#"she said "hello" to [PERSON_1]"#).is_ok());
    }

    #[test]
    fn a_value_that_could_leave_a_comment_is_refused() {
        // **Seeing a container does not prove the token is inside a string.**
        // `{/* [PERSON_1] */ safe:true}` is valid JSON5 with the token inside a
        // comment, and `*/ admin:true, /*` passes `can_leave_a_string` because
        // `/` and `*` are inert — and have to be: a German tax number is
        // `419/130/29933`. Found in review of #57.
        let mapping = mapped_to(&[("*/ admin:true, /*", "ORG")]);
        let mut buffer = RestoreBuffer::new(&mapping);
        let error = buffer
            .push("{/* [ORG_1] */ safe:true}")
            .expect_err("the value closes the comment it was substituted into");
        assert!(matches!(error, MappingError::Unrestorable(_)), "{error:?}");

        // The two characters apart are not the hazard, and refusing them would
        // refuse every German tax number and half the company names.
        let inert = mapped_to(&[("419/130/29933", "DE_STEUERNUMMER")]);
        let mut buffer = RestoreBuffer::new(&inert);
        let mut out = buffer.push(r#"{"tax":"[DE_STEUERNUMMER_1]"}"#).unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, r#"{"tax":"419/130/29933"}"#);
    }

    #[test]
    fn a_unicode_line_terminator_is_refused() {
        // U+2028 and U+2029 are line terminators to a JSON5 reader and forbidden
        // raw inside a string, and Rust's `is_control` covers neither. The
        // detector normalizes both while keeping offsets, which is how one
        // reaches a restored value. Found in review of #57.
        for separator in ['\u{2028}', '\u{2029}'] {
            let value = format!("Weber{separator}Martina");
            let mapping = mapped_to(&[(value.as_str(), "PERSON")]);
            let mut buffer = RestoreBuffer::new(&mapping);
            assert!(
                buffer.push(r#"{"name":"[PERSON_1]"}"#).is_err(),
                "U+{:04X} was substituted into a streamed structure",
                separator as u32
            );
        }
    }

    #[test]
    fn a_bare_value_position_needs_no_hazardous_character() {
        // **The finding that ended the character-blocklist argument.**
        // `{safe:false,value:[ORG_1]}` is valid JSON5 with the token in an
        // unquoted member position, and `null,admin:true,pad:null` adds a member
        // out of alphanumerics and punctuation that must stay inert — an e-mail
        // needs `@`, a date needs `:`. No blocklist closes this; only knowing
        // the token is *not* inside a string does. Found in review of #64.
        let mapping = mapped_to(&[("null,admin:true,pad:null", "ORG")]);
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(buffer.push("{safe:false,value:[ORG_1]}").is_err());

        // And a name in the same position still goes through, which is what
        // stops this being "refuse everything once a brace appears".
        let plain = mapped_to(&[("Martina Weber", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&plain);
        let mut out = buffer.push(r#"{"name":"[PERSON_1]"}"#).unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, r#"{"name":"Martina Weber"}"#);
    }

    #[test]
    fn an_escape_split_across_two_fragments_is_still_an_escape() {
        // A fragment boundary is not a token boundary. One push ending `"foo\`
        // and the next beginning `"` is an *escaped* quote; recreating the
        // escape state per run read it as a closing one, left the string, and
        // let a quote-bearing value through. Found in review of #64.
        let mapping = mapped_to(&[(r#"Martina "Weber""#, "PERSON")]);
        let mut buffer = RestoreBuffer::new(&mapping);
        assert_eq!(buffer.push(r#""foo\"#).unwrap(), r#""foo\"#);
        assert!(
            buffer.push(r#""[PERSON_1]"#).is_err(),
            "the escaped quote did not close the string, so the token is still inside it"
        );
    }

    #[test]
    fn a_quote_inside_a_comment_does_not_open_a_string() {
        // `/* " */ "[PERSON_1]"` — counting quotes without knowing about
        // comments makes the one in the comment cancel the real one, so the
        // token reads as unenclosed. Found in review of #64.
        let mapping = mapped_to(&[(r#"Martina "Weber""#, "PERSON")]);
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(buffer.push(r#"/* " */ "[PERSON_1]""#).is_err());
    }

    #[test]
    fn a_single_quoted_string_is_a_string() {
        // `'[PERSON_1]'` is a JSON5 document with no container and no double
        // quote, and `can_leave_a_string` calls the apostrophe hazardous — so
        // the enclosure state had to cover every delimiter the hazard names.
        // Found in review of #64.
        let mapping = mapped_to(&[("O'Brien", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(buffer.push("'[PERSON_1]'").is_err());

        // The other delimiter is inert inside this one: an apostrophe cannot
        // close a double-quoted string, and refusing it there would refuse the
        // ordinary case.
        let mut buffer = RestoreBuffer::new(&mapping);
        let mut out = buffer.push(r#"{"name":"[PERSON_1]"}"#).unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, r#"{"name":"O'Brien"}"#);
    }

    #[test]
    fn a_comment_at_the_top_level_is_still_a_comment() {
        // The comment check used to require a container. `/* [ORG_1] */` with
        // `*/ {"admin":true} /*` becomes a valid JSON5 document injecting an
        // object the model wrote only inside a comment. Found in review of #64.
        let mapping = mapped_to(&[(r#"*/ {"admin":true} /*"#, "ORG")]);
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(buffer.push("/* [ORG_1] */").is_err());
    }

    #[test]
    fn a_comment_delimiter_assembled_across_the_boundary_is_refused() {
        // `x*` followed by the carrier's `/` is the same `*/`. Checking the
        // value alone missed it, so inside a comment either character is
        // enough — a masked value is not something to serve inside a comment
        // anyway. Found in review of #64.
        let mapping = mapped_to(&[("x*", "ORG")]);
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(buffer
            .push("{/* [ORG_1]/ admin:true, /* fallback */ safe:false}")
            .is_err());
    }

    #[test]
    fn an_interpolation_opener_is_refused_where_it_can_act() {
        // The `${` hazard used to sit in the string rule and is deleted: a
        // backtick region is judged by the bare rule now, so the string arm is
        // only ever `"` or `'`, where `${` is inert for every parser in the
        // stated model — and the check still refused `Account ${name}` inside
        // `{"label":"…"}`.
        //
        // **This test used to pass without the check**, which is why it survived
        // a round: the backtick carrier is refused by the region, not by the
        // hazard. It asserts both halves now.
        let interpolating = mapped_to(&[("${globalThis.process.exit()}", "ORG")]);
        let mut buffer = RestoreBuffer::new(&interpolating);
        assert!(
            buffer.push("({message:`[ORG_1]`})").is_err(),
            "a backtick region takes word characters only"
        );

        // And a value that merely looks like interpolation, inside a real JSON
        // string, is data.
        let label = mapped_to(&[("Account ${name}", "ORG")]);
        let mut buffer = RestoreBuffer::new(&label);
        let mut out = buffer.push(r#"{"label":"[ORG_1]"}"#).unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, r#"{"label":"Account ${name}"}"#);
    }

    #[test]
    fn a_bracket_inside_a_quoted_value_is_not_a_structural_opener() {
        // **A delimiter inside a substituted value is only structural where it
        // lands.** `Acme [Europe]` restored into a quoted position carries a
        // bracket, and counting it as an opener would arm the bare-position rule
        // for the rest of the run — so the ordinary name two words later would
        // end the stream for a bracket that was inside a string.
        //
        // The lexer gets this for nothing, because it folds the value in at the
        // place the value lands: inside `Place::Text` a bracket falls through
        // like any other character. The flag-based version it replaced did not,
        // and this test is that finding kept as a property rather than as
        // history — raised in review of the commit before the lexer.
        let mapping = mapped_to(&[("Acme [Europe]", "ORG"), ("O'Brien", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&mapping);
        let mut out = buffer
            .push(r#"She called "[ORG_1]" and [PERSON_2] replied"#)
            .expect("a bracket inside a quoted value is literal, not an opener");
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, r#"She called "Acme [Europe]" and O'Brien replied"#);
    }

    /// One adversarial case: what the model streamed, what the detector mapped,
    /// whether the stream must end, and the sentence that says why.
    struct Case {
        carrier: &'static str,
        values: &'static [(&'static str, &'static str)],
        refuses: bool,
        why: &'static str,
    }

    #[test]
    fn the_lexer_survives_being_attacked() {
        // **Written to break it, not to confirm it**, after #65 recorded three
        // doubts about the design.
        //
        // Two were unfounded, and one for an interesting reason: a `*/` split
        // between two adjacent values cannot reach the one-character window,
        // because refusing either `*` or `/` inside a comment stops the first
        // value before the second arrives.
        //
        // **The third was right and this test said otherwise.** "A closed
        // container does not return to prose" was asserted here as though it
        // were a property, when it was the cost of a flag that never came down —
        // and it refused a name after any JSON snippet in a reply. Nesting is
        // counted now and the case is inverted, with an open container beside it
        // so the refusal is still pinned.
        //
        // The last case is not a defect. A bare position takes word characters
        // and an ampersand is not one, so a company name there ends the stream —
        // recorded as the cost of the rule rather than left to be discovered.
        let cases = [
            Case {
                carrier: "{/* [ORG_1][ORG_2] */ x:1}",
                values: &[("a*", "ORG"), ("/b", "ORG")],
                refuses: true,
                why: "a comment closed by two values meeting",
            },
            Case {
                carrier: r#"{"a":"/*","b":"[PERSON_1]"}"#,
                values: &[(r#"x","admin":true,"p":"y"#, "PERSON")],
                refuses: true,
                why: "a comment opener inside a string is a literal",
            },
            Case {
                carrier: "{// \" \n x:[ORG_1]}",
                values: &[("null,admin:true", "ORG")],
                refuses: true,
                why: "a quote in a line comment does not open a string",
            },
            Case {
                carrier: r#"{"a":1} then [PERSON_1]"#,
                values: &[("null,admin:true", "PERSON")],
                refuses: false,
                why: "a closed container returns to prose, and prose refuses nothing",
            },
            Case {
                carrier: r#"{"a":1, "b": [PERSON_1]"#,
                values: &[("null,admin:true", "PERSON")],
                refuses: true,
                why: "an open container is still a bare position",
            },
            Case {
                carrier: r#"{"a":"x\\","b":"[PERSON_1]"}"#,
                values: &[(r#"y","admin":true,"p":"z"#, "PERSON")],
                refuses: true,
                why: "an escaped backslash does not escape the quote after it",
            },
            Case {
                carrier: "{x:[ORG_1] admin:true */ y:1}",
                values: &[("1 /*", "ORG")],
                refuses: true,
                why: "a value opening a comment the carrier closes",
            },
            Case {
                carrier: "Guten Tag [PERSON_1], wie geht es?",
                values: &[("O'Brien", "PERSON")],
                refuses: false,
                why: "prose is prose",
            },
            Case {
                carrier: r#"{"name":"[PERSON_1]"}"#,
                values: &[("O'Brien", "PERSON")],
                refuses: false,
                why: "an apostrophe cannot close a double-quoted string",
            },
            Case {
                carrier: r#"{"tax":"[DE_STEUERNUMMER_1]"}"#,
                values: &[("419/130/29933", "DE_STEUERNUMMER")],
                refuses: false,
                why: "slashes are inert inside a string",
            },
            Case {
                carrier: "{org:[ORG_1]}",
                values: &[("Boerner AG & Co", "ORG")],
                refuses: true,
                why: "a bare position takes word characters",
            },
            Case {
                carrier: "{org: *[ORG_1]}",
                values: &[("victim", "ORG")],
                refuses: false,
                why: "an indicator the carrier wrote is not something a rule about values reaches — #80",
            },
            Case {
                carrier: "{tax:[DE_STEUERNUMMER_1]}",
                values: &[("419/130/29933", "DE_STEUERNUMMER")],
                refuses: true,
                why: "a bare position takes word characters, and a tax number is not one",
            },
            Case {
                carrier: "{mail:[EMAIL_1]}",
                values: &[("uschihiller@example.org", "EMAIL")],
                refuses: true,
                why: "the same, and it is the cost #69 is open about",
            },
            Case {
                carrier: "``[ORG_1]``",
                values: &[("Boerner AG & Co", "ORG")],
                refuses: true,
                why: "a region says nothing about what will read it, and `&` is a YAML anchor",
            },
            Case {
                carrier: "{/* [ORG_1] */ a:1}",
                values: &[("uschihiller@example.org", "EMAIL")],
                refuses: true,
                why: "a comment the lexer is in may be one the parser is not",
            },
        ];
        for case in cases {
            let mapping = mapped_to(case.values);
            let mut buffer = RestoreBuffer::new(&mapping);
            let outcome = buffer.push(case.carrier).and_then(|mut out| {
                buffer.finish().map(|tail| {
                    out.push_str(&tail);
                    out
                })
            });
            assert_eq!(
                outcome.is_err(),
                case.refuses,
                "{}: {} produced {outcome:?}",
                case.why,
                case.carrier
            );
        }
    }

    #[test]
    fn a_caller_s_own_token_does_not_open_a_structure() {
        // `reserve_literals` maps a caller's own `[PERSON_1]` to itself, so
        // restoring it emits exactly the bytes that were already there — and
        // `pieces` never showed those bytes to the lexer on the way in, because
        // it yields a token as its own piece. Counting them on the way out made
        // restoration change a document restoration did not touch: this prose
        // opened a bare position on the first token and refused an ordinary
        // name on the second.
        //
        // **It is the traffic the reserve-literals mechanism exists for** (#32):
        // a templating client, or one echoing an earlier turn. Found by
        // attacking the lexer rather than by review, which is the only finding
        // in this file that arrived that way.
        let mut mapping = Mapping::new();
        mapping
            .reserve_literals("[PERSON_1]")
            .expect("a literal no allocation holds reserves");
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
        let mut buffer = RestoreBuffer::new(&mapping);
        let mut out = buffer
            .push("the caller wrote [PERSON_1] and then [PERSON_2] replied")
            .expect("a token restored to itself changes nothing about the document");
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "the caller wrote [PERSON_1] and then O'Brien replied");
    }

    #[test]
    fn a_value_that_is_not_the_token_still_counts() {
        // The other half, so the exception above cannot widen into "values
        // never open anything". A value that genuinely restores to a bracket is
        // an opener exactly as a bracket in the model's own text is.
        let mapping = mapped_to(&[("[", "ORG"), ("null,admin:true", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(buffer.push("[ORG_1] then [PERSON_2]").is_err());
    }

    #[test]
    fn a_self_mapped_token_still_advances_the_lexer() {
        // **Skipping it entirely left the state mid-character.** A backslash
        // before the token stayed pending, so the quote *after* it was consumed
        // as escaped, the string never closed, and the next token read as quoted
        // content — where `null,admin:true` is inert, so it went through and
        // added a member. Found in review of #66, against the exception that
        // pull request introduced two commits earlier.
        let mut mapping = Mapping::new();
        mapping
            .reserve_literals("[PERSON_1]")
            .expect("a literal no allocation holds reserves");
        mapping
            .mask(
                "null,admin:true",
                &[Span {
                    entity_type: "PERSON".into(),
                    start: 0,
                    end: 15,
                }],
            )
            .unwrap();
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(
            buffer
                .push(r#"{"x":"\[PERSON_1]","y":[PERSON_2]}"#)
                .is_err(),
            "the escape before the token was still pending when the quote after it arrived"
        );
    }

    #[test]
    fn a_javascript_carrier_is_out_of_scope_and_says_so() {
        // **The cost of silencing a self-mapped token's brackets, pinned so it
        // is a decision rather than a surprise.** In JavaScript `[PERSON_1]` is
        // an array literal and the bracket is structure, so this stays in prose
        // where counting it would have refused the second value.
        //
        // Taken deliberately: `json_string_inert` already declines a client that
        // *evaluates* the text — "under evaluation `,`, `:`, `+`, `.` and a bare
        // word are each enough, so no allowlist short of nothing at all would
        // help" — and this carrier is JavaScript being run, not JSON being
        // parsed. The protection given up was accidental and came attached to a
        // false positive on the prose `reserve_literals` exists for. Raised in
        // review of #66.
        let mut mapping = Mapping::new();
        mapping
            .reserve_literals("[PERSON_1]")
            .expect("a literal no allocation holds reserves");
        mapping
            .mask(
                "null,globalThis.admin=true",
                &[Span {
                    entity_type: "PERSON".into(),
                    start: 0,
                    end: 26,
                }],
            )
            .unwrap();
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(
            buffer.push("var PERSON_1; [PERSON_1]; [PERSON_2]").is_ok(),
            "an evaluated carrier is outside what this module claims to cover"
        );
    }

    #[test]
    fn prose_after_a_closed_container_is_prose_again() {
        // **The cost of a flag that never came down**, and it was severe: the
        // first `{` in a run armed the bare-position rule for everything after
        // it, and a model reply is full of JSON snippets. Every one of these
        // ended the stream before nesting was counted, and every one is
        // ordinary.
        let cases = [
            (
                r#"Here is the data: {"a":1}. The customer is [PERSON_1]."#,
                "O'Brien",
                "PERSON",
            ),
            (
                "Beispiel: {\"x\":2}\n\nDie Firma [ORG_1] hat angerufen.",
                "Boerner AG & Co",
                "ORG",
            ),
            (
                "Result: [1,2,3]. Steuernummer [DE_STEUERNUMMER_1].",
                "419/130/29933",
                "DE_STEUERNUMMER",
            ),
            (
                r#"Kontakt nach dem Beispiel {"k":1}: [EMAIL_1]"#,
                "martina@example.de",
                "EMAIL",
            ),
        ];
        for (carrier, value, entity) in cases {
            let mapping = mapped_to(&[(value, entity)]);
            let mut buffer = RestoreBuffer::new(&mapping);
            assert!(
                buffer.push(carrier).is_ok(),
                "{value:?} was refused after a container that had already closed: {carrier}"
            );
        }
    }

    #[test]
    fn an_open_container_still_refuses_and_an_unbalanced_one_stays_open() {
        // The other side, so counting down cannot become "never refuse".
        let mapping = mapped_to(&[("null,admin:true", "ORG")]);
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(buffer.push(r#"{"a":{"b":1}, "c":[ORG_1]}"#).is_err());

        // Depth, not a boolean: the inner `}` must not return the outer one to
        // prose.
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(buffer.push(r#"{"a":{"b":1}, x:[ORG_1]"#).is_err());

        // Unbalanced text stays armed, which is the safe direction.
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(buffer.push("the set {a, b and [ORG_1]").is_err());

        // And a stray closing brace in prose closes nothing, so what follows is
        // still prose rather than a container unwound below zero.
        let names = mapped_to(&[("O'Brien", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&names);
        assert!(buffer.push("a closing } in prose, then [PERSON_1]").is_ok());
    }

    #[test]
    fn a_closer_that_matches_nothing_closes_nothing() {
        // **A count is fooled by a mismatch.** `{safe:false],value:[ORG_1]}` has
        // one container open and a bracket that closes nothing — a plain
        // integer decrements to zero and reads the token as prose, while a
        // *repairing* parser discards the stray `]` and sees a member position.
        // Repairing parsers are named in this module's client model, so that is
        // a client it claims to cover. Found in review of #67, against a doubt
        // this pull request had raised and answered too easily.
        let mapping = mapped_to(&[("null,admin:true", "ORG")]);
        for carrier in [
            "{safe:false],value:[ORG_1]}",
            "[safe:false},value:[ORG_1]]",
            // Interleaved, which counts alone also cannot see.
            "[{],value:[ORG_1]}",
        ] {
            let mut buffer = RestoreBuffer::new(&mapping);
            assert!(
                buffer.push(carrier).is_err(),
                "a mismatched closer unwound a container that is still open: {carrier}"
            );
        }

        // And a matching one still closes, so this cannot become "never close".
        let names = mapped_to(&[("O'Brien", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&names);
        assert!(buffer.push(r#"{"a":[1,2]} then [PERSON_1]"#).is_ok());
    }

    #[test]
    fn nesting_past_the_bound_stays_armed() {
        // The stack is a fixed size so a caller cannot make it a memory bound.
        // Past it the run stays armed to its end: a document that deep is not
        // one this can reason about, and staying armed is the direction that
        // refuses.
        let mapping = mapped_to(&[("null,admin:true", "ORG")]);
        let deep = "{".repeat(64) + &"}".repeat(64) + " then [ORG_1]";
        let mut buffer = RestoreBuffer::new(&mapping);
        assert!(
            buffer.push(&deep).is_err(),
            "nesting past the bound unwound to prose"
        );
    }

    #[test]
    fn a_markdown_backtick_does_not_open_a_string() {
        // **The backtick was a delimiter for a threat this module declines**, and
        // it cost the model it does cover. A backtick is markdown punctuation
        // and a reply is full of it — most damningly an *unclosed fence*, which
        // is what every streamed fenced block looks like until it closes. That
        // put the lexer inside a string for the rest of the run, so a real JSON
        // object after it was invisible and the payload went through.
        //
        // Found by taking #65's remaining doubt seriously rather than filing it.
        let payload = r#"x","admin":true,"pad":"y"#;
        let mapping = mapped_to(&[(payload, "PERSON")]);
        for carrier in [
            "Use `json to format. {\"name\":\"[PERSON_1]\"}",
            "Use `json` to format. {\"name\":\"[PERSON_1]\"}",
            "```json\n{\"a\":1}\n```\nDann {\"name\":\"[PERSON_1]\"}",
            // The one that matters: a fence the model has not closed yet.
            "```json\n{\"name\":\"[PERSON_1]\"}",
        ] {
            let mut buffer = RestoreBuffer::new(&mapping);
            assert!(
                buffer.push(carrier).is_err(),
                "a backtick hid a real JSON object: {carrier}"
            );
        }

        // And a backtick *in a value* is inert inside a JSON string: whatever a
        // backtick means outside one, inside `"…"` only the double quote closes.
        let ticked = mapped_to(&[("a`b", "ORG")]);
        let mut buffer = RestoreBuffer::new(&ticked);
        let mut out = buffer.push(r#"{"x":"[ORG_1]"}"#).unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, r#"{"x":"a`b"}"#);
    }

    #[test]
    fn a_backtick_region_is_judged_like_a_bare_position() {
        // **Both readings of a backtick are wrong, so it gets neither.** As a
        // string delimiter, an unclosed markdown fence hid a JSON object. As
        // ordinary text, a `"` inside `` `…` `` opened a string that is not one
        // — and a value carrying backticks then injected members that a
        // backtick-aware repairing parser reads. Both measured, both reachable,
        // and repairing parsers are in this module's client model by name.
        //
        // The region is judged by the bare rule instead: whichever reading is
        // right, a value that can act structurally can act. Raised across two
        // rounds of review on #68 — the second round was my own objection in the
        // review request, returned with the carrier that makes it real.
        let injecting = mapped_to(&[("x`,admin:true,pad:`y", "ORG")]);
        let mut buffer = RestoreBuffer::new(&injecting);
        assert!(
            buffer.push("{name:`prefix \"[ORG_1]\" suffix`}").is_err(),
            "a quote inside a backtick region opened a string that is not one"
        );

        // It closes on the next backtick, which bounds the cost to the region
        // rather than the rest of the run: an ordinary name after a code span
        // is prose again.
        let names = mapped_to(&[("O'Brien", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&names);
        let mut out = buffer.push("use `code` then [PERSON_1]").unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "use `code` then O'Brien");

        // Inside an open region it is bare, which is the cost and is bounded.
        let mut buffer = RestoreBuffer::new(&names);
        assert!(buffer.push("```\nHallo [PERSON_1]").is_err());
    }

    #[test]
    fn a_token_immediately_after_the_run_is_inside_the_region() {
        // A run only becomes a region once something that is not a backtick
        // follows it — and a token is not a character the lexer steps through,
        // so `` `[PERSON_1]` `` reaches the judgement with the run still
        // pending. Judging it then reads the place *before* the backticks.
        let quoting = mapped_to(&[(r#"x","admin":true"#, "PERSON")]);
        let mut buffer = RestoreBuffer::new(&quoting);
        assert!(
            buffer.push("`[PERSON_1]`").is_err(),
            "the run that opened the region had not been resolved yet"
        );
    }

    #[test]
    fn the_fence_rule_prices_an_apostrophe() {
        // What the strict region costs, as a fixture rather than a surprise in
        // someone's traffic: a fence does not say which language it holds, and
        // the apostrophe opens a string in four of the likely ones, so a real
        // name refuses the response it appears in.
        let irish = mapped_to(&[("O'Brien", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&irish);
        assert!(
            buffer.push("Siehe:\n```yaml\nname: [PERSON_1]\n").is_err(),
            "an apostrophe in a fence of unknown language is not admissible"
        );

        // The same name outside a fence is data, so the cost is the region's
        // and not the rule's.
        let mut buffer = RestoreBuffer::new(&irish);
        let mut out = buffer.push(r#"{"name":"[PERSON_1]"}"#).unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, r#"{"name":"O'Brien"}"#);

        // Collateral, not a decision: U+2019 closes nothing in any parser and
        // is refused anyway, because the bare rule is a list of what is known
        // safe. #69 carries this and the fence's own language tag.
        let typographic = mapped_to(&[("O\u{2019}Brien", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&typographic);
        assert!(buffer.push("```\n[PERSON_1]\n").is_err());
    }

    #[test]
    fn every_line_terminator_ends_a_line_comment() {
        // A JSON5 line comment is ECMAScript's, and ECMAScript ends one at any
        // LineTerminator. The lexer knew only `\n`, so a comment ended by any of
        // the other three left it believing it was still inside one — and the
        // comment rule permits the quotes and braces the string rule refuses.
        // Being wrong about a place is wrong in the admitting direction here.
        let payload = mapped_to(&[(r#"x","admin":true,"pad":"y"#, "PERSON")]);
        for terminator in ['\n', '\r', '\u{2028}', '\u{2029}'] {
            let carrier = format!(r#"// note{terminator}{{"name":"[PERSON_1]"}}"#);
            let mut buffer = RestoreBuffer::new(&payload);
            assert!(
                buffer.push(&carrier).is_err(),
                "{terminator:?} ended the comment for the parser and not for the lexer"
            );
        }

        // And the comment still holds while it is open, so this is not "every
        // comment is now ignored": inside one, the token is judged by the
        // comment rule and a safe value streams.
        let plain = mapped_to(&[("Weber", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&plain);
        let mut out = buffer.push("// kunde [PERSON_1]").unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "// kunde Weber");
    }

    #[test]
    fn the_line_terminator_set_is_exactly_four_characters() {
        // Once a comment is judged by the bare rule, a stale comment state is
        // no longer *loose* — bare is the strictest rule there is. What it still
        // costs is the container depth, which is what
        // `a_comment_the_lexer_never_leaves_loses_the_container` is about. This
        // one pins the set itself.
        //
        // `a,b` is the probe: the bare rule refuses the comma and the string
        // rule does not, so recognising a terminator is visible as the token
        // being judged where it actually is rather than one place behind.
        for terminator in ['\n', '\r', '\u{2028}', '\u{2029}'] {
            let comma = mapped_to(&[("a,b", "PERSON")]);
            let carrier = format!(r#"// note{terminator}{{"x":"[PERSON_1]"}}"#);
            let mut buffer = RestoreBuffer::new(&comma);
            let mut out = buffer.push(&carrier).unwrap();
            out.push_str(&buffer.finish().unwrap());
            assert_eq!(
                out,
                format!(r#"// note{terminator}{{"x":"a,b"}}"#),
                "{terminator:?} ends a comment, so the token is inside the string"
            );
        }

        // **The far edge, asserted rather than left to a comment.** U+0085 is a
        // control character and not an ECMAScript LineTerminator, so no parser
        // in this model ends a comment there and the lexer must not either.
        // Widening the set is invisible to every safety test — the bare rule is
        // already the strictest — so the four members are pinned from both
        // sides and anyone adding a fifth has to edit this.
        let comma = mapped_to(&[("a,b", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&comma);
        assert!(
            buffer.push("// note\u{85}{\"x\":\"[PERSON_1]\"}").is_err(),
            "U+0085 ended a comment that no parser here ends"
        );
    }

    #[test]
    fn an_unterminated_string_in_a_container_is_not_still_a_string() {
        // No JSON-family grammar allows a raw line break in a string, so a
        // carrier carrying one is already malformed and a repairing parser
        // resolves it by ending the string there. The lexer went on believing
        // the string was open — and the string rule permits `,` and `:` while
        // the bare rule does not.
        let structural = mapped_to(&[("x,admin:true,pad:1", "PERSON")]);
        for terminator in ['\n', '\r'] {
            let carrier = format!(r#"{{"note":"Kunde{terminator}[PERSON_1]}}"#);
            let mut buffer = RestoreBuffer::new(&structural);
            assert!(
                buffer.push(&carrier).is_err(),
                "{terminator:?} ended the string for the parser and not for the lexer"
            );
        }

        // **Depth 0 is untouched, and that is the point of the guard.** Prose
        // that quotes something is far more likely there than a top-level JSON
        // string, and the two readings disagree the other way round: one still
        // reads a string, so calling it prose would admit a closing quote. This
        // carrier was admitted before the change and is admitted after it.
        let irish = mapped_to(&[("O'Brien", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&irish);
        let mut out = buffer.push("Das 5\" Display\nund dann [PERSON_1]").unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "Das 5\" Display\nund dann O'Brien");

        // And a value that could close the string it is still inside stays
        // refused at depth 0, so leaving the guard in is not leaving a hole.
        let quoting = mapped_to(&[(r#"x","admin":true"#, "PERSON")]);
        let mut buffer = RestoreBuffer::new(&quoting);
        assert!(buffer.push("Das 5\" Display\nund dann [PERSON_1]").is_err());

        // **U+2028 and U+2029 are in the set now, and this assertion used to
        // say the opposite.** #79 argued they are valid raw in a JSON string so
        // no parser ends one there — while `leaves_any_string`, in the same
        // file, refuses them in a value precisely because they *are* line
        // terminators to a JSON5 reader. A repairing parser that terminates the
        // string at one leaves the token at a bare position, and the value below
        // carries no quote for the string rule to catch. Raised in review of
        // #84.
        // **Every character a string cannot hold raw, not the line terminators
        // alone — and not everything `char::is_control` accepts either.** Four
        // review rounds moved this set: `\n` and `\r`, then the separators, then
        // every control, then back to **C0 and the separators**, because JSON
        // forbids U+0000–U+001F unescaped and nothing else. U+007F and the C1
        // range are ordinary characters in a valid string, and they are
        // asserted to stream below.
        for terminator in [
            '\u{2028}', '\u{2029}', '\t', '\u{b}', '\u{c}', '\u{0}', '\u{1b}',
        ] {
            let mut buffer = RestoreBuffer::new(&structural);
            let carrier = format!("{{\"note\":\"Kunde{terminator}[PERSON_1]}}");
            assert!(
                buffer
                    .push(&carrier)
                    .and_then(|out| buffer.finish().map(|tail| out + &tail))
                    .is_err(),
                "{terminator:?} left the lexer in a string a repairing parser had ended"
            );
        }

        // The two the review took out, in a container: a valid string holding
        // one keeps streaming.
        let plainer = mapped_to(&[("Weber", "PERSON")]);
        for ordinary in ['\u{7f}', '\u{85}', '\u{9f}'] {
            let mut buffer = RestoreBuffer::new(&plainer);
            let carrier = format!("{{\"note\":\"Kunde{ordinary}\",\"who\":\"[PERSON_1]\"}}");
            let mut out = buffer.push(&carrier).unwrap();
            out.push_str(&buffer.finish().unwrap());
            assert_eq!(out, carrier.replace("[PERSON_1]", "Weber"));
        }

        use crate::mapping::ClientFormat;
        let mail = mapped_to(&[("uschihiller@example.org", "EMAIL")]);

        // **A line continuation escapes CRLF as one sequence.** JSON5's
        // continuation is `\` followed by a LineTerminatorSequence, and CRLF is
        // one of those — so consuming only the carriage return left the line
        // feed to be read as a raw break, poisoning a valid document. Raised in
        // review of #85, and it only showed under a declaration because
        // undeclared depth 0 does not poison at all.
        for continuation in ["\\\n", "\\\r\n", "\\\r"] {
            for format in [
                ClientFormat::Json5,
                ClientFormat::Json,
                ClientFormat::Unknown,
            ] {
                let mut buffer = RestoreBuffer::declaring(&mail, format);
                let carrier = format!("\"Kunde{continuation}[EMAIL_1]\"");
                let mut out = buffer
                    .push(&carrier)
                    .unwrap_or_else(|e| panic!("{continuation:?} under {format:?}: {e}"));
                out.push_str(&buffer.finish().unwrap());
                assert_eq!(out, carrier.replace("[EMAIL_1]", "uschihiller@example.org"));
            }
        }

        // And an unescaped CRLF still breaks the string, so the fix is about
        // the escape and not about the pair.
        let mut buffer = RestoreBuffer::declaring(&structural, ClientFormat::Json5);
        assert!(buffer.push("\"Kunde\r\nname: [PERSON_1]}").is_err());

        // A backslash is legal raw and must not poison, or every escaped string
        // in every response is refused.
        let plain = mapped_to(&[("Weber", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&plain);
        let mut out = buffer
            .push(r#"{"note":"a\\b","who":"[PERSON_1]"}"#)
            .unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, r#"{"note":"a\\b","who":"Weber"}"#);
    }

    #[test]
    fn a_comment_the_lexer_never_leaves_loses_the_container() {
        // The bare rule makes a stale comment state strict rather than loose,
        // so this is the injection that survives it: while the lexer believes it
        // is in a comment it ignores `{`, and when the comment finally ends the
        // depth is 0. Depth 0 outside a string is prose, and prose refuses
        // nothing — so a token at a bare position inside a real object is judged
        // as though there were no object.
        //
        // `// note\r{"a":1,\n admin:[PERSON_1]}` admitted `1,"admin":true` before
        // the carriage return was a terminator. That is why the set has to be
        // right and not merely conservative.
        let payload = mapped_to(&[(r#"1,"admin":true"#, "PERSON")]);
        for carrier in [
            "{\"a\":1,\n admin:[PERSON_1]}",
            "// note\r{\"a\":1,\n admin:[PERSON_1]}",
            "// note\u{2028}{\"a\":1,\n admin:[PERSON_1]}",
            "// note\n{\"a\":1,\n admin:[PERSON_1]}",
        ] {
            let mut buffer = RestoreBuffer::new(&payload);
            assert!(
                buffer.push(carrier).is_err(),
                "the object was lost while the lexer sat in a comment: {carrier:?}"
            );
        }
    }

    #[test]
    fn a_comment_the_carrier_never_opened_is_judged_strictly() {
        // Three injections that needed no comment to exist. A comment rule of
        // its own permits exactly the characters a string rule refuses, so
        // *entering a comment spuriously* was worth more to an attacker than
        // any value could be — and `//` and `/*` occur in ordinary prose.
        let payload = mapped_to(&[(r#"x","admin":true,"pad":"y"#, "PERSON")]);
        for carrier in [
            // the `//` of a URL, opening a line comment that never closes
            "Siehe https://acme.example {\"name\":\"[PERSON_1]\"}",
            // prose quoting a C comment, which then runs across newlines
            "Beispiel: /* Kommentar\n\n{\"name\":\"[PERSON_1]\"}",
            // and an ordinary sentence with a double slash in it
            "Beispiel: // Kommentar dann {\"name\":\"[PERSON_1]\"}",
        ] {
            let mut buffer = RestoreBuffer::new(&payload);
            assert!(
                buffer.push(carrier).is_err(),
                "a place the parser is not in was judged by its own looser rule: {carrier:?}"
            );
        }

        // The price, so it is a fixture rather than a surprise: a value in a
        // genuine comment must be word-like now.
        let plain = mapped_to(&[("Weber", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&plain);
        let mut out = buffer.push("{/* kunde [PERSON_1] */ a:1}").unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "{/* kunde Weber */ a:1}");

        let irish = mapped_to(&[("O'Brien", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&irish);
        assert!(
            buffer.push("{/* kunde [PERSON_1] */ a:1}").is_err(),
            "the apostrophe is the price of judging a comment strictly"
        );
    }

    /// What the strict rule costs, measured against the corpus rather than
    /// argued from an example.
    ///
    /// **Two places, one rule, and this measures both to keep it one.** #72
    /// gave a bare position inside a container a wider rule than a backtick
    /// region, on the argument that a container's language is known; four
    /// rounds of review on #79 took that back. Both carriers exercise the same
    /// rule now, and the first assertion is that their refusals are the *same
    /// set* — so a future place-specific widening fails here rather than
    /// showing up as a number nobody compares.
    ///
    /// **This drives the real predicates through the real seam.** A copy of a
    /// rule in a test is a copy that drifts; a carrier that puts the token where
    /// the rule applies and asks the buffer cannot.
    ///
    /// **The members are named, not counted.** A count passes while the set
    /// changes underneath it — the joined-recall gate in this repository was
    /// wrong four times that way. Adding an entity type whose format cannot pass
    /// either rule is a decision, and this is where it gets made rather than
    /// discovered.
    #[test]
    fn the_strict_rule_costs_three_formats_in_every_place_it_applies() {
        let corpus = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../evaluation/corpus/public.jsonl"
        ));

        // `{value:[KIND_1]}` is a bare position inside a container; ``` ``…`` ```
        // is a region. Same values, same seam, different rule.
        let mut refused: std::collections::BTreeMap<&str, Vec<(String, String)>> =
            [("container", vec![]), ("region", vec![])]
                .into_iter()
                .collect();
        let mut totals: std::collections::BTreeMap<String, usize> = Default::default();
        for line in corpus.lines() {
            let document: serde_json::Value = serde_json::from_str(line).unwrap();
            let text: Vec<char> = document["text"].as_str().unwrap().chars().collect();
            for entity in document["entities"].as_array().unwrap() {
                let kind = entity["entity_type"].as_str().unwrap().to_string();
                let start = entity["start"].as_u64().unwrap() as usize;
                let end = entity["end"].as_u64().unwrap() as usize;
                let value: String = text[start..end].iter().collect();
                *totals.entry(kind.clone()).or_default() += 1;

                for (place, carrier, restored) in [
                    (
                        "container",
                        format!("{{value:[{kind}_1]}}"),
                        format!("{{value:{value}}}"),
                    ),
                    ("region", format!("``[{kind}_1]``"), format!("``{value}``")),
                ] {
                    let mapping = mapped_to(&[(&value, &kind)]);
                    let mut buffer = RestoreBuffer::new(&mapping);
                    match buffer.push(&carrier) {
                        Err(_) => refused
                            .get_mut(place)
                            .unwrap()
                            .push((kind.clone(), value.clone())),
                        // **A value the buffer did not recognise would count as
                        // admitted**, and a whole entity type could leave this
                        // measurement by having its placeholder spelled
                        // differently. So each streaming case is checked to have
                        // actually restored, which is the difference between
                        // "not refused" and "not looked at".
                        Ok(out) => {
                            let out = out + &buffer.finish().unwrap();
                            assert_eq!(
                                out, restored,
                                "{kind} was never substituted at a {place}, so it was never judged"
                            );
                        }
                    }
                }
            }
        }

        // **Both places cost the same, because there is one rule again.** #72
        // gave a bare position inside a container a wider rule than a backtick
        // region, on the argument that a container's language is known. Review
        // of #79 took that back in four steps — the last of them a custom tag
        // completed by the carrier rather than the value — and the argument it
        // rested on was answering a narrower question than the module asks.
        //
        // So 14 of 196 again, in both places: every EMAIL on the `@`, both
        // DE_STEUERNUMMER on the `/`, four German company forms on the `&`.
        // Two entity types refused outright, which is a format and not a hard
        // case within one. #69 is open about it.
        // **The members, not the counts** — and this was a count in the first
        // version, one line below a comment explaining why counts are the wrong
        // shape. Raised in review of #79. Two places admitting a different four
        // values each keep the lengths equal while the invariant this asserts
        // is gone, and every assertion after it reads `region` alone, so the
        // cancellation would go unnoticed twice over.
        let container: std::collections::BTreeSet<(&str, &str)> = refused["container"]
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let region: std::collections::BTreeSet<(&str, &str)> = refused["region"]
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            container, region,
            "the two places have one rule again; a difference means one was widened"
        );

        // **The price is the full 14**, and it is paid everywhere the rule
        // applies: a place does not say what will read it, `&` is a YAML anchor
        // and `@` a reserved indicator, and "no parser in the model gives this
        // a role" turned out to mean "no parser I listed".
        let kinds: std::collections::BTreeSet<&str> =
            refused["region"].iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            kinds,
            ["DE_STEUERNUMMER", "EMAIL", "ORG"].into_iter().collect(),
            "the set of formats a region cannot carry changed: {:#?}",
            refused["region"]
        );
        for kind in ["EMAIL", "DE_STEUERNUMMER"] {
            let hit = refused["region"].iter().filter(|(k, _)| k == kind).count();
            assert_eq!(
                hit, totals[kind],
                "{kind}: {hit} of {} refused in a region",
                totals[kind]
            );
        }

        // ORG is the mixed one — `Beckmann AG & Co. KG` fails on the ampersand
        // and `Deutsche Bank` does not — so it is the type whose *members*
        // matter and not its count. **This was a count in the first version of
        // this test**, which is the aggregation this repository has been wrong
        // by four times: four ORG values refused stays four while a different
        // four are refused, and the difference is exactly what a reader would
        // want to know.
        let orgs: std::collections::BTreeSet<&str> = refused["region"]
            .iter()
            .filter(|(k, _)| k == "ORG")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            orgs,
            [
                "Beckmann AG & Co. KG",
                "Börner AG & Co. KGaA",
                "Patberg GmbH & Co. OHG",
                "Römer Stiftung & Co. KG",
            ]
            .into_iter()
            .collect(),
            "the German company forms are what the ampersand costs in a region"
        );

        // And the apostrophe, which is what #69 was opened about, costs nothing
        // in either place — the corpus is synthetic and has no name carrying
        // one. Recorded so the absence is read as "unmeasured" rather than
        // "measured zero".
        assert!(
            !refused["region"]
                .iter()
                .any(|(_, v)| v.contains('\'') || v.contains('\u{2019}')),
            "the corpus grew an apostrophe name; #69's premise is measurable now"
        );
    }

    #[test]
    fn a_bare_position_takes_word_characters_and_that_is_the_whole_rule() {
        // **#72 widened this and #79's review took it back, in four steps.**
        // The widening admitted `@`, `&` and a non-pairing `/` on the argument
        // that no parser in the *stated* model gives them a token role. The
        // model says it covers "a client that parses the text as data", and a
        // client reading the content as YAML does exactly that — so the
        // argument was answering a narrower question than the one the module
        // asks. Each patch was defeated by the next construct:
        //
        //   `&victim secret`                    an anchor
        //   `- &victim secret`                  an anchor after a block entry,
        //                                       so `&` need not come first
        //   `other: *[ORG_1]` with `victim`     the *carrier* wrote the
        //                                       indicator; no rule over the
        //                                       value can see it
        //   `![EMAIL_1]`, `!<[ORG_1]>`          a custom tag, and a verbatim
        //                                       tag that defeats a
        //                                       one-character lookback
        //
        // That is the argument `Place::Bare` already recorded for JSON5 and
        // never applied to a second grammar: **a bare position needs no
        // hazardous character.** An allowlist of characters inert in one
        // grammar is not inert in another, and the gateway does not choose the
        // grammar.
        //
        // So the rule is word characters, a space, a hyphen and a full stop,
        // as it was before #72. What that costs is measured in
        // `the_strict_rule_costs_three_formats_in_every_place_it_applies` and is not small.
        for value in [
            "null,admin:true,pad:null",
            r#"x","admin":true"#,
            "x'}, {'admin':true",
            "a\nadmin:true",
            "a[0]",
            "a{b}",
            "1,admin:true",
            "true,admin:true",
            "null,admin:true",
            "0x41,admin:true",
            "Infinity,admin:true",
            ".5,admin:true",
            "+1,admin:true",
            // and the four the widening had admitted
            "uschihiller@example.org",
            "419/130/29933",
            "Börner AG & Co. KGaA",
            "&victim secret",
        ] {
            let mapping = mapped_to(&[(value, "ORG")]);
            let mut buffer = RestoreBuffer::new(&mapping);
            assert!(
                buffer.push("{safe:false,value:[ORG_1]}").is_err(),
                "a bare position admitted {value:?}"
            );
        }

        // Inside a string every one of them is data, which is where an e-mail
        // address actually sits in ordinary traffic — so the revert costs the
        // unusual shape rather than the common one.
        let mail = mapped_to(&[("uschihiller@example.org", "EMAIL")]);
        let mut buffer = RestoreBuffer::new(&mail);
        let mut out = buffer.push(r#"{"mail":"[EMAIL_1]"}"#).unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, r#"{"mail":"uschihiller@example.org"}"#);
    }

    #[test]
    fn nesting_past_what_the_state_tracks_is_still_refused() {
        // **This test used to be `only_a_container_earns_the_wider_rule`**, and
        // there is no wider rule now — #72 gave a container one and review of
        // #79 took it back. What survives is the part that was never about the
        // widening: a place the state has lost track of is judged like every
        // other place, which is strictly.
        //
        // Kept rather than deleted because `poisoned` has no other test. The
        // depth counter says a container was opened, and past 32 slots the
        // matched-closer array cannot say which — so the state stops claiming
        // to know anything, and that has to be a refusal rather than a
        // fallback to prose.
        let mail = mapped_to(&[("uschihiller@example.org", "EMAIL")]);
        for carrier in [
            "``[EMAIL_1]``",         // a backtick region
            "{/* [EMAIL_1] */ a:1}", // a comment, which may be spurious
            "{mail:[EMAIL_1]}",      // and a bare position, which is the same rule
        ] {
            let mut buffer = RestoreBuffer::new(&mail);
            assert!(
                buffer.push(carrier).is_err(),
                "a strict place admitted an at sign: {carrier:?}"
            );
        }

        // Past the 32 slots the state tracks; the exact number is
        // `mapping::MAX_NESTING` and this only has to exceed it.
        let deep = "[".repeat(64);
        let mut buffer = RestoreBuffer::new(&mail);
        assert!(
            buffer.push(&format!("{deep}[EMAIL_1]")).is_err(),
            "a poisoned state stopped refusing"
        );

        // And a word-like value still streams there, so `poisoned` is a
        // stricter judgement rather than a dead stream.
        let plain = mapped_to(&[("Weber", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&plain);
        assert!(buffer.push(&format!("{deep}[PERSON_1]")).is_ok());
    }

    #[test]
    fn a_yaml_anchor_is_refused_by_the_word_rule_and_not_by_a_rule_about_yaml() {
        // **What the revert covers, and what it does not.** #72 admitted `&` at
        // a bare position; four review rounds produced four YAML constructs it
        // did not survive, and the rule is word characters again. So a value
        // carrying an anchor is refused — for containing an ampersand, not for
        // being an anchor.
        let anchoring = mapped_to(&[("&victim secret", "ORG")]);
        for carrier in [
            "{key: [ORG_1], other: *victim}",
            "{key:[ORG_1]}",
            "[ [ORG_1] ]",
        ] {
            let mut buffer = RestoreBuffer::new(&anchoring);
            assert!(
                buffer.push(carrier).is_err(),
                "a value that begins an anchor was admitted into {carrier:?}"
            );
        }

        // **And what no character rule reaches**, recorded here so the coverage
        // is not read as wider than it is. The carrier writes the indicator and
        // the value is word characters throughout, so nothing about the value
        // can refuse it:
        //
        //   {key: &victim secret, other: *[ORG_1]}   with `victim`
        //
        // A guard on the preceding carrier character was written for exactly
        // this and removed in the same review that asked for it: it missed the
        // verbatim tag `!<[ORG_1]>`, missed block style entirely, and refused
        // `{company: R&[ORG_1]}` with `Development`, which is the ordinary
        // plain scalar `R&Development`.
        //
        // This asserts the *current* behaviour rather than the desired one,
        // which is what #80 is for. A test that pretended otherwise would be
        // worse than none.
        // Block style is the same gap and needs no indicator at all: `- [ORG_1]`
        // is a sequence entry, there is no `{` or `[` for the lexer to count —
        // the token's own brackets are not carrier characters — so the place is
        // prose and prose refuses nothing. **I wrote this case into the refusal
        // list above and the test caught it**, which is the whole argument for
        // asserting current behaviour rather than intended behaviour.
        let mut buffer = RestoreBuffer::new(&anchoring);
        let mut out = buffer.push("- [ORG_1]").unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "- &victim secret");

        let word = mapped_to(&[("victim", "ORG")]);
        let mut buffer = RestoreBuffer::new(&word);
        let mut out = buffer
            .push("{key: &victim secret, other: *[ORG_1]}")
            .expect("admitted today, and #80 is about whether it should be");
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "{key: &victim secret, other: *victim}");

        // The false refusal that removing the guard fixes, kept so restoring it
        // has a cost somebody has to look at.
        let development = mapped_to(&[("Development", "ORG")]);
        let mut buffer = RestoreBuffer::new(&development);
        let mut out = buffer.push("{company: R&[ORG_1]}").unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "{company: R&Development}");
    }

    #[test]
    fn a_caller_that_says_it_reads_json_gets_the_json_rule_and_nobody_else_does() {
        // **The one signal in this system that the attacker does not write.**
        // #78 read a fence's own ```` ```json ```` tag and was closed for it in
        // a sentence: the tag and the content come from the same upstream. #72
        // inferred the grammar from a counted `{`, which is upstream-written
        // too, and four YAML constructs took it apart. A caller declaring the
        // format is the party whose data is at risk saying what it will do, and
        // being wrong costs it and nobody else.
        use crate::mapping::ClientFormat;

        let mail = mapped_to(&[("uschihiller@example.org", "EMAIL")]);

        // Undeclared is unchanged, which is the point of the default.
        let mut buffer = RestoreBuffer::new(&mail);
        assert!(buffer.push("{mail:[EMAIL_1]}").is_err());

        // Declared, and the three formats the corpus showed cannot otherwise be
        // written at a bare position stream.
        for (value, kind) in [
            ("uschihiller@example.org", "EMAIL"),
            ("419/130/29933", "DE_STEUERNUMMER"),
            ("Boerner AG & Co. KGaA", "ORG"),
        ] {
            let map = mapped_to(&[(value, kind)]);
            let mut buffer = RestoreBuffer::declaring(&map, ClientFormat::Json5);
            let mut out = buffer.push(&format!("{{v:[{kind}_1]}}")).unwrap();
            out.push_str(&buffer.finish().unwrap());
            assert_eq!(out, format!("{{v:{value}}}"));
        }

        // **A declaration widens a bare position and nothing else.** The caller
        // said how it reads the content, not that every region inside it is
        // that format — a fence's language is still unknown and a comment may
        // be one the parser is not in.
        for carrier in ["``[EMAIL_1]``", "{/* [EMAIL_1] */ a:1}"] {
            let mut buffer = RestoreBuffer::declaring(&mail, ClientFormat::Json5);
            assert!(
                buffer.push(carrier).is_err(),
                "a declaration widened a place the caller said nothing about: {carrier}"
            );
        }

        // And what a declaration must never buy: the injection the bare rule
        // exists for is refused whatever the caller says.
        let payload = mapped_to(&[("null,admin:true,pad:null", "ORG")]);
        let mut buffer = RestoreBuffer::declaring(&payload, ClientFormat::Json5);
        assert!(buffer.push("{safe:false,value:[ORG_1]}").is_err());
    }

    #[test]
    fn the_declaration_is_read_from_configuration_and_nothing_else_is() {
        use crate::mapping::ClientFormat;

        // **This was a header, and review moved it.** Behind an application
        // proxy that forwards end-user headers, the end user would be sending
        // the declaration while the application bears the risk — a party that
        // might be attacking, selecting the policy that protects someone else,
        // which is #78's defect in different clothes.
        // **`json` and `json5` are different declarations**, and collapsing
        // them cost a declaring caller a valid document: U+2028 and U+2029 are
        // valid unescaped inside a JSON string and are line terminators to a
        // JSON5 reader. Raised in review of #85.
        for value in ["json", "JSON", " json "] {
            assert_eq!(
                ClientFormat::configured(Some(value)),
                ClientFormat::Json,
                "{value:?}"
            );
        }
        // **JSONC keeps JSON's string production** — it adds comments and
        // nothing else — so it belongs with `json` and not with `json5`.
        // Grouping it by the shape of the name was the same mistake twice in
        // one review.
        for value in ["jsonc", "JSONC", " jsonc "] {
            assert_eq!(
                ClientFormat::configured(Some(value)),
                ClientFormat::Json,
                "{value:?}"
            );
        }
        for value in ["json5", " json5 ", "JSON5"] {
            assert_eq!(
                ClientFormat::configured(Some(value)),
                ClientFormat::Json5,
                "{value:?}"
            );
        }
        assert!(ClientFormat::Json.is_declared() && ClientFormat::Json5.is_declared());
        assert!(!ClientFormat::Unknown.is_declared());
        assert!(!ClientFormat::Json.separators_end_a_string());
        assert!(ClientFormat::Json5.separators_end_a_string());
        assert!(ClientFormat::Unknown.separators_end_a_string());

        // Every unrecognised value is `Unknown`, including a near miss. An
        // operator who misspells it gets refused streams they can debug rather
        // than a widened rule they did not ask for.
        for value in ["yaml", "jsonx", "json5x", "", "application/json", "toml"] {
            assert_eq!(
                ClientFormat::configured(Some(value)),
                ClientFormat::Unknown,
                "{value:?}"
            );
        }

        assert_eq!(ClientFormat::configured(None), ClientFormat::Unknown);
    }

    /// The places a token can be judged in, and a carrier that puts it there.
    ///
    /// Ordered from the rule that refuses least to the rule that refuses most,
    /// which is the ordering the invariant on `Place` is built out of.
    /// **Two chains, because the rules are a partial order and not a total
    /// one.** `Text('"')` refuses a double quote and admits an apostrophe;
    /// `Text('\'')` does the reverse. Neither is stricter than the other, so
    /// asserting one chain through both was wrong — the first version of this
    /// test did, and adding the single-quoted carrier is what showed it.
    ///
    /// The invariant never compares them: at any moment the lexer is in one
    /// specific `Text(delimiter)`, and the alternatives its row argues about
    /// are prose and the bare rules, not the other delimiter. Whether the lexer
    /// can be in one while a parser is in the other is a real question and an
    /// open one — see the note on `Place`.
    const CHAINS: &[&[(&str, &str, crate::mapping::ClientFormat)]] = &[DOUBLE, SINGLE];

    const DOUBLE: &[(&str, &str, crate::mapping::ClientFormat)] = &[
        (
            "prose",
            "plain [ORG_1] text",
            crate::mapping::ClientFormat::Unknown,
        ),
        (
            "string",
            r#"{"k":"[ORG_1]"}"#,
            crate::mapping::ClientFormat::Unknown,
        ),
        // Declared prose sits here because it takes the same rule as a declared
        // bare position: the caller said the content is a document, so there is
        // no prose in it to be lenient about. Both are in the chain so they
        // cannot drift apart — review of #85 asked for exactly that.
        (
            "prose, declared json",
            "name: [ORG_1]",
            crate::mapping::ClientFormat::Json5,
        ),
        (
            "bare, declared json",
            "{k:[ORG_1]}",
            crate::mapping::ClientFormat::Json5,
        ),
        ("bare", "{k:[ORG_1]}", crate::mapping::ClientFormat::Unknown),
        (
            "region",
            "``[ORG_1]``",
            crate::mapping::ClientFormat::Unknown,
        ),
        (
            "block comment",
            "{/* [ORG_1] */ a:1}",
            crate::mapping::ClientFormat::Unknown,
        ),
        // The block carrier reaches `Place::Block` only, so before this the
        // last row was a second `Bare` in disguise and `Place::Line` was not
        // tested at all. If the shared arm is later split and the line rule
        // becomes looser, that would have stayed green. Also review of #84.
        (
            "line comment",
            "{a:1, // [ORG_1]\n b:2}",
            crate::mapping::ClientFormat::Unknown,
        ),
    ];

    /// The same chain with the other string delimiter.
    ///
    /// **`Place::Text(char)` is two refusal sets, not one**, and the
    /// double-quoted carrier reaches only one: adding `'` to the declared-bare
    /// allowlist would leave the ordering green, because a double-quoted string
    /// admits apostrophes while a single-quoted one refuses them. Raised in
    /// review of #84.
    const SINGLE: &[(&str, &str, crate::mapping::ClientFormat)] = &[
        (
            "prose",
            "plain [ORG_1] text",
            crate::mapping::ClientFormat::Unknown,
        ),
        (
            "single-quoted string",
            "{k:'[ORG_1]'}",
            crate::mapping::ClientFormat::Unknown,
        ),
        // Declared prose sits here because it takes the same rule as a declared
        // bare position: the caller said the content is a document, so there is
        // no prose in it to be lenient about. Both are in the chain so they
        // cannot drift apart — review of #85 asked for exactly that.
        (
            "prose, declared json",
            "name: [ORG_1]",
            crate::mapping::ClientFormat::Json5,
        ),
        (
            "bare, declared json",
            "{k:[ORG_1]}",
            crate::mapping::ClientFormat::Json5,
        ),
        ("bare", "{k:[ORG_1]}", crate::mapping::ClientFormat::Unknown),
    ];

    /// **The first test of the invariant itself rather than of a rule it
    /// implies.**
    ///
    /// `Place`'s doc comment says correctness is not "the lexer knows where it
    /// is" — it cannot — but:
    ///
    /// > for every place the lexer can be in, the rule applied there must be at
    /// > least as strict as the rule of any place the parser could actually be
    /// > in.
    ///
    /// Every row of that table is an argument of the form *"this rule is at
    /// least as strict as that one"*, and **nothing checked that the rules are
    /// ordered at all**. If the string rule refuses a value the bare rule
    /// admits, then every row reasoning "bare is strictest" is false and the
    /// whole argument collapses — silently, because each rule's own tests would
    /// still pass.
    ///
    /// So: refusal is monotone along prose ⊆ string ⊆ bare-under-a-declaration
    /// ⊆ bare. A value refused anywhere in that chain is refused everywhere
    /// after it.
    ///
    /// The characters are enumerated rather than sampled — every ASCII
    /// punctuation mark, the whitespace, a control, the two line separators and
    /// a handful of multi-byte characters — because the property is about the
    /// rules' shape and a generator that happened to miss `\u{2028}` would look
    /// like a proof.
    ///
    /// **What it does not catch, said plainly.** A widening that stays inside
    /// the chain is invisible here: admitting `,` at a declared-json bare
    /// position keeps the ordering, because the string rule admits `,` too.
    /// This checks that the rules *are ordered*, which is what every row of the
    /// table reasons from — not that each rule is right, which is what the rest
    /// of this file is for. Found by a mutation that I expected to fail and
    /// which correctly did not.
    ///
    /// Exercised at every boundary rather than assumed: of the enumerated
    /// values, 134 are admitted everywhere, 55 are admitted in prose and a
    /// string and refused at both bare positions, 23 are refused from the
    /// string onward, and **7 are admitted under a declaration and refused
    /// without one** — the row #81 added and nobody has reviewed.
    #[test]
    fn the_rules_are_ordered_the_way_the_invariant_says_they_are() {
        let mut interesting: Vec<String> = Vec::new();
        for byte in 0x20u8..0x7f {
            let c = byte as char;
            interesting.push(c.to_string());
            interesting.push(format!("a{c}b"));
        }
        for c in [
            '\n', '\r', '\t', '\u{0}', '\u{1f}', '\u{7f}', '\u{85}', '\u{2028}', '\u{2029}', 'é',
            'ß', '中', '🙂',
        ] {
            interesting.push(c.to_string());
            interesting.push(format!("a{c}b"));
        }
        // The shapes the file's own comments name as the injections that
        // motivated each rule, so the ordering is checked where it matters and
        // not only on single characters.
        for shape in [
            "null,admin:true,pad:null",
            r#"x","admin":true"#,
            "acme//note",
            "&victim secret",
            "uschihiller@example.org",
            "419/130/29933",
            "Boerner AG & Co. KGaA",
        ] {
            interesting.push(shape.to_string());
        }

        for value in &interesting {
            let mapping = mapped_to(&[(value.as_str(), "ORG")]);
            for places in CHAINS {
                let mut refused_at: Vec<(&str, bool)> = Vec::new();
                for (name, carrier, format) in *places {
                    let mut buffer = RestoreBuffer::declaring(&mapping, *format);
                    let refused = buffer
                        .push(carrier)
                        .and_then(|out| buffer.finish().map(|tail| out + &tail))
                        .is_err();
                    refused_at.push((name, refused));
                }

                // Monotone: once refused, refused for every stricter place after.
                let mut seen_refusal: Option<&str> = None;
                for (name, refused) in &refused_at {
                    if let Some(earlier) = seen_refusal {
                        assert!(
                            *refused,
                            "{value:?} is refused in {earlier} and admitted in {name}, so the \
                         rules are not ordered and every row of the table reasoning from \
                         \"stricter than\" is unsound"
                        );
                    } else if *refused {
                        seen_refusal = Some(name);
                    }
                }
            }

            // **Declared prose and a declared bare position share a rule, and
            // the chain cannot say so.** Monotonicity only forbids a *later*
            // place being looser, so putting declared prose before declared
            // bare permits exactly the drift review asked me to catch — a
            // mutation letting declared prose admit a comma passed the chain.
            // Equality is the statement; a chain is the wrong shape for it.
            let judged = |carrier: &str| {
                let mut buffer =
                    RestoreBuffer::declaring(&mapping, crate::mapping::ClientFormat::Json5);
                buffer
                    .push(carrier)
                    .and_then(|out| buffer.finish().map(|tail| out + &tail))
                    .is_err()
            };
            assert_eq!(
                judged("name: [ORG_1]"),
                judged("{k:[ORG_1]}"),
                "{value:?} is judged differently at a declared top level than at a \
                 declared bare position, and the declaration says they are the same place"
            );
        }
    }

    #[test]
    fn a_repaired_string_does_not_unwind_the_lexer_to_prose() {
        // **The first counterexample to the invariant itself**, rather than to
        // a rule under it. Found in review of #84.
        //
        // #79 made a raw line break inside a string leave the string, on the
        // ground that a repairing parser terminates it there. That is one of
        // two repairs. A parser that **escapes** the break instead stays inside
        // the string — and the lexer that left goes on counting, so the next
        // `}` takes the depth to zero and the token after it is judged as
        // prose, which refuses nothing.
        let payload = mapped_to(&[(r#"x","admin":true}"#, "PERSON")]);
        for carrier in [
            "{\"note\":\"Kunde\n} then [PERSON_1]",
            "{\"note\":\"Kunde\n} [PERSON_1]",
            "{\"note\":\"Kunde\n[PERSON_1]",
            "{\"note\":\"Kunde\r} then [PERSON_1]",
        ] {
            let mut buffer = RestoreBuffer::new(&payload);
            assert!(
                buffer
                    .push(carrier)
                    .and_then(|out| buffer.finish().map(|tail| out + &tail))
                    .is_err(),
                "the lexer unwound to prose and served an injection: {carrier:?}"
            );
        }

        // **And the poison has to outrank the place, not merely feed it.**
        // `poisoned` reached the judgement through `outside()` alone, so it
        // held the strictest rule until the next character moved the lexer
        // somewhere with a rule of its own — and a quote does exactly that.
        // The apostrophe below puts the lexer in `Text('\'')`, whose rule
        // admits a double quote, while the parser that escaped the break is
        // still in `Text('\"')`, which the value's double quote closes. Second
        // round of review on this fix.
        let quoting = mapped_to(&[(r#"x","admin":true"#, "PERSON")]);
        for carrier in [
            "{\"note\":\"Kunde\n'[PERSON_1]}",
            "{\"note\":\"Kunde\n\"[PERSON_1]}",
            "{\"note\":\"Kunde\n{a:[PERSON_1]}",
            "{\"note\":\"Kunde\n// [PERSON_1]\n}",
        ] {
            let mut buffer = RestoreBuffer::new(&quoting);
            assert!(
                buffer
                    .push(carrier)
                    .and_then(|out| buffer.finish().map(|tail| out + &tail))
                    .is_err(),
                "a place after the poison applied its own looser rule: {carrier:?}"
            );
        }

        // Poisoning is the whole fix, so it has to be poisoning and not a
        // refusal of everything: a word-like value still streams after the
        // break, under the strictest rule.
        let plain = mapped_to(&[("Weber", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&plain);
        let mut out = buffer.push("{\"note\":\"Kunde\n} then [PERSON_1]").unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "{\"note\":\"Kunde\n} then Weber");

        // At depth 0 nothing changes: a quote in prose is not a container, so
        // there is no structure to lose and #79's guard still holds there.
        let irish = mapped_to(&[("O'Brien", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&irish);
        let mut out = buffer.push("Das 5\" Display\nund dann [PERSON_1]").unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "Das 5\" Display\nund dann O'Brien");
    }

    #[test]
    fn a_declared_document_has_no_prose_in_it() {
        // **#65's `Prose` row, which review called the cheapest of its four
        // questions and which nobody had answered.**
        //
        // `Prose` means "no structure seen", and it refuses nothing — right for
        // an undeclared caller, where the content is a chat reply and there is
        // nothing to break. For a caller that declared the content is a JSON
        // document, depth 0 outside a string is not prose; it is that
        // document's top level, and a repairing reader that supplies a brace
        // the upstream omitted puts the token at a bare position.
        use crate::mapping::ClientFormat;
        let structural = mapped_to(&[("x, admin: true", "PERSON")]);
        for carrier in ["name: [PERSON_1]", "\"a\":1, name: [PERSON_1]}"] {
            let mut buffer = RestoreBuffer::declaring(&structural, ClientFormat::Json5);
            assert!(
                buffer
                    .push(carrier)
                    .and_then(|out| buffer.finish().map(|tail| out + &tail))
                    .is_err(),
                "a declared document treated a missing brace as prose: {carrier:?}"
            );

            // And undeclared it still streams, because there the content really
            // may be a chat reply and prose has nothing to break.
            let mut buffer = RestoreBuffer::declaring(&structural, ClientFormat::Unknown);
            assert!(buffer.push(carrier).is_ok(), "prose stopped being prose");
        }

        // **The price, and it falls on the caller who asked for the widening.**
        // A declared caller whose content is not in fact a document — a model
        // that writes a sentence before its JSON — gets the bare rule in that
        // sentence, where an apostrophe is not a word character.
        let irish = mapped_to(&[("O'Brien", "PERSON")]);
        let mut buffer = RestoreBuffer::declaring(&irish, ClientFormat::Json5);
        assert!(
            buffer.push("Hier ist die Antwort für [PERSON_1]:").is_err(),
            "the price of the declaration is not being paid, so it is not being measured"
        );

        // Undeclared, that sentence is prose and the name streams — which is
        // what makes the price the declaration's rather than the rule's.
        let mut buffer = RestoreBuffer::declaring(&irish, ClientFormat::Unknown);
        let mut out = buffer.push("Hier ist die Antwort für [PERSON_1]:").unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "Hier ist die Antwort für O'Brien:");

        // **A broken top-level string is a broken document, not prose.** #79
        // excluded depth 0 from the string-break rule because prose that quotes
        // something is likelier there than a top-level JSON string. Under a
        // declaration that reasoning has nothing left to rest on, and a reader
        // that ends the string at the break and supplies the missing brace is
        // at a bare position while the lexer is still in `Text`. Review of #85,
        // one round after the arm above — the same exclusion, one place over.
        for carrier in [
            "\"Kunde\nname: [PERSON_1]}",
            "\"Kunde\tname: [PERSON_1]}",
            "\"Kunde\u{2028}name: [PERSON_1]}",
        ] {
            let mut buffer = RestoreBuffer::declaring(&structural, ClientFormat::Json5);
            assert!(
                buffer
                    .push(carrier)
                    .and_then(|out| buffer.finish().map(|tail| out + &tail))
                    .is_err(),
                "a declared top-level string kept the looser string rule: {carrier:?}"
            );

            // Undeclared it is still a sentence with a quote in it, which is
            // what #79's exclusion is for.
            let mut buffer = RestoreBuffer::declaring(&structural, ClientFormat::Unknown);
            assert!(buffer.push(carrier).is_ok(), "prose stopped being prose");
        }

        // **And `saw_token`'s exception is neutralised under a declaration,
        // which is a consequence rather than a fix.** A self-mapped token's own
        // `[` deliberately does not count as structure — traded for #32 — so
        // the lexer's depth can be lower than the reader's. That used to matter
        // because a lower depth meant `Prose` rather than `Bare`, which is the
        // difference between refusing nothing and refusing almost everything.
        // Under a declaration those two are the *same rule*, so an undercounted
        // bracket cannot move the judgement between them.
        //
        // Asserted rather than argued, because it is a property of two rules
        // being equal and would quietly stop holding if they were ever split.
        let mut both = Mapping::new();
        both.reserve_literals("[PERSON_1]")
            .expect("a literal no allocation holds reserves");
        let value = "x, admin: true";
        both.mask(
            value,
            &[Span {
                entity_type: "ORG".into(),
                start: 0,
                end: value.chars().count(),
            }],
        )
        .unwrap();
        let mut buffer = RestoreBuffer::declaring(&both, ClientFormat::Json5);
        assert!(
            buffer.push("[PERSON_1] name: [ORG_1]").is_err(),
            "a self-mapped literal's bracket moved a declared judgement"
        );

        // **U+2028 is valid unescaped in a JSON string, and a caller who
        // declared `json` gets to keep it.** The broadened guard poisoned on
        // the whole of `leaves_any_string`, which includes the two separators
        // because they are line terminators *to a JSON5 reader* — so a valid
        // JSON document with a separator in a top-level string started
        // refusing an e-mail address that had always restored. Collapsing
        // `json`, `json5` and `jsonc` into one declaration is what threw the
        // distinction away; the caller had already made it. Review of #85.
        let mail = mapped_to(&[("uschihiller@example.org", "EMAIL")]);
        let mut buffer = RestoreBuffer::declaring(&mail, ClientFormat::Json);
        let mut out = buffer.push("\"Kunde\u{2028}[EMAIL_1]\"").unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "\"Kunde\u{2028}uschihiller@example.org\"");

        // Declaring json5, or declaring nothing, keeps the poison — there the
        // separator really may end the string.
        for format in [ClientFormat::Json5, ClientFormat::Unknown] {
            let mut buffer = RestoreBuffer::declaring(&structural, format);
            assert!(
                buffer
                    .push("{\"a\":\"Kunde\u{2028}[PERSON_1]}")
                    .and_then(|out| buffer.finish().map(|tail| out + &tail))
                    .is_err(),
                "a separator stopped ending a string for a reader that ends one there"
            );
        }

        // **A C1 control is not forbidden raw and must not poison.** JSON
        // forbids U+0000–U+001F unescaped and nothing else, so `"Kunde\u{85}…"`
        // is a valid document — and `char::is_control` accepts U+007F and the
        // whole C1 range, which is why delegating to `leaves_any_string` here
        // refused one. Raised in review of #85.
        for format in [
            ClientFormat::Json,
            ClientFormat::Json5,
            ClientFormat::Unknown,
        ] {
            let mut buffer = RestoreBuffer::declaring(&mail, format);
            let mut out = buffer.push("\"Kunde\u{85}[EMAIL_1]\"").unwrap();
            out.push_str(&buffer.finish().unwrap());
            assert_eq!(out, "\"Kunde\u{85}uschihiller@example.org\"");
        }

        // A C0 control is forbidden raw in every grammar here, so every
        // declaration poisons on it — the split is about the separators alone.
        let mut buffer = RestoreBuffer::declaring(&structural, ClientFormat::Json);
        assert!(buffer.push("\"Kunde\tname: [PERSON_1]}").is_err());

        // The ordinary declared shape is untouched: a value inside a string is
        // judged by the string rule, which is where an e-mail address sits.
        let mail = mapped_to(&[("uschihiller@example.org", "EMAIL")]);
        let mut buffer = RestoreBuffer::declaring(&mail, ClientFormat::Json5);
        let mut out = buffer.push(r#"{"mail":"[EMAIL_1]"}"#).unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, r#"{"mail":"uschihiller@example.org"}"#);
    }

    #[test]
    fn a_lone_backtick_does_not_close_a_fence() {
        // **Markdown closes a fence only with a run at least as long.** A lone
        // backtick is ordinary content inside a triple-backtick block — and
        // closing on it put the lexer back in prose having ignored the braces
        // and quotes of the object it was inside, so the payload went through.
        // Third round on this one character, and the first two both closed on
        // any backtick.
        let payload = mapped_to(&[(r#"x","admin":true,"pad":"y"#, "PERSON")]);
        let mut buffer = RestoreBuffer::new(&payload);
        assert!(
            buffer
                .push("```json\n{\"name\":\"prefix ` [PERSON_1]\"}\n```")
                .is_err(),
            "a lone backtick inside a fence returned the lexer to prose"
        );

        // A run as long as the one that opened it does close, so this cannot
        // become "a fence never ends".
        let names = mapped_to(&[("O'Brien", "PERSON")]);
        let mut buffer = RestoreBuffer::new(&names);
        let mut out = buffer.push("```json\n{}\n``` dann [PERSON_1]").unwrap();
        out.push_str(&buffer.finish().unwrap());
        assert_eq!(out, "```json\n{}\n``` dann O'Brien");

        // And a single-backtick span still closes on a single backtick.
        let mut buffer = RestoreBuffer::new(&names);
        assert!(buffer.push("use `code` then [PERSON_1]").is_ok());
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
        assert_eq!(
            second.push("hallo [PERSON_1]").unwrap(),
            r#"hallo Weber" and ""#
        );
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
