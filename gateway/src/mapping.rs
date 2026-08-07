use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

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
    #[error(
        "detector reported an unusable span ({0}); the request is refused rather than \
             forwarded with the value still in it"
    )]
    BadSpan(&'static str),
    #[error(
        "entity type {0:?} cannot be written as a restorable placeholder; the request is \
             refused rather than masked with a token restoration would not recognize"
    )]
    BadEntityType(String),
}

/// The longest entity type that can be written as a placeholder. Restoration in
/// a stream holds back a bounded number of bytes while a token completes, so a
/// placeholder longer than that bound would be released as ordinary text and
/// reach the client unrestored. Bounding it here makes that impossible rather
/// than unlikely: `[` + 40 + `_` + at most 20 digits + `]` is 63 bytes, inside
/// `stream::MAX_HELD`.
pub const MAX_ENTITY_TYPE: usize = 40;

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

    pub fn mask(&mut self, text: &str, spans: &[Span]) -> Result<String, MappingError> {
        // Character indices, because that is what the detector reports.
        let chars: Vec<char> = text.chars().collect();
        let mut ordered: Vec<&Span> = spans.iter().collect();
        ordered.sort_by_key(|span| span.start);

        // A placeholder-shaped token already in the caller's text would be
        // indistinguishable from one we issue. Mapping it to itself reserves
        // the number and makes an echo restore to exactly what was sent.
        self.reserve_literals(text);

        let mut result = String::with_capacity(text.len());
        let mut cursor = 0usize;
        for span in ordered {
            // A span we cannot apply means the value stays in the text, and the
            // text is about to leave the process. Refuse instead: skipping here
            // would turn a detector contract bug into raw egress.
            if span.start >= span.end {
                return Err(MappingError::BadSpan("empty or inverted"));
            }
            if span.end > chars.len() {
                return Err(MappingError::BadSpan("past the end of the text"));
            }
            if span.start < cursor {
                return Err(MappingError::BadSpan("overlapping"));
            }
            result.extend(&chars[cursor..span.start]);
            let value: String = chars[span.start..span.end].iter().collect();
            result.push_str(&self.placeholder_for(&span.entity_type, value)?);
            cursor = span.end;
        }
        result.extend(&chars[cursor..]);
        Ok(result)
    }

    /// Map every placeholder-shaped token already present to itself, so it is
    /// never issued for a detected value and an echo restores unchanged.
    fn reserve_literals(&mut self, text: &str) {
        let mut rest = text;
        while let Some(open) = rest.find('[') {
            let from_open = &rest[open..];
            let Some(close) = from_open.find(']') else {
                return;
            };
            let candidate = &from_open[..=close];
            if is_placeholder(candidate) {
                self.by_placeholder
                    .entry(candidate.to_owned())
                    .or_insert_with(|| candidate.to_owned());
                rest = &from_open[close + 1..];
            } else {
                rest = &from_open[1..];
            }
        }
    }

    fn placeholder_for(
        &mut self,
        entity_type: &str,
        value: String,
    ) -> Result<String, MappingError> {
        if let Some(existing) = self.by_value.get(&value) {
            return Ok(existing.clone());
        }
        // The detector's entity_type is an unrestricted string, but only types
        // matching the restoration grammar produce a token restoration will
        // recognize. Anything else would sail through masked and come back
        // unrestored, so it refuses instead.
        if entity_type.is_empty()
            || entity_type.len() > MAX_ENTITY_TYPE
            || !entity_type
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_')
        {
            return Err(MappingError::BadEntityType(entity_type.to_owned()));
        }
        // Skip numbers already taken by a literal in the caller's own text.
        let placeholder = loop {
            self.next += 1;
            let candidate = format!("[{entity_type}_{}]", self.next);
            if !self.by_placeholder.contains_key(&candidate) {
                break candidate;
            }
        };
        self.by_value.insert(value.clone(), placeholder.clone());
        self.by_placeholder.insert(placeholder.clone(), value);
        Ok(placeholder)
    }

    /// Restore every string in a value. Used for upstream envelopes, whose shape
    /// is the provider's business but which may quote the masked text back.
    pub fn restore_value(&self, value: &Value) -> Result<Value, MappingError> {
        Ok(match value {
            Value::String(text) => Value::String(self.restore(text)?),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| self.restore_value(item))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, item)| Ok((key.clone(), self.restore_value(item)?)))
                    .collect::<Result<serde_json::Map<_, _>, MappingError>>()?,
            ),
            other => other.clone(),
        })
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
                rest = &from_open[close + 1..];
            } else {
                // Not a placeholder: consume only this bracket and keep looking.
                // Swallowing the whole candidate would step over a real
                // placeholder nested inside it, as in "[see [PERSON_1]]".
                result.push('[');
                rest = &from_open[1..];
            }
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
        && entity_type
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_')
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(entity_type: &str, start: usize, end: usize) -> Span {
        Span {
            entity_type: entity_type.to_owned(),
            start,
            end,
        }
    }

    #[test]
    fn masking_replaces_a_span_with_a_typed_placeholder() {
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask("Herr Weber schreibt", &[span("PERSON", 5, 10)])
            .unwrap();
        assert_eq!(masked, "Herr [PERSON_1] schreibt");
    }

    #[test]
    fn later_spans_do_not_shift_earlier_ones() {
        // Replacing left to right would invalidate every offset after the first.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask(
                "Weber und Schmidt",
                &[span("PERSON", 0, 5), span("PERSON", 10, 17)],
            )
            .unwrap();
        assert_eq!(masked, "[PERSON_1] und [PERSON_2]");
    }

    #[test]
    fn the_same_value_keeps_the_same_placeholder() {
        // Two placeholders for one person would tell the model there are two.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask(
                "Weber schrieb an Weber",
                &[span("PERSON", 0, 5), span("PERSON", 17, 22)],
            )
            .unwrap();
        assert_eq!(masked, "[PERSON_1] schrieb an [PERSON_1]");
    }

    #[test]
    fn numbering_continues_across_calls() {
        // One request carries several texts; they share a mapping.
        let mut mapping = Mapping::new();
        mapping.mask("Weber", &[span("PERSON", 0, 5)]).unwrap();
        let second = mapping.mask("Schmidt", &[span("PERSON", 0, 7)]).unwrap();
        assert_eq!(second, "[PERSON_2]");
    }

    #[test]
    fn restoring_puts_the_values_back() {
        let mut mapping = Mapping::new();
        mapping.mask("Weber", &[span("PERSON", 0, 5)]).unwrap();
        assert_eq!(
            mapping.restore("Hallo [PERSON_1]!").unwrap(),
            "Hallo Weber!"
        );
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
        assert_eq!(
            Mapping::new().restore("nothing here").unwrap(),
            "nothing here"
        );
    }

    #[test]
    fn bracketed_text_that_is_not_a_placeholder_survives() {
        assert_eq!(
            Mapping::new().restore("see [1] and [note]").unwrap(),
            "see [1] and [note]"
        );
    }

    #[test]
    fn a_span_past_the_end_refuses_rather_than_leaking() {
        // A detector contract bug must not become raw egress.
        let mut mapping = Mapping::new();
        assert!(mapping.mask("Weber", &[span("PERSON", 0, 99)]).is_err());
    }

    #[test]
    fn an_overlapping_span_refuses() {
        let mut mapping = Mapping::new();
        let spans = [span("PERSON", 0, 5), span("IBAN", 3, 8)];
        assert!(mapping.mask("Weber schreibt", &spans).is_err());
    }

    #[test]
    fn an_inverted_span_refuses() {
        let mut mapping = Mapping::new();
        assert!(mapping.mask("Weber", &[span("PERSON", 4, 2)]).is_err());
    }

    #[test]
    fn a_placeholder_nested_after_another_bracket_is_restored() {
        // "[see [PERSON_1]]": pairing every '[' with the next ']' would step
        // straight over the real placeholder.
        let mut mapping = Mapping::new();
        mapping.mask("Weber", &[span("PERSON", 0, 5)]).unwrap();
        assert_eq!(mapping.restore("[see [PERSON_1]]").unwrap(), "[see Weber]");
    }

    #[test]
    fn a_literal_placeholder_in_the_request_is_reserved_and_survives() {
        // The caller's own "[PERSON_1]" must not become the value we masked,
        // and it must come back as it was sent.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask("[PERSON_1] fragt nach Weber", &[span("PERSON", 22, 27)])
            .unwrap();
        assert!(masked.starts_with("[PERSON_1] fragt nach ["));
        assert!(
            !masked.ends_with("[PERSON_1]"),
            "the number was reused: {masked}"
        );
        assert_eq!(
            mapping.restore(&masked).unwrap(),
            "[PERSON_1] fragt nach Weber"
        );
    }

    #[test]
    fn an_entity_type_outside_the_grammar_is_refused() {
        // "[person_1]" or "[PERSON-ROLE_1]" would not be recognized on the way
        // back, so it would reach the client unrestored.
        let mut mapping = Mapping::new();
        assert!(mapping.mask("Weber", &[span("person", 0, 5)]).is_err());
        assert!(mapping.mask("Weber", &[span("PERSON-ROLE", 0, 5)]).is_err());
        assert!(mapping.mask("Weber", &[span("", 0, 5)]).is_err());
    }

    #[test]
    fn an_entity_type_too_long_to_survive_a_stream_is_refused() {
        // Restoration in a stream holds back a bounded number of bytes. A
        // placeholder longer than that bound would be released as text and
        // handed to the client unrestored, so it is never issued.
        let mut mapping = Mapping::new();
        let long = "A".repeat(MAX_ENTITY_TYPE + 1);
        assert!(mapping.mask("Weber", &[span(&long, 0, 5)]).is_err());
        let longest = "A".repeat(MAX_ENTITY_TYPE);
        assert!(mapping.mask("Weber", &[span(&longest, 0, 5)]).is_ok());
    }

    #[test]
    fn masking_is_offset_correct_on_multibyte_text() {
        // The detector counts characters; Rust slices bytes.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask("Grüße an Weber", &[span("PERSON", 9, 14)])
            .unwrap();
        assert_eq!(masked, "Grüße an [PERSON_1]");
    }
}
