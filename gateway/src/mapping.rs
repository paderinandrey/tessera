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
    #[error("a tool document is nested deeper than this gateway will walk")]
    TooDeep,
    #[error("a tool document carries more values than this gateway will walk")]
    TooLarge,
    #[error(
        "masked strings do not correspond to a document's text leaves ({0}); the request is \
             refused rather than served with a value misplaced or left in"
    )]
    MaskCountMismatch(&'static str),
}

/// The longest entity type that can be written as a placeholder.
///
/// Nothing in this module checks it against an incoming type any more: a name
/// is taken only when it is in `ENTITY_TYPES`, and what holds that list to this
/// bound is `every_declared_type_fits_a_streamed_placeholder` below. The bound
/// is worth asserting because restoration in a stream holds back a fixed number
/// of bytes while a token completes, so a longer name would be released as
/// ordinary text and reach the client unrestored: `[` + 40 + `_` + at most 20
/// digits + `]` is 63 bytes, inside `stream::MAX_HELD`.
///
/// Its one use at runtime is `audit::is_entity_type`, bounding what may become
/// a key in the journal.
pub const MAX_ENTITY_TYPE: usize = 40;

/// The entity types this gateway's detector declares — eight from its
/// identifier catalog and fourteen from its NER configuration.
///
/// The list lives here, and not behind a question to the detector, because the
/// detector's response is what it defends against: a compromised one asked to
/// declare its own vocabulary would simply declare a submitted value to be a
/// type. `scripts/check_entity_types.py` fails CI when this list and the
/// catalogs disagree, so adding a type stays a deliberate change in two places
/// rather than a silent divergence.
pub const ENTITY_TYPES: [&str; 22] = [
    // Deterministic (identifiers.yaml)
    "CH_AVS",
    "CREDIT_CARD",
    "DE_STEUERNUMMER",
    "DE_STEUER_ID",
    "EMAIL",
    "FR_NIF",
    "FR_NIR",
    "IBAN",
    // Quasi-identifiers (ner.yaml)
    "LOCATION",
    "ORG",
    "PERSON",
    // GDPR Article 9 special categories (ner.yaml)
    "BIOMETRIC",
    "ETHNICITY",
    "GENETIC",
    "HEALTH",
    "PHILOSOPHICAL_BELIEF",
    "POLITICAL_AFFILIATION",
    "POLITICAL_OPINION",
    "RELIGION",
    "SEXUAL_ORIENTATION",
    "SEX_LIFE",
    "TRADE_UNION",
];

/// What a span masks as when its type is not one of ours. The value is hidden
/// either way; what is lost is the model knowing what kind of thing it was.
/// Deliberately absent from the detector's catalogs — a detector returning it
/// would be indistinguishable from this fallback.
pub const REDACTED_TYPE: &str = "REDACTED";

#[derive(Debug, Default, Clone)]
pub struct Mapping {
    by_value: HashMap<String, String>,
    by_placeholder: HashMap<String, String>,
    /// Placeholders in the order they were issued. A session commits in this
    /// order, so a cap keeps the earliest values rather than an arbitrary set.
    /// Literals reserved from the caller's own text are deliberately absent:
    /// they map to themselves and name nobody.
    order: Vec<String>,
    next: usize,
    /// How many spans arrived with a type outside our vocabulary. Reported once
    /// per request rather than per span: a detector that disagrees about types
    /// disagrees about all of them, and one line per span would be a flood.
    redacted: usize,
}

impl Mapping {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many values this mapping has issued placeholders for.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// How many spans this mapping had to mask under the generic type.
    pub fn redacted_count(&self) -> usize {
        self.redacted
    }

