//! Restoration of a response that arrives in pieces.
//!
//! The buffered path hands a whole string to `Mapping::restore`. A stream has
//! no whole string: `[PERSON_1]` arrives as `[PER` in one event and `SON_1]` in
//! the next, and the HTTP chunks under those events break at arbitrary byte
//! offsets, including the middle of a UTF-8 character. Restoring per chunk
//! would emit `[PER` to the client and never recognize the token.

use crate::mapping::{Mapping, MappingError};

/// A `[` that never closes would suspend the stream. Past this many bytes the
/// bracket cannot begin a placeholder, so it is emitted as ordinary text.
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
}

impl SseFramer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some((end, width)) = find_blank_line(&self.buffer) {
            let block = self.buffer[..end].to_vec();
            self.buffer.drain(..end + width);
            if let Some(event) = parse_event(&block) {
                events.push(event);
            }
        }
        events
    }

    /// Bytes left over when the body ended. A stream that stops without its
    /// final blank line must not swallow the text it already sent.
    pub fn finish(&mut self) -> Option<SseEvent> {
        let block = std::mem::take(&mut self.buffer);
        parse_event(&block)
    }
}

/// Offset and width of the first blank line, in either line-ending convention.
/// Scanning forward matters: a `\r\n\r\n` delimiter contains no `\n\n`, so
/// searching for `\n\n` across the whole buffer first would run past it and
/// merge two events into one.
fn find_blank_line(buffer: &[u8]) -> Option<(usize, usize)> {
    for index in 0..buffer.len() {
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
        assert_eq!(events[0].data.as_deref(), Some("{\"a\":1}"));
        assert_eq!(events[1].name, None);
        assert_eq!(events[1].data.as_deref(), Some("[DONE]"));
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
        assert_eq!(events[0].data.as_deref(), Some("{\"t\":\"Grüße\"}"));
    }

    #[test]
    fn crlf_delimited_events_are_framed_separately() {
        // Searching the whole buffer for "\n\n" first would merge these two.
        let mut framer = SseFramer::new();
        let events = framer.push(b"data: one\r\n\r\ndata: two\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data.as_deref(), Some("one"));
        assert_eq!(events[1].data.as_deref(), Some("two"));
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        let mut framer = SseFramer::new();
        let events = framer.push(b"data: one\ndata: two\n\n");
        assert_eq!(events[0].data.as_deref(), Some("one\ntwo"));
    }

    #[test]
    fn trailing_bytes_without_a_blank_line_are_still_delivered() {
        // A body that ends without the final blank line must not swallow text.
        let mut framer = SseFramer::new();
        assert!(framer.push(b"data: tail").is_empty());
        assert_eq!(framer.finish().unwrap().data.as_deref(), Some("tail"));
    }

    #[test]
    fn other_fields_survive_the_round_trip() {
        // Dropping `id:` would break a client's resume.
        let mut framer = SseFramer::new();
        let events = framer.push(b"id: 7\nevent: ping\ndata: {}\n\n");
        assert_eq!(events[0].render(), "event: ping\nid: 7\ndata: {}\n\n");
    }

    #[test]
    fn an_event_without_data_renders_no_data_line() {
        let mut framer = SseFramer::new();
        let events = framer.push(b"event: ping\n\n");
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
