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
