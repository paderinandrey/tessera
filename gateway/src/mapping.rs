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
    fn masking_is_offset_correct_on_multibyte_text() {
        // The detector counts characters; Rust slices bytes.
        let mut mapping = Mapping::new();
        let masked = mapping.mask("Grüße an Weber", &[span("PERSON", 9, 14)]);
        assert_eq!(masked, "Grüße an [PERSON_1]");
    }
}