    pub fn mask(&mut self, text: &str, spans: &[Span]) -> Result<String, MappingError> {
        // Bail out before touching any state: `placeholder_for` never fails
        // (see its own body), so this is the only way `mask` can return
        // `Err`, and checking it up front means the loop below never has to.
        check_spans(text, spans)?;

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
            result.extend(&chars[cursor..span.start]);
            let value: String = chars[span.start..span.end].iter().collect();
            result.push_str(&self.placeholder_for(&span.entity_type, value)?);
            cursor = span.end;
        }
        result.extend(&chars[cursor..]);
        Ok(result)
    }

    /// Commit `other`'s new pairs, in the order they were issued, until `cap`
    /// is reached. `other` is the copy a request masked with; `self` is the
    /// session's table.
    ///
    /// The counter moves past pairs that were declined. A number that already
    /// named somebody in a request that reached the provider must never be
    /// issued to a second value, or a later response restores the wrong name.
    pub fn absorb(&mut self, other: &Mapping, cap: usize) {
        for placeholder in &other.order {
            if self.by_placeholder.contains_key(placeholder) {
                continue;
            }
            if self.order.len() >= cap {
                break;
            }
            let Some(value) = other.by_placeholder.get(placeholder) else {
                continue;
            };
            self.by_value.insert(value.clone(), placeholder.clone());
            self.by_placeholder
                .insert(placeholder.clone(), value.clone());
            self.order.push(placeholder.clone());
        }
        self.next = self.next.max(other.next);
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
        // Syntax cannot tell a type name from a value shaped like one, and
        // `WEBER` for a span covering WEBER passes any grammar. So the name is
        // taken only when it is one we declared; anything else is still masked,
        // under a name that carries nothing of the value.
        let entity_type = if ENTITY_TYPES.contains(&entity_type) {
            entity_type
        } else {
            self.redacted += 1;
            REDACTED_TYPE
        };
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
        self.order.push(placeholder.clone());
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

/// How deep a client's document may nest, and how many values it may hold.
/// `restore_value` needs neither: it walks a provider's own response envelope,
/// whose shape is the provider's business. This walk is reachable from a
/// client's tool arguments, which are untrusted input — a document nested ten
/// thousand deep would end the process by exhausting the stack, and the
/// recursion below is the one that would do it.
pub const MAX_JSON_DEPTH: usize = 64;
pub const MAX_JSON_NODES: usize = 10_000;

/// A value a document carries, in the order the walk finds it. Keys are absent
/// by construction: this walk descends into values and never yields a name.
#[derive(Debug, Clone, PartialEq)]
pub enum Leaf {
    Text(String),
    /// Rendered as the client wrote it. A card number, a German tax ID and a
    /// French NIR are digits alone, so a number has to be looked at — but it is
    /// never replaced, because a schema that declared a number may reject a
    /// string.
    Number(String),
}

// Constructed by task 4's detection call; nothing in this task builds one yet.
#[allow(dead_code)]
pub fn json_leaves(value: &Value) -> Result<Vec<Leaf>, MappingError> {
    let mut leaves = Vec::new();
    let mut nodes = 0usize;
    walk(value, 0, &mut nodes, &mut leaves)?;
    Ok(leaves)
}

fn walk(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
    leaves: &mut Vec<Leaf>,
) -> Result<(), MappingError> {
    if depth > MAX_JSON_DEPTH {
        return Err(MappingError::TooDeep);
    }
    *nodes += 1;
    if *nodes > MAX_JSON_NODES {
        return Err(MappingError::TooLarge);
    }
    match value {
        Value::String(text) => leaves.push(Leaf::Text(text.clone())),
        Value::Number(number) => leaves.push(Leaf::Number(number.to_string())),
        Value::Array(items) => {
            for item in items {
                walk(item, depth + 1, nodes, leaves)?;
            }
        }
        Value::Object(fields) => {
            // `fields` is iterated, never yielded: a key is the client's
            // dispatch, and masking one would break the call it dispatches.
            for (_key, item) in fields {
                walk(item, depth + 1, nodes, leaves)?;
            }
        }
        Value::Bool(_) | Value::Null => {}
    }
    Ok(())
}

/// The mirror of `restore_value`, and the second half of masking: the walk
/// above collected the leaves, `mask_all` detected them, and this puts the
/// masked strings back where they came from. Order is the only correspondence,
/// which is why both walks are the same function shape.
// Constructed by task 4's detection call; nothing in this task builds one yet.
#[allow(dead_code)]
pub fn replace_text_leaves(value: &Value, masked: &[String]) -> Result<Value, MappingError> {
    let mut next = 0usize;
    let mut nodes = 0usize;
    let result = replace(value, 0, &mut nodes, masked, &mut next)?;
    if next != masked.len() {
        return Err(MappingError::MaskCountMismatch(
            "more masked strings than text leaves",
        ));
    }
    Ok(result)
}

fn replace(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
    masked: &[String],
    next: &mut usize,
) -> Result<Value, MappingError> {
    if depth > MAX_JSON_DEPTH {
        return Err(MappingError::TooDeep);
    }
    *nodes += 1;
    if *nodes > MAX_JSON_NODES {
        return Err(MappingError::TooLarge);
    }
    Ok(match value {
        Value::String(_) => {
            let replacement = masked.get(*next).ok_or(MappingError::MaskCountMismatch(
                "fewer masked strings than text leaves",
            ))?;
            *next += 1;
            Value::String(replacement.clone())
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| replace(item, depth + 1, nodes, masked, next))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| {
                    Ok((key.clone(), replace(item, depth + 1, nodes, masked, next)?))
                })
                .collect::<Result<serde_json::Map<_, _>, MappingError>>()?,
        ),
        other => other.clone(),
    })
}

/// Whether `spans` can be applied to `text` at all: every span non-empty,
/// in range, and non-overlapping once ordered by start. This is `mask`'s
/// own well-formedness check, pulled out so a caller deciding whether a
/// detection is safe to *remember* can ask the exact question `mask` will
/// ask when it is later asked to *apply* the same spans to the same text —
/// one copy of the three conditions rather than two that could drift and
/// disagree about which spans are usable.
///
/// Order-dependent — the conditions run over spans sorted by `start`, with
/// a `cursor` tracking how far a preceding span reached — and the bound is
/// `text.chars().count()`, a character count, because that is what the
/// detector's offsets are in. A caller that sorted differently or bounded
/// by byte length would not be asking the same question `mask` asks, and
/// could cache a response `mask` would reject, or refuse one `mask` would
/// have accepted.
pub fn check_spans(text: &str, spans: &[Span]) -> Result<(), MappingError> {
    let char_count = text.chars().count();
    let mut ordered: Vec<&Span> = spans.iter().collect();
    ordered.sort_by_key(|span| span.start);
    let mut cursor = 0usize;
    for span in ordered {
        // A span that fails here means the value would stay in the text,
        // and the text is about to leave the process. Refuse instead:
        // passing it through would turn a detector contract bug into raw
        // egress.
        if span.start >= span.end {
            return Err(MappingError::BadSpan("empty or inverted"));
        }
        if span.end > char_count {
            return Err(MappingError::BadSpan("past the end of the text"));
        }
        if span.start < cursor {
            return Err(MappingError::BadSpan("overlapping"));
        }
        cursor = span.end;
    }
    Ok(())
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
    use serde_json::json;

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
    fn numbering_runs_across_types_rather_than_per_type() {
        // One counter for the whole mapping, so the number says "the nth value
        // this mapping saw" and not "the nth PERSON". A per-type counter would
        // read more naturally and would also make the number a function of the
        // type mix, which is what the two tests below rely on it not being.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask(
                "Weber DE89370400440532013000",
                &[span("PERSON", 0, 5), span("IBAN", 6, 28)],
            )
            .unwrap();
        assert_eq!(masked, "[PERSON_1] [IBAN_2]");
    }

    #[test]
    fn a_growing_prefix_reissues_the_same_numbers() {
        // What lets a client work without `X-Tessera-Session` at all. A harness
        // resends the whole conversation every turn and gets a fresh mapping
        // each time; numbers are assigned by order of first appearance, and an
        // appended turn leaves that order alone, so the numbering reproduces
        // itself with no store behind it. Numbering that depended on anything
        // but appearance order would take that away silently.
        let mut turn_one = Mapping::new();
        let first = turn_one
            .mask(
                "Weber und Bauer",
                &[span("PERSON", 0, 5), span("PERSON", 10, 15)],
            )
            .unwrap();
        assert_eq!(first, "[PERSON_1] und [PERSON_2]");

        let mut turn_two = Mapping::new();
        let second = turn_two
            .mask(
                "Weber und Bauer und Klein",
                &[
                    span("PERSON", 0, 5),
                    span("PERSON", 10, 15),
                    span("PERSON", 20, 25),
                ],
            )
            .unwrap();
        assert_eq!(second, "[PERSON_1] und [PERSON_2] und [PERSON_3]");
    }

    #[test]
    fn a_truncated_prefix_renumbers_but_stays_self_consistent() {
        // A harness trims history to fit its context window, which changes who
        // appears first: Bauer was the second value last turn and is the first
        // one now. That is safe only because the request is masked and restored
        // through one mapping and the client holds the real values, so there is
        // no cross-request placeholder continuity to violate. The round-trip is
        // the half worth asserting — the number alone proves nothing.
        let mut turn_one = Mapping::new();
        assert_eq!(
            turn_one
                .mask(
                    "Weber und Bauer",
                    &[span("PERSON", 0, 5), span("PERSON", 10, 15)]
                )
                .unwrap(),
            "[PERSON_1] und [PERSON_2]"
        );

        let mut truncated = Mapping::new();
        let masked = truncated
            .mask("Bauer allein", &[span("PERSON", 0, 5)])
            .unwrap();
        assert_eq!(masked, "[PERSON_1] allein");
        assert_eq!(truncated.restore(&masked).unwrap(), "Bauer allein");
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
    fn a_type_outside_the_grammar_is_masked_rather_than_refused() {
        // It used to be refused. Masking is the same protection and does not
        // break a gateway whose detector has grown a type it does not know.
        let mut mapping = Mapping::new();
        assert_eq!(
            mapping
                .mask("Weber", &[span("person", 0, 5)])
                .expect("masked, not refused"),
            "[REDACTED_1]"
        );
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

    #[test]
    fn absorb_commits_new_pairs_in_the_order_they_were_issued() {
        let mut session = Mapping::new();
        let mut work = session.clone();
        work.mask(
            "Weber und Meier",
            &[span("PERSON", 0, 5), span("PERSON", 10, 15)],
        )
        .unwrap();
        session.absorb(&work, 10);
        assert_eq!(
            session.restore("[PERSON_1] und [PERSON_2]").unwrap(),
            "Weber und Meier"
        );
    }

    #[test]
    fn absorb_stops_at_the_cap_and_keeps_the_earliest() {
        let mut session = Mapping::new();
        let mut work = session.clone();
        work.mask(
            "Weber und Meier",
            &[span("PERSON", 0, 5), span("PERSON", 10, 15)],
        )
        .unwrap();
        session.absorb(&work, 1);
        assert_eq!(session.len(), 1);
        assert_eq!(session.restore("[PERSON_1]").unwrap(), "Weber");
        assert!(session.restore("[PERSON_2]").is_err());
    }

    #[test]
    fn absorb_carries_the_counter_past_what_it_declined() {
        let mut session = Mapping::new();
        let mut work = session.clone();
        work.mask(
            "Weber und Meier",
            &[span("PERSON", 0, 5), span("PERSON", 10, 15)],
        )
        .unwrap();
        // [PERSON_2] went to Meier and was declined by the cap. It must not be
        // reissued: the number already named somebody in a request that shipped.
        session.absorb(&work, 1);
        let mut next = session.clone();
        next.mask("Schmidt", &[span("PERSON", 0, 7)]).unwrap();
        assert_eq!(next.restore("[PERSON_3]").unwrap(), "Schmidt");
    }

    #[test]
    fn absorb_does_not_carry_the_redacted_count_into_the_session() {
        // The count describes one request. A session that inherited it would
        // repeat an old request's disagreement on every later turn, forever.
        let mut session = Mapping::new();
        let mut work = session.clone();
        work.mask("WEBER", &[span("WEBER", 0, 5)]).unwrap();
        assert_eq!(work.redacted_count(), 1);

        session.absorb(&work, 10);
        assert_eq!(session.redacted_count(), 0);
    }

    #[test]
    fn a_clone_does_not_write_back_to_its_source() {
        let session = Mapping::new();
        let mut work = session.clone();
        work.mask("Weber", &[span("PERSON", 0, 5)]).unwrap();
        // The request failed before absorb; the session never heard of Weber.
        assert!(session.restore("[PERSON_1]").is_err());
    }

    #[test]
    fn absorb_ignores_placeholders_reserved_from_the_callers_own_text() {
        let mut session = Mapping::new();
        let mut work = session.clone();
        // "[PERSON_9]" is 10 characters, so "Weber" starts at 16.
        work.mask("[PERSON_9] traf Weber", &[span("PERSON", 16, 21)])
            .unwrap();
        session.absorb(&work, 10);
        assert_eq!(session.len(), 1);
        assert_eq!(session.restore("[PERSON_1]").unwrap(), "Weber");
        // The literal was reserved so an echo survives, not committed: it names
        // nobody, and a session that remembered it would restore it to itself
        // for every later caller of this conversation.
        assert!(session.restore("[PERSON_9]").is_err());
    }

    #[test]
    fn a_value_masquerading_as_a_type_does_not_reach_the_placeholder() {
        // The leak this slice exists for: a detector that returns the span's
        // own value as its type would otherwise put that value in the token
        // the provider receives.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask("WEBER", &[span("WEBER", 0, 5)])
            .expect("an unknown type is masked, not refused");

        assert_eq!(masked, "[REDACTED_1]");
        assert!(
            !masked.contains("WEBER"),
            "the submitted value reached the placeholder: {masked}"
        );
    }

    #[test]
    fn every_declared_type_keeps_its_own_name() {
        // Without this, a fix that rejects everything passes the test above.
        for entity_type in ENTITY_TYPES {
            let mut mapping = Mapping::new();
            let masked = mapping
                .mask("Weber", &[span(entity_type, 0, 5)])
                .expect("a declared type masks");
            assert_eq!(
                masked,
                format!("[{entity_type}_1]"),
                "{entity_type} did not keep its name"
            );
        }
    }

    #[test]
    fn every_name_a_placeholder_can_carry_is_one_restoration_recognises() {
        // What is left of the old `[A-Z_]` input check, moved to the only names
        // that can still reach a placeholder: the list, and the fallback every
        // other type — empty, lower-case, longer than MAX_ENTITY_TYPE — is
        // masked under. `is_placeholder` is what restoration uses to decide a
        // token is ours, so a name it does not admit would mask cleanly and
        // then be handed to the client instead of the value, as a success.
        for entity_type in ENTITY_TYPES.iter().chain([&REDACTED_TYPE]) {
            assert!(
                is_placeholder(&format!("[{entity_type}_1]")),
                "{entity_type} cannot be written as a placeholder restoration recognises"
            );
        }
    }

    #[test]
    fn two_unknown_types_stay_distinguishable() {
        // REDACTED draws from the shared counter, so two values do not collapse
        // into one token and tell the model they are the same thing.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask("WEBER MEIER", &[span("WEBER", 0, 5), span("MEIER", 6, 11)])
            .expect("both are masked");

        assert_eq!(masked, "[REDACTED_1] [REDACTED_2]");
    }

    #[test]
    fn an_unknown_type_restores_to_its_value() {
        // Masking under a generic name must not cost restoration.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask("WEBER", &[span("WEBER", 0, 5)])
            .expect("masked");
        assert_eq!(mapping.restore(&masked).expect("restores"), "WEBER");
    }

    #[test]
    fn redacted_is_not_a_type_the_detector_can_claim() {
        // A detector returning REDACTED would be indistinguishable from the
        // gateway's own fallback, so the vocabulary must not contain it.
        assert!(!ENTITY_TYPES.contains(&REDACTED_TYPE));
    }

    #[test]
    fn the_mapping_counts_what_it_had_to_redact() {
        let mut mapping = Mapping::new();
        mapping
            .mask("WEBER Weber", &[span("WEBER", 0, 5), span("PERSON", 6, 11)])
            .expect("masked");

        assert_eq!(mapping.redacted_count(), 1, "one unknown type, one count");
    }

    #[test]
    fn every_declared_type_fits_a_streamed_placeholder() {
        // MAX_ENTITY_TYPE stops being an input check and becomes an assertion
        // about this list: a longer name would be released as ordinary text by
        // the stream's hold-back buffer and reach the client unrestored.
        for entity_type in ENTITY_TYPES {
            assert!(
                entity_type.len() <= MAX_ENTITY_TYPE,
                "{entity_type} is too long to survive a stream"
            );
        }
    }

    #[test]
    fn a_walk_reports_string_leaves_in_order_and_ignores_keys() {
        let document = json!({
            "b_second": "Weber",
            "a_first": {"nested": "Meier"},
            "list": ["Schmidt", 7, true, null]
        });
        let leaves = json_leaves(&document).unwrap();
        assert_eq!(
            leaves,
            vec![
                Leaf::Text("Meier".to_owned()),
                Leaf::Text("Weber".to_owned()),
                Leaf::Text("Schmidt".to_owned()),
                Leaf::Number("7".to_owned()),
            ],
            "keys are not leaves, and booleans and null carry no personal data"
        );
    }

    #[test]
    fn replacement_puts_masked_text_back_in_walk_order() {
        // This crate does not enable serde_json's `preserve_order` feature, so
        // `Value::Object` is backed by a `BTreeMap` and the walk visits keys
        // alphabetically: "n", "where", "who" — not the order they were
        // written in. "where" (Bern) is therefore the first text leaf and
        // takes the first masked string; "who" (Weber) is the second. The two
        // masked strings are deliberately distinguishable so a swap between
        // them shows up here rather than being absorbed. If this crate ever
        // gains `preserve_order`, the walk order changes and this expectation
        // must be revisited — see `json_leaves_and_replace_text_leaves_agree_on_position`
        // below for the invariant that holds regardless of which order rule
        // is in force.
        let document = json!({"who": "Weber", "n": 7, "where": "Bern"});
        let masked = vec!["[PERSON_1]".to_owned(), "[LOCATION_2]".to_owned()];
        let result = replace_text_leaves(&document, &masked).unwrap();
        assert_eq!(
            result,
            json!({"who": "[LOCATION_2]", "n": 7, "where": "[PERSON_1]"}),
            "numbers keep their type and their value; keys visit alphabetically, \
             so \"where\" is masked before \"who\""
        );
    }

    #[test]
    fn json_leaves_and_replace_text_leaves_agree_on_position() {
        // The invariant the rest of the slice actually rests on is not "the
        // walk visits keys in this particular order" — it is that
        // `json_leaves` and `replace_text_leaves` visit leaves in the *same*
        // order as each other, whatever that order is. This test pins that
        // correspondence directly, and unlike the test above, survives a
        // change to the ordering rule.
        let document = json!({
            "who": "Weber",
            "location": {"city": "Bern", "notes": ["Schmidt", 7]},
            "count": 3
        });
        let text_leaf_count = json_leaves(&document)
            .unwrap()
            .into_iter()
            .filter(|leaf| matches!(leaf, Leaf::Text(_)))
            .count();
        // Each replacement encodes the position it should land in.
        let masked: Vec<String> = (0..text_leaf_count).map(|i| format!("leaf-{i}")).collect();
        let replaced = replace_text_leaves(&document, &masked).unwrap();
        let text_leaves_after: Vec<String> = json_leaves(&replaced)
            .unwrap()
            .into_iter()
            .filter_map(|leaf| match leaf {
                Leaf::Text(text) => Some(text),
                Leaf::Number(_) => None,
            })
            .collect();
        for (index, text) in text_leaves_after.iter().enumerate() {
            assert_eq!(
                *text,
                format!("leaf-{index}"),
                "leaf at walk position {index} did not receive the replacement built for it"
            );
        }
    }

    #[test]
    fn a_document_past_the_depth_bound_is_refused() {
        let mut document = json!("Weber");
        for _ in 0..(MAX_JSON_DEPTH + 1) {
            document = json!([document]);
        }
        assert!(matches!(json_leaves(&document), Err(MappingError::TooDeep)));
    }

    #[test]
    fn a_document_past_the_node_bound_is_refused() {
        let wide: Vec<Value> = (0..=MAX_JSON_NODES).map(|n| json!(n)).collect();
        assert!(matches!(
            json_leaves(&Value::Array(wide)),
            Err(MappingError::TooLarge)
        ));
    }
}
