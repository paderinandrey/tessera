use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::Value;

/// A span as the detector reports it: offsets are in characters, not bytes.
///
/// `PartialEq` so that `Joined::split`'s rebasing can be asserted as a value
/// rather than field by field — the arithmetic is the whole of that function.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Span {
    pub entity_type: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MappingError {
    /// Carries the placeholder for tests and logs, deliberately not for the
    /// message — the same split `PlaceholderKey` below makes, and for the same
    /// reason. A client is never supposed to see a placeholder; restoration
    /// exists so they do not, and a refusal body (or a stream's error event) is
    /// the one path that could hand one over. This predates tool traffic: it
    /// was leaking the token on both paths long before this slice, and the rule
    /// arriving late is not a reason for it to keep doing so.
    #[error(
        "no mapping for a placeholder in the upstream response; the request is refused \
             rather than served with it"
    )]
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
    /// Two of this gateway's own walks of one document disagreeing about its
    /// leaves. Never anything a client sent — every walk is over a document
    /// already accepted — so it is a defect here and carries a 502.
    ///
    /// The prefix says "walks" rather than "masked strings" because the
    /// correspondence it guards is wider than the masking loop: `proxy::mask_all`
    /// raises it too, when the per-leaf spans `Joined::split` returned do not
    /// number the leaves `json_leaves` found. Each site says which in `{0}`.
    #[error(
        "this gateway's own walks of a tool document disagree about its leaves ({0}); the \
             request is refused rather than served with a value misplaced or left in"
    )]
    MaskCountMismatch(&'static str),
    /// Carries the key for tests and logs, deliberately *not* for the message.
    /// A placeholder is this gateway's own token and a client is never supposed
    /// to see one — restoration exists so they do not. An error body is the one
    /// path that could hand one over, so it does not.
    #[error(
        "upstream response uses a placeholder as a property name; a placeholder names a \
             value, and a key is dispatch, so the response is refused rather than served \
             with it"
    )]
    PlaceholderKey(String),
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
    // Deterministic (identifiers.yaml) — also listed, and enforced, as
    // `DETERMINISTIC_TYPES` below. Something reads that partition now, so it
    // is no longer only a note to the next person editing this array.
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

/// The half of `ENTITY_TYPES` the detector decides from the value itself — a
/// checksum, a format, a grammar — rather than from a model's reading of the
/// text around it. These are `identifiers.yaml`'s eight; the other fourteen
/// come from `ner.yaml` and are a judgement about meaning.
///
/// The distinction earns a list of its own because `proxy::mask_all` refuses a
/// request on a span in a *numeric* leaf only when the span's type is one of
/// these — the reasoning is written where that decision is made. Everywhere
/// else the two halves are treated alike.
///
/// This is not a hand-kept subset. `scripts/check_entity_types.py` holds it to
/// `identifiers.yaml` exactly and holds the complement to `ner.yaml` exactly,
/// and `the_deterministic_types_are_a_subset_of_the_vocabulary` below holds it
/// to `ENTITY_TYPES`, so a ninth identifier fails a check rather than landing
/// silently on the side of the predicate that does not refuse.
pub const DETERMINISTIC_TYPES: [&str; 8] = [
    "CH_AVS",
    "CREDIT_CARD",
    "DE_STEUERNUMMER",
    "DE_STEUER_ID",
    "EMAIL",
    "FR_NIF",
    "FR_NIR",
    "IBAN",
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
    ///
    /// Allocations, not findings: a value already mapped is returned from
    /// `by_value` before its type is looked at, so a repeat does not count
    /// again. That is the right number for the one thing that reads it — a
    /// `tracing` line about a detector whose vocabulary disagrees with ours —
    /// and it is deliberately *not* the journal's `redacted` field, which
    /// counts this request's findings rather than this mapping's allocations.
    /// The two were the same number until the journal stopped asking this one.
    pub fn redacted_count(&self) -> usize {
        self.redacted
    }

    /// Whether the provider received `value` under `entity_type`'s own name.
    ///
    /// False in three different situations, and the journal treats them alike
    /// because the provider does: the type was not one we declare and the value
    /// went up as `[REDACTED_n]`; the value was already carrying a placeholder
    /// issued for some *other* type, in this request or in an earlier turn of
    /// the session; or the value was never masked at all. What the caller is
    /// asking is "did the name in my evidence line reach the provider", and the
    /// answer is no in all three.
    pub fn named_as(&self, value: &str, entity_type: &str) -> bool {
        self.by_value
            .get(value)
            .map(|placeholder| placeholder.as_str())
            .and_then(placeholder_type)
            == Some(entity_type)
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
        //
        // This reserves what *this* text holds, which is everything a single
        // `mask` can see and not enough on its own: a request masks many
        // strings, and a literal in the second collides with a number the
        // first was already given. `proxy::mask_all` reserves across every
        // string in the request before it masks any of them, so inside a
        // request this call finds its own literals already reserved. It stays
        // because `mask` is called on its own too, and a caller masking one
        // text must not have to know about the wider pass to be correct.
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
    ///
    /// `or_insert` is deliberate, and it is also the limit of what this can
    /// do: it yields to whoever holds the key already. A literal that reaches
    /// this *after* its number was issued for a detected value is therefore
    /// not reserved, and the provider's echo of the caller's own text restores
    /// to that value. Reserving ahead of every allocation is the caller's job,
    /// and `proxy::mask_all` does it for a whole request. Across turns nothing
    /// can do it: the allocation happened before the literal existed.
    pub fn reserve_literals(&mut self, text: &str) {
        for piece in pieces(text) {
            if let Piece::Placeholder(candidate) = piece {
                self.by_placeholder
                    .entry(candidate.to_owned())
                    .or_insert_with(|| candidate.to_owned());
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
    /// is the provider's business but which may quote the masked text back —
    /// and, since tool traffic, for a `tool_use` block's arguments, whose shape
    /// is the *client's* schema and whose content the model wrote.
    ///
    /// Unbounded recursion, unlike `walk` below, and the reason it is safe is
    /// worth stating because it is not local. The primary protection is one
    /// direction earlier: a `tool_use` response mirrors a schema this gateway
    /// already refused past `MAX_JSON_DEPTH`, so a response deep enough to
    /// matter describes a request that never reached the provider.
    ///
    /// Behind that, `serde_json` bounds deserialization depth at all — which is
    /// why `disable_recursion_limit` exists to switch it off. That is the
    /// backstop, and it is a dependency's promise rather than ours: the figure
    /// is 128 today (measured), under a `serde_json = "1"` that any `cargo
    /// update` may move. Do not build on the number.
    ///
    /// A caller handing this a value assembled in memory rather than parsed has
    /// neither protection and needs its own bound.
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
                    .map(|(key, item)| {
                        // Keys are never masked going up, so a placeholder in
                        // key position did not come from us rewriting one — the
                        // model wrote it, having seen placeholders in the text.
                        // Restoring it would rename the property the client's
                        // tool reads its argument from; leaving it puts our own
                        // token in the client's hands. Both change dispatch, so
                        // neither is served.
                        if self.key_is_unserveable(key) {
                            return Err(MappingError::PlaceholderKey(key.clone()));
                        }
                        Ok((key.clone(), self.restore_value(item)?))
                    })
                    .collect::<Result<serde_json::Map<_, _>, MappingError>>()?,
            ),
            other => other.clone(),
        })
    }

    /// Whether this property name is one the client cannot be given.
    ///
    /// Two questions wear the same brackets here, and the reason this is one
    /// function with two arms rather than one rule is that they have different
    /// answers.
    ///
    /// **A key that *is* a placeholder is refused whatever this session
    /// issued.** That is the recorded ruling and it is unchanged: neither
    /// answer serves the client, because restoring renames the property their
    /// tool reads its argument from and leaving it hands them our token as a
    /// property name — and conditioning the refusal on our own map would make
    /// the status code report which numbers this session has issued.
    ///
    /// **A key that merely *carries* one is refused only when the token is
    /// ours.** Nothing is renamed in that case — a substring was never going to
    /// be restored — so the only thing that can go wrong is one of our tokens
    /// leaving inside somebody else's key, and that question has an exact
    /// answer: present in `by_placeholder`, mapped to something other than
    /// itself. A caller's own literal is self-mapped by `reserve_literals` and
    /// is theirs to have back, which is what the second half of the test rules
    /// out.
    ///
    /// The containment arm is the trap in the obvious fix, and it is why the
    /// tightening of `is_placeholder` could not be shipped on its own.
    /// `[[PERSON_1]]` is no longer a placeholder, so a shape-only check now
    /// serves it — with `[PERSON_1]` inside it, in a session that issued
    /// `[PERSON_1]` for a real name. That is the failure restoration exists to
    /// prevent, arrived at by fixing a false refusal. It also closes a case
    /// nothing covered before: `owner[PERSON_1]` begins with no bracket, so the
    /// old check never looked at it and it was forwarded whole.
    fn key_is_unserveable(&self, key: &str) -> bool {
        if is_placeholder(key) {
            return true;
        }
        pieces(key).any(|piece| match piece {
            Piece::Placeholder(candidate) => self
                .by_placeholder
                .get(candidate)
                .is_some_and(|value| value != candidate),
            Piece::Text(_) => false,
        })
    }

    pub fn restore(&self, text: &str) -> Result<String, MappingError> {
        let mut result = String::with_capacity(text.len());
        for piece in pieces(text) {
            match piece {
                Piece::Text(run) => result.push_str(run),
                Piece::Placeholder(candidate) => {
                    let value = self
                        .by_placeholder
                        .get(candidate)
                        .ok_or_else(|| MappingError::Unknown(candidate.to_owned()))?;
                    result.push_str(value);
                }
            }
        }
        Ok(result)
    }
}

/// Which tokens in a response this gateway may claim as its own.
///
/// A `by_placeholder` lookup is **not** provenance: a session outlives a
/// request, so a token turn one issued is in the table when turn three's caller
/// writes that same literal themselves. Restoring on the lookup would hand the
/// client turn one's value in place of its own text, and refusing on it would
/// reject a response that is served today. Both were tried; the spec records
/// them.
///
/// So provenance is built from this request and nothing else.
#[allow(dead_code)]
pub struct Provenance {
    /// Tokens `placeholder_for` returned during this request's mask pass. A
    /// caller's literal never reaches `placeholder_for` — `reserve_literals` is
    /// the only thing that sees one — so this set cannot be forged from the
    /// request body.
    issued: HashSet<String>,
    /// Placeholder-shaped tokens the request body carried, from **every**
    /// string in it. Not from `reserve_literals`: that runs only inside
    /// provider-selected slots, and dispatch strings are deliberately not
    /// slots, so a tool name `lookup_[PERSON_1]` would be invisible to it and
    /// the echoed name would come back as `lookup_Martina Weber` — a broken
    /// call the client cannot diagnose.
    written: HashSet<String>,
}

impl Provenance {
    #[allow(dead_code)]
    pub fn new(issued: HashSet<String>, written: HashSet<String>) -> Self {
        Self { issued, written }
    }

    /// Whether the sweep may restore this token. A token in both sets is
    /// ambiguous by construction: the two occurrences reach the response as the
    /// same bytes, and nothing distinguishes them. Left, which loses coverage
    /// and corrupts nothing. #32 is what separates them.
    #[allow(dead_code)]
    pub fn restorable(&self, token: &str) -> bool {
        self.issued.contains(token) && !self.written.contains(token)
    }
}

/// How deep a client's document may nest, and how many values it may hold.
/// This walk is reachable from a client's tool arguments, which are untrusted
/// input — a document nested ten thousand deep would end the process by
/// exhausting the stack, and the recursion below is the one that would do it.
///
/// `restore_value` has no bound of its own; see the note on it for why this one
/// is the protection that matters.
pub const MAX_JSON_DEPTH: usize = 64;
pub const MAX_JSON_NODES: usize = 10_000;

/// What a document *is*, which decides whether every string in it is data.
///
/// The distinction exists because "keys are never masked" is a rule about
/// position, and JSON Schema breaks the assumption that position is enough: it
/// names properties as string *values* as well as keys. `{"required":
/// ["Weber"]}` puts a property name where the walk sees data, and masking it
/// leaves the provider a schema requiring a property that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A tool's arguments, or anything else somebody wrote as data. Every
    /// string in it is a value, and all of them are scanned.
    Instance,
    /// A JSON Schema. Most of its strings are prose and are scanned; the ones
    /// listed below are identifiers, and masking one changes what the schema
    /// *means* rather than what it says.
    Schema,
    /// An object under `properties` or `$defs`, whose keys are names the
    /// caller chose and whose values are subschemas. It is its own shape
    /// because the keyword lists must not be read against those keys: `type`
    /// is an ordinary name for a parameter, and treating it as a keyword would
    /// skip that parameter's whole subschema and forward its prose unmasked.
    SchemaMap,
    /// A subschema whose *instances* are property names — the value of
    /// `propertyNames`. Prose inside it is prose like anywhere else, but the
    /// strings it constrains are names, so the keywords that hold example
    /// instances hold names rather than data and stay unscanned.
    NameSchema,
    /// An object under draft-07's `dependencies`, whose keys are property
    /// names and whose values are *either* an array of property names or a
    /// subschema. 2020-12 split those into `dependentRequired` and
    /// `dependentSchemas` precisely because one keyword meaning two things is
    /// hard to read; the older spelling is still what a client may send, and
    /// the two halves need opposite treatment.
    DependencyMap,
}

/// Keywords whose contents are identifiers rather than prose.
///
/// Closed by the JSON Schema specification rather than by our imagination, and
/// a keyword missing from it is scanned rather than skipped — which for *this*
/// list is the harmless direction: the value is masked when it need not have
/// been, the call breaks visibly, and nobody's data left.
///
/// This comment used to claim that safety on behalf of the mechanism as a
/// whole, and that was wrong. The lists are not symmetric. A keyword missing
/// from this one or from `SCHEMA_MAP_KEYWORDS` over-masks; a keyword holding an
/// *instance* that no list names would be walked as though it were schema
/// structure, and then this list would skip the client's own data inside it.
/// That direction leaks, and `SCHEMA_APPLICATOR_KEYWORDS` is what closes it —
/// see the rule in `descend_into`.
///
/// `dependentRequired` is skipped whole rather than descended into: it maps
/// property names to lists of property names, and there is no prose anywhere in
/// it. `propertyNames` is *not* on this list even though part of it is dispatch,
/// because only part of it is — see `Shape::NameSchema`, which is what a keyword
/// gets when it needs a rule rather than a verdict.
///
/// **Membership is necessary and not sufficient**, the same sentence
/// `SCHEMA_NAME_INSTANCE_KEYWORDS` carries and for the same reason: each entry
/// pairs its keyword with the shape that keyword's value has when it holds
/// identifiers, and `descend_into` skips the value only if it has that shape.
/// Consulted from the key alone, this list copied `{"required": {"owner":
/// "Martina Weber"}}` into the egress untouched (measured, at 200).
///
/// **And a shape is necessary and not sufficient either**, which is the third
/// column. The shape is the *container* — a string, an array of them, a map of
/// them — and a container is not an identifier: `{"type": "Martina Weber"}` is
/// a string where `type` takes a string, and JSON Schema publishes exactly
/// seven values it may be. So each entry also names what the strings inside it
/// have to *be*, and the fourteen do not answer that the same way — see
/// `Content`, where the three kinds and the reason the third one checks
/// nothing are written out.
///
/// **And "what grammar does this keyword have" is not the same question as "in
/// which drafts does it exist at all".** Only the first was asked when the
/// third column was added, and the second is what `$dynamicAnchor` needed: it
/// was given the *union* of 2019-09's and 2020-12's anchor patterns, on the
/// argument — correct for `$anchor` — that a client may send either draft. But
/// 2020-12 introduced `$dynamicAnchor` and no earlier draft has it, so there is
/// no second spelling it could legitimately be written in, and 2019-09's
/// pattern permits `:`. `Martina:Weber` was skipped (measured, at 200). Every
/// entry has now been asked the second question:
///
/// | Keyword | Defined by | Verdict |
/// |---|---|---|
/// | `$ref`, `format`, `pattern`, `required`, `type` | 04 through 2020-12 | one rule, published unchanged by all of them |
/// | `$schema` | 04 through 2020-12 | an *absolute* URI in all of them; checked as a URI-reference, which is looser than every draft — the recorded residual, unchanged |
/// | `$id` | 06 through 2020-12 | a URI-reference in all of them; 06 and 07 permit a fragment and 2019-09 forbids a non-empty one, so the check is the looser of the two |
/// | `id` | draft-04's spelling of `$id`, and nothing later | one draft, one rule |
/// | `contentEncoding`, `contentMediaType` | 07, 2019-09, 2020-12 | one rule in all three |
/// | `dependentRequired` | 2019-09, 2020-12 | one rule, and it is the caller's own vocabulary either way |
/// | `$anchor` | 2019-09, 2020-12 | **two drafts, two patterns — the union is right here** |
/// | `$dynamicAnchor`, `$dynamicRef` | 2020-12 only | **one draft, so one grammar.** `$dynamicAnchor` was the leak |
/// | `$recursiveRef` | 2019-09 only | one draft, one rule, and the rule is URI-reference |
///
/// **A keyword this list omits is walked as prose, and that is the other way to
/// over-mask.** `$recursiveRef` was on the ledger's deferred pile since Task 4
/// and reached this round unlisted, so `{"$recursiveRef": "Weber.json"}` went to
/// the provider as `{"$recursiveRef": "[PERSON_1].json"}` — a rewritten
/// reference target, which breaks the schema exactly as a masked `$ref` would
/// (measured, at 200). The comment beside `$dynamicRef` said the keyword's
/// "value must be `\"#\"`", and 2019-09 says no such thing: §8.2.4.2.1 is "MUST
/// be a string which is a URI-reference", and the core meta-schema publishes it
/// as `{"type": "string", "format": "uri-reference"}`. The draft adds that the
/// *behavior* is defined only for `"#"` and that implementations MAY treat
/// other values as errors — a statement about evaluation, not about syntax, and
/// a permission this gateway is not the one to exercise.
///
/// **Its other half is not on this list and cannot be: `$recursiveAnchor` MUST
/// be a boolean** (§8.2.4.2.2, and the meta-schema agrees). It holds no string
/// in any draft, so there is nothing for a detector to see and nothing for a
/// mask to break; a `$recursiveAnchor` holding a *string* states no anchor and
/// is the caller's data, which is what an unlisted keyword already gets. The
/// second name the ledger deferred, **OpenAPI's `discriminator`**, is not on
/// this list either and the reason is different — see `Content`.
///
/// Two entries are looser than the drafts on purpose and both are recorded
/// where the looseness is: draft-03 spelled `required` as a boolean on the
/// property's own schema, which `Identifier::Names` rejects as no name list —
/// a bare `true` carries no text, so nothing leaks and nothing over-masks; and
/// draft-03 published an eighth type name, `any`, which 04 dropped. A draft-03
/// `{"type": "any"}` is now scanned, and `any` is not a person, so the cost is
/// a detector call. Widening a genuinely closed vocabulary for a draft nobody
/// sends is the trade this list exists to refuse.
const SCHEMA_IDENTIFIER_KEYWORDS: [(&str, Identifier, Content); 15] = [
    // Two drafts define it and they publish different patterns, so the check
    // is the union of the two — see `Content::Anchor`. `$dynamicAnchor` below
    // is the same shape of keyword and *not* the same answer, for a reason
    // that is about the drafts rather than about the grammar.
    ("$anchor", Identifier::Name, Content::Anchor),
    // **One draft defines it, so there is one grammar it can be written in.**
    // 2020-12 introduced `$dynamicAnchor` and no earlier draft has it; a union
    // with 2019-09's pattern gives it a spelling no draft ever published, and
    // 2019-09's permits `:`, so `Martina:Weber` was skipped (measured, at 200).
    ("$dynamicAnchor", Identifier::Name, Content::DynamicAnchor),
    // 2020-12 too, and 2019-09's nearest analogue is a different keyword —
    // `$recursiveRef`, below. One draft, one grammar, and 2020-12 says
    // URI-reference.
    ("$dynamicRef", Identifier::Name, Content::UriReference),
    ("$id", Identifier::Name, Content::UriReference),
    // 2019-09's own dynamic reference — the keyword 2020-12 retired in favour
    // of `$dynamicRef` above. The draft that defines it says URI-reference and
    // says so twice, in the prose and in the meta-schema; the sentence people
    // remember instead ("only the value `#` has a clear use case") is about
    // what an implementation may *do* with the value, not about which values
    // are legal to write.
    ("$recursiveRef", Identifier::Name, Content::UriReference),
    ("$ref", Identifier::Name, Content::UriReference),
    ("$schema", Identifier::Name, Content::UriReference),
    ("contentEncoding", Identifier::Name, Content::Token),
    ("contentMediaType", Identifier::Name, Content::MediaType),
    // Property names, both of them — the caller's own vocabulary, and the one
    // kind of identifier there is nothing to check. See `Content::Arbitrary`.
    (
        "dependentRequired",
        Identifier::NamesPerName,
        Content::Arbitrary,
    ),
    // **No draft publishes a grammar for it.** "The value of this keyword is
    // called a format attribute. It MUST be a string" is the whole of the
    // constraint, and §7.2.3 adds that implementations MAY support custom
    // format attributes. A one-token check was inferred from what the
    // registered names happen to look like, and `{"format": "Weber Martina"}`
    // went to the provider as `{"format": "[PERSON_1] Martina"}` (measured, at
    // 200). See `Content::Arbitrary` for the residual that buys.
    ("format", Identifier::Name, Content::Arbitrary),
    // draft-04 spelled `$id` this way. A schema written to that draft is
    // still a schema a client may send, and a masked base URI breaks every
    // `$ref` that resolves against it.
    ("id", Identifier::Name, Content::UriReference),
    // An ECMA-262 regular expression. Round 8 put this on `Content::Arbitrary`
    // on the argument that every string is a valid one; `Martina Weber(` is an
    // unterminated group and ECMA-262 rejects it, so the argument was false and
    // the value was skipped (measured, at 200). What is checked is structure
    // only — see `Content::RegularExpression` for why a parser is the wrong
    // instrument here and what that leaves open.
    ("pattern", Identifier::Name, Content::RegularExpression),
    ("required", Identifier::Names, Content::Arbitrary),
    ("type", Identifier::NameOrNames, Content::TypeName),
];

fn identifier_keyword(key: &str) -> Option<(Identifier, Content)> {
    SCHEMA_IDENTIFIER_KEYWORDS
        .iter()
        .find(|(keyword, _, _)| *keyword == key)
        .map(|(_, shape, content)| (*shape, *content))
}

/// The seven values JSON Schema permits for `type`, and there is no eighth.
///
/// Closed by the specification in a way almost nothing else on this branch is:
/// every draft from 04 to 2020-12 publishes the same list, and a document
/// stating anything else states no type at all.
const JSON_SCHEMA_TYPE_NAMES: [&str; 7] = [
    "array", "boolean", "integer", "null", "number", "object", "string",
];

/// The shape an identifier keyword's value takes when it really does hold
/// identifiers.
///
/// **One rule does not cover all fourteen, and the rule loose enough to try
/// leaks.** A union admitting every keyword's shape at once — a string, an
/// array of strings, or an object of arrays of strings — would skip `{"$ref":
/// {"note": ["Martina Weber"]}}`, where the strings are the caller's prose and
/// no `$ref` was ever stated. So each keyword is held to the shape its own
/// draft defines. Being wrong then costs an over-masked schema the caller can
/// see, which is the direction this walk chooses everywhere; the other way
/// round costs a value nothing looked at.
///
/// What this does *not* narrow, and deliberately: a property genuinely named
/// `Martina Weber` is still skipped under `required`, because it is a name and
/// names are the caller's dispatch. That residual is the one the walk has
/// always had — keys are never masked — and it is unchanged.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Identifier {
    /// One identifier: a string, and nothing else. `$ref`, `$id`, `format`,
    /// `pattern` and the rest of the URI-and-token keywords.
    Name,
    /// A string *or* an array of strings — `type`'s published union, and the
    /// shape a name-stating instance keyword takes under `propertyNames`.
    NameOrNames,
    /// An array of property names: `required`.
    Names,
    /// An object mapping a property name to an array of property names:
    /// `dependentRequired`, and draft-07's `dependencies` where its value is
    /// the array half.
    NamesPerName,
}

impl Identifier {
    /// Whether `value` is the shape this keyword takes when it holds
    /// identifiers. Anything else is malformed against the keyword's own draft,
    /// so it states no identifier at all and is the client's data like every
    /// other unrecognized value here.
    fn holds(self, value: &Value) -> bool {
        match self {
            Identifier::Name => value.is_string(),
            Identifier::NameOrNames => value.is_string() || is_name_list(value),
            Identifier::Names => is_name_list(value),
            Identifier::NamesPerName => value
                .as_object()
                .is_some_and(|fields| fields.values().all(is_name_list)),
        }
    }
}

/// What an identifier keyword's strings have to *be* for the keyword to have
/// stated an identifier.
///
/// `Identifier` is the container and this is what is in it, and the round that
/// added the container stopped one level short. Its own sentence was the right
/// one — "does this document *state* the thing the rule exempts" — and it asked
/// it of the container only: `{"type": "Martina Weber"}` is a string where
/// `type` takes a string, so the container check passes and the name went to
/// the provider unread (measured, at 200). JSON Schema permits seven values for
/// `type`. `Martina Weber` is not one, so that document states no type, and the
/// bytes are the caller's data like any other unrecognized value here.
///
/// **The fifteen keywords are three kinds, not fifteen of one**, and the third
/// kind is why this enum has a variant that checks nothing:
///
/// - **a closed vocabulary** — `type`, and only `type`. The seven names are
///   published by every draft and extended by none.
/// - **a grammar** — `$ref`, `$id`, `id`, `$schema`, `$dynamicRef` and
///   `$recursiveRef` are URI-references; `$anchor` and `$dynamicAnchor` have a
///   published pattern, and *not the same one*, because they are not published
///   by the same set of drafts; `contentMediaType` is a media type down to its
///   parameters; `contentEncoding` is a bare token, and the drafts name where
///   its values come from; `pattern` is a regular expression, checked for
///   structure only.
/// - **a string the caller chose, with no grammar anywhere to hold it to** —
///   `required` and `dependentRequired` hold *property names*, and `format`
///   holds a format attribute the drafts decline to constrain past "MUST be a
///   string". Nothing about the characters is knowable, and a check invented
///   here would mask a schema that was correct. `Content::Arbitrary` is that
///   answer written down rather than left as an omission. It is **three**
///   keywords; `pattern` was a fourth until the argument putting it there —
///   "every string is a valid regex" — turned out to be false, and `format`
///   left for one round and came back.
///
/// **Nine rounds found this walk too loose and the tenth found it too strict,
/// twice, both times in a check written in the two rounds before it.** The
/// question that catches that is the mirror of round 8's: not "does the
/// document state the thing the rule exempts", but *does the draft actually say
/// this, or was it inferred from what the well-formed values happen to look
/// like*. `format` failed it — every registered format name is one token, and
/// no draft ever said it had to be. Asking it of a grammar is cheap and asking
/// it late is not: a check stricter than the draft masks a valid schema and
/// breaks a caller who was working, and unlike a leak they see it immediately.
///
/// **What it costs to be too strict is exposure, not a mask, and the difference
/// decides which of these is worth closing.** An over-strict grammar hands the
/// value to the detector; the value is only rewritten if it also *fires* one.
/// So `contentMediaType`'s refusal of the trailing space in `"text/plain "` is
/// a real strictness — RFC 822 folds whitespace around a structured field's
/// tokens, and this check's own parameter half trims for exactly that reason —
/// and it was measured costing nothing: `{"contentMediaType": "text/plain "}`
/// is handed to the detector, fires nothing, and reaches the provider verbatim.
/// It is only a mask when the *essence* is also a person, which is
/// `"text/Weber "` and not a document anyone writes. Recorded here and left,
/// with the trade named: trimming each half separately would admit `"Weber /
/// Martina"` as the media type `Weber/Martina`, and buying a marginal over-mask
/// with a new skip is the over-correction this paragraph exists to stop.
///
/// **And the keyword that belongs on no list at all: OpenAPI's
/// `discriminator`.** It has been beside `$recursiveRef` on the ledger since
/// Task 4 and it is not the same kind of item, which is why one round can close
/// both. It is not a JSON Schema keyword, and its value is an *object*:
/// `propertyName`, a property name; `mapping`, from discriminator values the
/// client chose to `$ref` strings; and — OpenAPI says so explicitly — any
/// number of `x-` extensions. Three kinds in one value, so no single
/// `(Identifier, Content)` pair describes it, and a list entry is a verdict on
/// the whole subtree. Entered here it would skip an `x-note` holding a
/// sentence, which is a leak; left off, as it is, `mapping`'s right-hand
/// `$ref`s are walked as prose and a detection rewrites one, which is
/// `$recursiveRef`'s bug at one remove.
///
/// So: **nowhere on this list, and the ledger's entry is closed as that** — but
/// closed with its residual named rather than as a tidiness item that turned
/// out to be nothing. What `discriminator` needs is what `propertyNames` got: a
/// `Shape`, which is a rule applied per field, not a verdict on a subtree. That
/// is a keyword's worth of work, it is OpenAPI's vocabulary rather than JSON
/// Schema's, and the cost of not doing it is an over-masked `$ref` inside one
/// OpenAPI-only object — visible at the caller, in the direction this walk
/// chooses when it has to be wrong.
///
/// **Every one of these carries a residual, and this comment says what each one
/// is rather than reading as though the checks are complete.** Two rounds
/// running, a comment claiming completeness is what let the next finding
/// through: round 7's said the container was the check, round 8's said the
/// contents were. The three kinds fail differently and it is worth naming how.
///
/// - The **vocabulary** has no residual and is the only kind that does not.
///   Seven names, published by every draft, extended by none.
/// - A **grammar** is checked to wherever its own draft stops being
///   unambiguous, and never past it, because a check stricter than the draft
///   masks a valid schema and breaks a working caller — visibly, at the call.
///   So a URI-reference is read for whitespace and control characters and not
///   for RFC 3986's other exclusions; `$schema` is not required to be
///   absolute; `$recursiveRef` is not held to `"#"`, which is a rule about
///   evaluation rather than syntax; `contentEncoding` is one token because the
///   drafts name where its values come from and every one of them, extension
///   points included, is a token; a `contentMediaType` parameter whose value is
///   a *quoted string* is arbitrary text by RFC 2045's own definition and is
///   accepted whole; and `pattern` is checked for structure and not parsed.
///   Each of those is a string that states no identifier and is forwarded
///   anyway, and each is named at its variant with the reason.
/// - The **third kind** is a residual end to end and always was, and it is now
///   **three** strings wide rather than two. `{"required": ["Martina Weber"]}`
///   reaches the provider verbatim, because a property name is dispatch and
///   masking it breaks the schema it names; `{"dependentRequired": {"card":
///   ["Martina Weber"]}}` likewise; and, added this round, `{"format":
///   "Martina Weber"}` **forwards verbatim too**, because no draft says a
///   format attribute is anything but a string. That is a leak with a name and
///   it is the same one the other two carry. It is bought deliberately: the
///   alternative was a token rule no specification contains, which this round
///   measured breaking `{"format": "Weber Martina"}` into `{"format":
///   "[PERSON_1] Martina"}`.
///
/// A mismatch is scanned as an instance, as every neighbouring arm does:
/// `descend_into` has no refusal channel, and an over-masked *malformed* schema
/// is a cost the caller can see. That is what makes the grammars safe to be
/// strict about and would not make a *vocabulary* safe to invent for the third
/// kind — over-masking a real property name breaks a schema that was correct.
///
/// **The other two exemptions were asked the same question and the answer is
/// no.** `descend_into` returns `None` in exactly three places, and the other
/// two are `Shape::DependencyMap`'s array half and the name-instance arm under
/// `propertyNames`. Both exempt *property names* — the third kind — so their
/// container check is the whole check available, and inventing a grammar for
/// either would mask the schema's own property names. `SCHEMA_ANNOTATION_KEYWORDS`
/// and every remaining arm grant `Shape::Instance`, which skips nothing, so
/// there is no content for a check to be about. One level out in `provider.rs`,
/// `Admits::OneOf` already reads the value against the provider's published
/// set, and `Admits::Dispatch` is the third kind again with the argument
/// written at the variant: MCP tool names carry `.` and `/`, so the permissive
/// grammar is load-bearing, and a name is forwarded because it is dispatch.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Content {
    /// One of `JSON_SCHEMA_TYPE_NAMES`.
    TypeName,
    /// RFC 3986's URI-reference, checked at the one place there is no
    /// judgement: a URI-reference contains no whitespace and no control
    /// character, ever, in any draft.
    ///
    /// **And checked nowhere else, on purpose.** RFC 3986 also excludes
    /// `<>"{}|\^` and the backtick, and real OpenAPI documents put a path
    /// template in a `$ref` — `#/paths/~1pets~1{petId}/get` — which is
    /// technically un-encoded and is what a widely used generator emits.
    /// Masking one would break a working client to enforce a rule the provider
    /// does not enforce either, which is the mistake this branch has already
    /// made once this week.
    UriReference,
    /// `$anchor`, and only `$anchor`: 2019-09's `^[A-Za-z][-A-Za-z0-9.:_]*$`
    /// and 2020-12's `^[A-Za-z_][-A-Za-z0-9._]*$`, as their **union**. Both
    /// drafts define the keyword and a client may send either, so a string
    /// satisfying either pattern is an anchor and refusing one would break a
    /// schema written to the older draft.
    Anchor,
    /// `$dynamicAnchor`: 2020-12's `^[A-Za-z_][-A-Za-z0-9._]*$` **alone**.
    ///
    /// The same shape of keyword as `$anchor` and deliberately not the same
    /// check, because the union's argument is about *drafts* and not about
    /// grammars. 2020-12 introduced `$dynamicAnchor`; no earlier draft has it,
    /// so a value written in 2019-09's spelling is not a schema anyone could
    /// have written to a draft that defines the keyword. 2019-09's pattern
    /// permits `:` and 2020-12's does not, which is the whole of the
    /// difference and is exactly what `Martina:Weber` walked through.
    DynamicAnchor,
    /// A media type: `type/subtype`, and after each `;` an RFC 2045
    /// `attribute=value` where the attribute is a token and the value is a
    /// token or a quoted string.
    ///
    /// **The parameters are read, and the round that wrote this check said in
    /// as many words that they were not.** `text/plain; Martina Weber` passed
    /// on the strength of its essence and forwarded the rest unscanned
    /// (measured, at 200). `Martina Weber` is neither a token nor a
    /// quoted-string value and states no parameter, so the value states no
    /// media type and is scanned whole — the essence masked with it, which is
    /// the all-or-nothing ruling `Content::stated_by` already makes for
    /// `type`'s union and for the same reason.
    ///
    /// **Residual: a well-formed quoted string is arbitrary text and is
    /// accepted.** `text/plain; name="Martina Weber"` states a media type by
    /// RFC 2045 — a quoted string exists precisely so a value may hold spaces
    /// and tspecials — and it is forwarded. There is no channel here to hand
    /// back one substring of a leaf, so closing it would mean scanning the
    /// whole value, essence included, for every `boundary="..."` a real client
    /// sends. Recorded rather than closed, and it is a decision a later round
    /// can overturn as a decision.
    MediaType,
    /// A bare token: `contentEncoding`, and **only** `contentEncoding` — it
    /// held `format` too until the drafts were read for what they say rather
    /// than for what their examples look like.
    ///
    /// **The two keywords were given one rule by analogy and the analogy is
    /// false.** `format` has no cited origin at all: "MUST be a string" is the
    /// whole constraint, and 2020-12 §7.2.3 invites implementations to define
    /// custom attributes without saying what one may be spelled like. That is
    /// `Content::Arbitrary`, and it is where `format` now is.
    /// `contentEncoding`'s text is different in every draft that has it, and
    /// all three name a *source* for the values:
    ///
    /// - draft-07 §8.3 — "RFC 2045, Sec 6.1 lists the possible values for this
    ///   property";
    /// - 2019-09 §8.3 — "Possible values for this property are listed in RFC
    ///   2045, Sec 6.1 and RFC 4648";
    /// - 2020-12 §8.3 — RFC 4648's names, RFC 2045 §§6.7 and 6.8, and "this
    ///   keyword is derived from MIME's Content-Transfer-Encoding header".
    ///
    /// **And the cited source closes the extensibility argument the other
    /// way.** RFC 2045 §6.1's `mechanism` is open — `ietf-token` and `x-token`
    /// are there precisely so a new encoding can be named — but both are RFC
    /// 2045 `token`s, so the extension point *has a published grammar*, which
    /// is exactly what `format`'s does not. `x-Weber-encoding` is a token and
    /// is forwarded (measured). A value with a space in it names no encoding in
    /// either RFC, in either RFC's extension point, or in a header field whose
    /// value grammar cannot hold one.
    ///
    /// Two keywords, two texts, two answers. That is a result, not a
    /// discrepancy, and reading them as one rule is what round 8 did.
    ///
    /// The check itself is looser than RFC 2045's `token` — non-empty, no
    /// whitespace, no control character, and nothing about tspecials or ASCII —
    /// for the standing reason: the draft cites the RFC to say where the names
    /// come from, not to make the RFC's lexer normative here.
    Token,
    /// An ECMA-262 regular expression, checked for **structure only**:
    /// balanced `(`/`)`, a closed `[`…`]`, and no trailing lone backslash.
    ///
    /// **The instrument was the whole of the work here, and the obvious one is
    /// wrong.** A check too strict masks a *valid* pattern, and a masked
    /// `pattern` breaks a schema that was correct, visibly, at the caller. So
    /// before validating, what the validator accepts has to be established:
    ///
    /// - `regex-syntax`, the parser under the `regex` crate, **rejects legal
    ///   ECMA-262**. Measured, not assumed: it refuses `(?=Martina)Weber` and
    ///   `(?<=Weber)Martina` ("look-around … is not supported"), `(Martina) \1`
    ///   ("backreferences are not supported"), `a{` and `[]` — five kinds of
    ///   pattern that a browser compiles and that JSON Schema permits, because
    ///   the drafts define `pattern` as ECMA-262 and not as Rust's dialect.
    ///   Validating with it would mask all five.
    /// - It is also not a dependency of this crate. `regex` is in the lock file
    ///   only as a transitive dev-dependency of `wiremock`, so using it means
    ///   adding a runtime dependency — which would need its own argument even
    ///   if the dialect matched, and the dialect does not.
    ///
    /// So the check is the part that is invalid in **every** dialect there is,
    /// ECMA-262 included, and nothing beyond it. Where the dialects disagree
    /// about `[`…`]` — ECMA-262 closes the class at the first `]`, so `[]` is
    /// the empty class, while POSIX and Rust read that `]` as a literal member
    /// — the reading taken is ECMA-262's, which is both the draft's own and the
    /// more permissive of the two about termination.
    ///
    /// **Residual, and it is real: a pattern invalid for a subtler reason still
    /// forwards its contents.** `*Martina Weber` (a quantifier with nothing to
    /// repeat) and `[z-a]Martina Weber` (a reversed class range) are rejected
    /// by ECMA-262 and accepted here. Closing those needs a parser that accepts
    /// ECMA-262 and rejects nothing else, and no such parser is in this tree.
    /// The check is deliberately incomplete, and this sentence is the part of
    /// it that must not be dropped.
    ///
    /// And separately from the residual, unchanged and by design: a pattern
    /// that is perfectly valid and spells a person out — `^Martina Weber$` —
    /// is a regular expression the caller wrote and is forwarded. That is not
    /// a gap in this check; it is the same position `required` holds.
    RegularExpression,
    /// A string the caller chose, and there is nothing to check. The third
    /// kind, and the one that must stay this way. **Three keywords**:
    /// `required` and `dependentRequired`, both holding property names, and
    /// `format`, holding an attribute name the drafts decline to constrain.
    /// `pattern` was a fourth until its argument — "every string is a valid
    /// regex" — was measured and failed.
    ///
    /// **`format` arrived here by the same argument that sent `pattern` away,
    /// run honestly in both directions.** Round 8 called it a grammar because
    /// every registered format name is one word; that is a fact about the
    /// registry's contents, not a rule the registry imposes, and JSON Schema
    /// says only "It MUST be a string" (2020-12 §7.1) while §7.2.3 lets an
    /// implementation recognise whatever names it likes. By this walk's own
    /// criterion — does the document *state* the thing the rule exempts — any
    /// string states a format attribute, because a format attribute may be any
    /// string. `{"format": "Weber Martina"}` reached the provider as
    /// `{"format": "[PERSON_1] Martina"}` (measured, at 200), which is a valid
    /// schema broken to enforce a rule no specification contains.
    ///
    /// **What that costs, stated rather than implied: `{"format": "Martina
    /// Weber"}` now forwards verbatim.** A person's name written where a
    /// format attribute goes reaches the provider unread. That is the identical
    /// residual `required` and `pattern` already carry — a string the caller
    /// controls, in a position the draft leaves open — and it is the price of
    /// not masking the format attribute a client actually defined. Closing it
    /// would need a grammar, and inventing one here is the mistake this variant
    /// exists to record.
    Arbitrary,
}

impl Content {
    /// Whether every identifier in `value` is one.
    ///
    /// Recursive, and it may be: the container check has already run, so the
    /// strings in `value` are exactly the identifiers the keyword states — one
    /// of them for `Name`, an array of them for `Names`, an object of arrays
    /// for `NamesPerName`. Anything else cannot be here, and if it somehow is,
    /// `false` scans it.
    ///
    /// **All of them, not any.** `{"type": ["object", "Martina Weber"]}` states
    /// one type name and one thing that is not, and the array is scanned whole
    /// — `object` included. Masking a real type name inside a malformed union
    /// breaks a schema the caller can see, which is the cheaper of the two
    /// mistakes and is the ruling `a_mixed_array_under_a_name_keyword_is_scanned_whole`
    /// already made for the same shape.
    ///
    /// **The object arm cannot decide anything today, and is kept anyway.**
    /// `NamesPerName` is `dependentRequired`'s container and nothing else's,
    /// and `dependentRequired` holds property names, so the arm always reaches
    /// `Content::Arbitrary` and always says yes. Emptying it to `true` leaves
    /// the suite green (measured). It stays because it is the arm that stays
    /// *correct* if a keyword pairing that container with a grammar is ever
    /// added — the recursion is the general answer, and a per-container answer
    /// is what this walk has had to correct twice. `Content` is asked about it
    /// directly in `a_grammar_is_checked_where_its_own_draft_puts_the_boundary`
    /// so that the arm is covered rather than merely unreachable.
    fn stated_by(self, value: &Value) -> bool {
        match value {
            Value::String(text) => self.states(text),
            Value::Array(items) => items.iter().all(|item| self.stated_by(item)),
            Value::Object(fields) => fields.values().all(|field| self.stated_by(field)),
            _ => false,
        }
    }

    fn states(self, text: &str) -> bool {
        /// Non-empty, and no whitespace or control character. The one thing a
        /// token cannot contain whatever registry it comes from.
        fn is_token(text: &str) -> bool {
            !text.is_empty() && !text.chars().any(|c| c.is_whitespace() || c.is_control())
        }
        match self {
            Content::TypeName => JSON_SCHEMA_TYPE_NAMES.contains(&text),
            // The empty string is a legal URI-reference — it points at the
            // current document, which is how a recursive `$ref` is spelled —
            // and it carries nothing either way.
            Content::UriReference => !text.chars().any(|c| c.is_whitespace() || c.is_control()),
            Content::Anchor => is_anchor_2019_09(text) || is_anchor_2020_12(text),
            Content::DynamicAnchor => is_anchor_2020_12(text),
            Content::MediaType => {
                let (essence, parameters) = match text.split_once(';') {
                    Some((essence, parameters)) => (essence, Some(parameters)),
                    None => (text, None),
                };
                essence
                    .split_once('/')
                    .is_some_and(|(kind, subtype)| is_media_token(kind) && is_media_token(subtype))
                    && parameters.is_none_or(states_media_parameters)
            }
            Content::Token => is_token(text),
            Content::RegularExpression => states_regular_expression(text),
            Content::Arbitrary => true,
        }
    }
}

/// 2019-09's anchor pattern, `^[A-Za-z][-A-Za-z0-9.:_]*$`. The `:` is the
/// character 2020-12 dropped, and `$anchor` is the only keyword that may still
/// be written this way.
fn is_anchor_2019_09(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':' | '_'))
}

/// 2020-12's `anchorString`, `^[A-Za-z_][-A-Za-z0-9._]*$`. Both `$anchor` and
/// `$dynamicAnchor` reference it in that draft's meta-schema; `$dynamicAnchor`
/// exists in no other draft, so for that keyword this is the whole rule.
fn is_anchor_2020_12(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

/// RFC 2045's `token`: non-empty US-ASCII with no space, no control character
/// and none of the tspecials.
///
/// Stricter than the `is_token` inside `states`, and about a different thing:
/// that one is a *format attribute* name out of an open registry, where the
/// only rule anyone can name is "one word". This one has a grammar published
/// with the character set spelled out, so it is checked against the character
/// set.
fn is_media_token(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|c| {
            c.is_ascii()
                && !c.is_ascii_whitespace()
                && !c.is_control()
                && !matches!(
                    c,
                    '(' | ')'
                        | '<'
                        | '>'
                        | '@'
                        | ','
                        | ';'
                        | ':'
                        | '\\'
                        | '"'
                        | '/'
                        | '['
                        | ']'
                        | '?'
                        | '='
                )
        })
}

/// Whether everything after a media type's first `;` is RFC 2045's parameter
/// section: `attribute=value` pairs separated by `;`, the attribute a token and
/// the value a token or a quoted string.
///
/// Surrounding whitespace is trimmed at each boundary, because RFC 822's
/// folding rules put it there and every real client writes `text/plain;
/// charset=utf-8` with the space after the `;`.
fn states_media_parameters(parameters: &str) -> bool {
    let mut rest = parameters;
    loop {
        // The attribute runs to the `=`. A parameter with no `=` in it at all
        // is the leak this function exists for: `text/plain; Martina Weber`.
        let Some((attribute, after)) = rest.split_once('=') else {
            return false;
        };
        if !is_media_token(attribute.trim()) {
            return false;
        }
        let after = after.trim_start();
        rest = match after.strip_prefix('"') {
            // A quoted string ends at the first `"` that is not escaped; `\`
            // escapes the character after it, which is how a quote gets in.
            Some(quoted) => {
                let mut characters = quoted.char_indices();
                loop {
                    match characters.next() {
                        Some((_, '\\')) => {
                            if characters.next().is_none() {
                                return false;
                            }
                        }
                        Some((at, '"')) => break &quoted[at + 1..],
                        Some(_) => {}
                        None => return false,
                    }
                }
            }
            // A token value ends where the next parameter begins, or at the
            // end of the string.
            None => {
                let end = after.find(';').unwrap_or(after.len());
                if !is_media_token(after[..end].trim()) {
                    return false;
                }
                &after[end..]
            }
        };
        // What follows a value is either the `;` that starts the next
        // parameter or the end of the media type. Anything else — trailing
        // text after a closing quote, say — states no parameter.
        let remainder = rest.trim_start();
        match remainder.strip_prefix(';') {
            Some(next) => rest = next,
            None => return remainder.is_empty(),
        }
    }
}

/// Whether `text` is *structurally* a regular expression.
///
/// Three rules, and they are the ones every regex dialect agrees on — see
/// `Content::RegularExpression` for why the check stops here and what it
/// therefore lets through. A group has to close, a character class has to
/// close, and a backslash has to have something to escape.
fn states_regular_expression(text: &str) -> bool {
    let mut characters = text.chars();
    let mut depth = 0usize;
    let mut in_class = false;
    while let Some(character) = characters.next() {
        match character {
            // A backslash escapes the next character everywhere — inside a
            // class and outside one — and a trailing lone backslash has
            // nothing to escape, which no dialect accepts.
            '\\' => {
                if characters.next().is_none() {
                    return false;
                }
            }
            // Inside a class, `(`, `)` and `[` are literal and `]` closes.
            // ECMA-262 closes at the *first* `]`, which makes `[]` the empty
            // class where POSIX and Rust read an unterminated one — the
            // draft's own reading, and the one that cannot mask a pattern the
            // draft calls valid.
            _ if in_class => in_class = character != ']',
            '[' => in_class = true,
            '(' => depth += 1,
            ')' => {
                let Some(outer) = depth.checked_sub(1) else {
                    return false;
                };
                depth = outer;
            }
            _ => {}
        }
    }
    depth == 0 && !in_class
}

/// A list of property names: an array, every element of which is a string.
///
/// One spelling, read by every arm that asks. The question was answered three
/// separate times in `descend_into` — once per arm, each by whoever was looking
/// at the arm that had just leaked — and **two of the three answers were
/// wrong**. `dependencies` accepted any array whatever, so `{"dependencies":
/// {"a": [{"owner": "Martina Weber"}]}}` skipped an object subtree (measured,
/// at 200); `SCHEMA_IDENTIFIER_KEYWORDS` asked nothing about the value at all.
/// A shared predicate cannot be right in one arm and wrong in the next.
fn is_name_list(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.iter().all(Value::is_string))
}

/// Keywords whose values are subschemas — the ones under which schema keywords
/// go on meaning what they mean.
///
/// This list is what lets the walk treat *everything it does not recognize* as
/// the client's data, which is the rule that closes the leak. The inverse
/// arrangement — structure by default, data only where named — cannot work:
/// `enum` and `default` are enumerable but `example` (OpenAPI 3.0's singular
/// spelling), `x-anything`, and whatever a vendor invents tomorrow are not, and
/// each of them walked as structure hands the identifier list a chance to skip
/// real data.
///
/// The list has to be right in the other direction too, and it can be: which
/// keywords hold subschemas is closed by the specification. Miss one and every
/// `required` below it is masked — a broken schema rather than a leak, which is
/// the way round to be wrong.
///
/// **Membership is necessary and not sufficient** — the third list to carry
/// that sentence, and the one that proves why the *other* half of round 3's
/// audit criterion was needed. This arm can only return `Some`, so the audit
/// read it as unable to leak; but *which shape* it returns decides which skip
/// rules apply below it, and `Shape::Schema` is not a promise to scan, it is a
/// promise to apply schema rules. Consulted from the key alone, `{"allOf":
/// {"type": "Martina Weber"}}` — an object where the draft says array — was
/// granted schema semantics anyway, and `type`'s value was skipped one level
/// down as a type name (measured, at 200). So each keyword is paired with the
/// container its own draft defines, and a value that is not that container
/// states no schema structure and is the client's data.
const SCHEMA_APPLICATOR_KEYWORDS: [(&str, Container); 15] = [
    ("additionalItems", Container::Schema),
    ("additionalProperties", Container::Schema),
    ("allOf", Container::SchemaArray),
    ("anyOf", Container::SchemaArray),
    ("contains", Container::Schema),
    ("contentSchema", Container::Schema),
    ("else", Container::Schema),
    ("if", Container::Schema),
    // The one keyword on this list that publishes both containers, and it is
    // not a looseness we chose: draft-04 through draft-07 give `items` a tuple
    // form — an array of schemas, one per position — and 2020-12 moved that
    // spelling to `prefixItems` and made `items` a single schema. A client may
    // send either draft, so both are the keyword's own shape. Handled per value
    // the way `dependencies` is, and for the same reason.
    ("items", Container::SchemaOrSchemaArray),
    ("not", Container::Schema),
    ("oneOf", Container::SchemaArray),
    ("prefixItems", Container::SchemaArray),
    ("then", Container::Schema),
    ("unevaluatedItems", Container::Schema),
    ("unevaluatedProperties", Container::Schema),
];

fn applicator_keyword(key: &str) -> Option<Container> {
    SCHEMA_APPLICATOR_KEYWORDS
        .iter()
        .find(|(keyword, _)| *keyword == key)
        .map(|(_, container)| *container)
}

/// The container a schema keyword's value takes when the keyword really does
/// govern schema structure.
///
/// The sibling of `Identifier`, one layer out. `Identifier` asks whether a
/// value *states* an identifier; this asks whether a value is the thing the
/// keyword's rules are *about*. Both exist because a keyword's meaning is a
/// pair — a name and a shape — and reading only the name grants a rule to
/// whatever the caller happened to write.
///
/// A mismatch is scanned as an instance rather than refused, which is the
/// answer `dependencies`, the identifier arms and the name-instance arm all
/// already give: `descend_into` returns `Option<Shape>` and has no refusal
/// channel, and an over-masked malformed schema is a cost the caller can see.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Container {
    /// One schema. An object — or, draft-06 onward, a boolean, which is a
    /// schema with no children and so nothing to skip.
    Schema,
    /// An array of schemas: `allOf`, `anyOf`, `oneOf`, `prefixItems`.
    SchemaArray,
    /// Either, both published for the same keyword by different drafts.
    SchemaOrSchemaArray,
    /// An object whose *keys* the keyword's own rule reads as names the caller
    /// chose: `properties`, `$defs`, `patternProperties`, `dependentSchemas`
    /// and draft-07's `dependencies`. Never a boolean — these are maps, not
    /// schemas.
    Map,
}

impl Container {
    /// Whether `value` is the container this keyword governs. Anything else is
    /// malformed against the keyword's own draft, so the keyword states no
    /// schema structure there and the value is the client's data.
    fn holds(self, value: &Value) -> bool {
        /// A schema is an object, or a boolean from draft-06 on. A boolean
        /// carries no leaf and no key, so admitting it changes nothing the walk
        /// can see; it is here because refusing it would call a valid schema
        /// malformed.
        fn is_schema(value: &Value) -> bool {
            value.is_object() || value.is_boolean()
        }
        match self {
            Container::Schema => is_schema(value),
            Container::SchemaArray => value
                .as_array()
                .is_some_and(|items| items.iter().all(is_schema)),
            Container::SchemaOrSchemaArray => {
                Container::Schema.holds(value) || Container::SchemaArray.holds(value)
            }
            Container::Map => value.is_object(),
        }
    }
}

/// Keywords holding prose about a schema rather than a value it constrains.
///
/// Named separately because they are the one thing scanned in *both* schema
/// shapes: under `propertyNames` an `enum` lists property names and is skipped,
/// but a `description` there is still a sentence somebody wrote.
const SCHEMA_ANNOTATION_KEYWORDS: [&str; 3] = ["$comment", "description", "title"];

/// Keywords whose object *keys* are names the caller chose, and whose values
/// are subschemas.
///
/// The third list, and the list above is unsound without it in the mirror of
/// the way it is unsound without the instance one. `{"properties": {"type":
/// {"description": "Martina Weber"}}}` describes a parameter called `type` —
/// an ordinary name for a parameter — and reading that key as a keyword skips
/// the subschema whole, forwarding its prose to the provider unmasked. The
/// keys themselves are keys and were never at risk; it is what hangs below
/// them that is.
const SCHEMA_MAP_KEYWORDS: [&str; 5] = [
    "$defs",
    "definitions",
    "dependentSchemas",
    "patternProperties",
    "properties",
];

/// Under `propertyNames`, the keywords whose instances are property *names*.
///
/// The fourth list, and the only one written the way `SCHEMA_INSTANCE_KEYWORDS`
/// was before it was deleted — small, closed, and consulted before a default.
/// The difference is which way the default falls, which is the whole of the
/// lesson that deletion taught: a list of keywords holding *data* cannot be
/// closed, because every `x-` a vendor invents extends it, so the default there
/// has to be "scan". A list of keywords holding *names* can be, because it is
/// the same enumerable set the drafts define — `enum`, `const`, `default`,
/// `examples` — and a keyword written after this line is far likelier to be a
/// vendor's prose than a fifth way of stating a name.
///
/// Wrong in the other direction the schema pays for and the caller sees: a name
/// stated under a keyword not on this list is masked, which breaks the schema
/// rather than leaking anything. That is the way round this walk chooses
/// everywhere else, and this arm is the one place it did not.
///
/// Membership is necessary and not sufficient. A property name is a string, so
/// `descend_into` reads the value as well: only a string, or an array of
/// strings, is names. Anything else under one of these is data.
const SCHEMA_NAME_INSTANCE_KEYWORDS: [&str; 4] = ["const", "default", "enum", "examples"];

/// What the walk does with one field of an object, given the shape it is in.
/// Shared by both walks so they cannot drift: they correspond by position, and
/// a field one of them skipped and the other did not would silently put a
/// masked string somewhere it does not belong.
///
/// The `value` is here for the keywords whose meaning depends on it. Most rules
/// read the key alone, which works while a keyword means one thing — and two of
/// them do not. Draft-07's `dependencies` holds, per key, either an array of
/// property names or a subschema, and only its type says which. Under
/// `propertyNames`, `SCHEMA_NAME_INSTANCE_KEYWORDS` state *names*, and a name is
/// a string: an `examples` holding an object there states no name at all, so
/// skipping it from the key alone forwarded `{"propertyNames": {"examples":
/// [{"owner": "Martina Weber"}]}}` verbatim. A list of names cannot express
/// either rule, so the decision has to be able to look.
fn descend_into(shape: Shape, key: &str, value: &Value) -> Option<Shape> {
    // The two schema shapes share one body rather than one re-implementing the
    // other minus a few arms. `NameSchema` differs from `Schema` in exactly one
    // respect — the instances it constrains are property names — so it is a
    // modifier on a shape and not a second shape's worth of rules. Written as
    // two arms it drifted twice: `$defs` under `propertyNames` read definition
    // names against the keyword lists, which is the `properties` fault from two
    // rounds ago reproduced inside the newer shape.
    let names = match shape {
        Shape::Instance => return Some(Shape::Instance),
        // Every key here is a name the caller chose, and below it is a
        // subschema again.
        Shape::SchemaMap => return Some(Shape::Schema),
        // The array half is `dependentRequired` under its old name — property
        // names, every one of them, so the whole array is skipped. The object
        // half is `dependentSchemas`, an ordinary subschema. Anything else is
        // malformed, and scanning is the safe way to be wrong.
        //
        // **Every one of them, which is what this arm did not check.** It
        // skipped an array for being an array, so `{"dependencies": {"a":
        // [{"owner": "Martina Weber"}]}}` — an array holding an object, which
        // states no property name anywhere — travelled whole (measured, at
        // 200). This arm was written to answer a keyword whose meaning depends
        // on its value's type and then asked only half the question about that
        // type. `is_name_list` is the shared answer.
        Shape::DependencyMap => {
            return match value {
                value if is_name_list(value) => None,
                Value::Object(_) => Some(Shape::Schema),
                _ => Some(Shape::Instance),
            }
        }
        Shape::Schema => false,
        Shape::NameSchema => true,
    };
    let identifier = identifier_keyword(key);
    let applicator = applicator_keyword(key);
    match key {
        // The container check is on all three of these arms and on the
        // applicator arm below, and it is one rule: **a keyword's schema rules
        // apply to the container its own draft defines, and to nothing else.**
        // Round 3's audit read every arm here for whether it could return
        // `None`, on the ground that exemption is the only leak channel. That
        // is half the question. An arm returning `Some(Shape::Schema)` grants
        // the *rules*, and the rules skip things — so descending with the wrong
        // shape is a leak channel in its own right. All four arms decided from
        // the key alone and all four leaked (measured, at 200).
        //
        // `propertyNames` takes a schema. Given an array, the name-stating
        // rules were applied inside it, and `{"propertyNames": [{"enum":
        // ["Martina Weber"]}]}` skipped a name nothing states.
        "propertyNames" if Container::Schema.holds(value) => Some(Shape::NameSchema),
        // `dependencies` takes an object. Given an array, `{"dependencies":
        // [{"a": ["Martina Weber"]}]}` reached the array half of
        // `Shape::DependencyMap` through the elements and was skipped whole.
        "dependencies" if Container::Map.holds(value) => Some(Shape::DependencyMap),
        // `properties` and its siblings take objects, and the same route
        // through an array's elements reaches their keys: `{"properties":
        // [{"a": {"required": ["Martina Weber"]}}]}` was skipped.
        key if SCHEMA_MAP_KEYWORDS.contains(&key) && Container::Map.holds(value) => {
            Some(Shape::SchemaMap)
        }
        // An identifier keyword holding what that keyword's own draft says it
        // holds. The key names a shape and the value has to be it: `required`
        // is an array of strings, `type` a string or an array of them, `$ref`
        // a string. See `Identifier`.
        //
        // **And the strings in it have to be identifiers of that kind.** The
        // shape is the container, and the same question this match asks of
        // containers four arms up is a question about contents too: `{"type":
        // "Martina Weber"}` is a string where `type` takes a string, and
        // states no type, because JSON Schema publishes seven type names and
        // that is not one of them (measured, at 200 — and pinned as correct by
        // a test this branch wrote). `Content` is the second half, and its
        // third kind — property names, a regular expression — checks nothing,
        // deliberately.
        _ if identifier
            .is_some_and(|(shape, content)| shape.holds(value) && content.stated_by(value)) =>
        {
            None
        }
        // On the list, and holding something the keyword does not define. It
        // states no identifier, so it is the client's data and is scanned —
        // the third arm of this match to reach that conclusion, after
        // `dependencies` and the name-instance arm below. Scanned rather than
        // refused for the reason the name-instance arm gives: `descend_into`
        // has no refusal channel, and a malformed annotation costs an
        // over-masked schema the caller can see.
        _ if identifier.is_some() => Some(Shape::Instance),
        // Prose either way. Under `propertyNames` the values are names, but a
        // description of them is still a sentence.
        key if SCHEMA_ANNOTATION_KEYWORDS.contains(&key) => Some(Shape::Instance),
        // A subschema, so the shape carries — modifier included, because an
        // `enum` inside an `allOf` inside a `propertyNames` still lists names.
        //
        // **And only when the value is the container the keyword defines.**
        // `allOf` is an array of schemas; given an object it is not an `allOf`
        // at all, and granting `Shape::Schema` to it let the identifier arm one
        // level down skip `{"type": "Martina Weber"}` as a type name (measured,
        // at 200). `not` given an array is the same mistake the other way
        // round. `items` alone publishes both containers and gets both.
        _ if applicator.is_some_and(|container| container.holds(value)) => Some(shape),
        // Under `propertyNames` the instances are property names, and masking
        // one breaks the schema — but only for the keywords that state a name.
        // That set is small and closed; the set of keywords holding a client's
        // prose is neither, which is why this arm names the first rather than
        // defaulting to it. Skipping every unrecognized keyword here forwarded
        // an `x-note` under `propertyNames` verbatim while masking the
        // `description` beside it: the same inversion `SCHEMA_INSTANCE_KEYWORDS`
        // was deleted for, in the one shape that deletion did not reach.
        //
        // **And the keyword is not the whole of it: a name is a string.** The
        // arm decided from the key alone, so `{"propertyNames": {"examples":
        // [{"owner": "Martina Weber"}]}}` skipped an entire object subtree that
        // states no property name anywhere — a legal annotation, and `Weber`
        // reached the provider verbatim (measured, at 200). `dependencies` is
        // the precedent: a keyword whose meaning turns on its value's type
        // needs a rule per value, not a verdict per key.
        //
        // A string is a name. An array **of strings** is names — that is
        // `enum`'s and `examples`' shape, and every member of it is a name. An
        // object, or an array holding anything that is not a string, is not a
        // name and never was, so it is scanned as the client's data. Scanning
        // rather than refusing, because the value is malformed against
        // `propertyNames` and *some* client will write it: being wrong here
        // then costs an over-masked schema the caller can see, which is the
        // direction this walk chooses everywhere. Over-masking reaches the
        // strings that genuinely are names inside a mixed array too — they are
        // masked with the rest, and a schema the caller can see broken is the
        // cheaper of the two mistakes.
        key if names && SCHEMA_NAME_INSTANCE_KEYWORDS.contains(&key) => {
            // The same question the identifier arms ask, so the same
            // predicate: a name is a string, and a list of names is an array
            // of them. Written out here once and answered differently three
            // lines up is how two of these arms came to disagree.
            if Identifier::NameOrNames.holds(value) {
                None
            } else {
                Some(Shape::Instance)
            }
        }
        // Everything else: `enum` and `default` outside `propertyNames`,
        // `example`, `x-whatever`, and any keyword written after this line. It
        // holds an instance, and an instance is the client's data.
        _ => Some(Shape::Instance),
    }
}

/// A value a document carries, in the order the walk finds it. Keys are absent
/// by construction: this walk descends into values and never yields a name.
#[derive(Debug, Clone, PartialEq)]
pub enum Leaf {
    Text(String),
    /// Rendered as the client wrote it, and **looked at but never replaced**.
    ///
    /// The walk yields numbers because they have to be inspected: a credit
    /// card, a German tax ID and a French NIR are digits alone, so a document
    /// whose personal data sits in a numeric leaf would otherwise be covered by
    /// nothing. `mask_all` joins this rendering into the same detection call the
    /// document's strings make, and the rebuild still copies the number
    /// straight through — `replace_text_leaves` asks for a replacement per
    /// *text* leaf and this variant is not one.
    ///
    /// A `DETERMINISTIC_TYPES` span found inside one **refuses the request**
    /// rather than masking it: a schema that declared a number may reject a
    /// string, so the outcome for a number carrying an identifier is a refusal
    /// rather than a placeholder. An NER span on a numeric leaf does not refuse
    /// — a label on a bare digit run is grounded in nothing, and the reasoning
    /// and the counter-evidence are both at the predicate in `proxy::mask_all`.
    ///
    /// Even among the eight the refusal reaches only as far as the catalogs do,
    /// and they have no telephone entity — a phone number written as a JSON
    /// number is forwarded, exactly as it is in prose.
    Number(String),
}

/// What separates two leaves inside one detection call.
///
/// **Not a delimiter.** Nothing ever scans for it: splitting uses the character
/// ranges recorded while joining, so a leaf that happens to contain this
/// sequence changes nothing. Its only job is to stop the model reading the end
/// of one leaf and the start of the next as a single phrase, and a paragraph
/// break is what does that in the prose the model was trained on. It carries no
/// entity of its own, which a row of punctuation or a bracketed marker could
/// not promise.
const JOIN_SEPARATOR: &str = "\n\n";

/// Several text leaves presented to the detector as one call.
///
/// One call per string is not merely slow, it is blind: a two-character leaf
/// detected alone carries no context, and the same leaf inside its schema
/// carries the whole of it. What makes joining safe here — and what a general
/// chunker cannot claim — is that **we choose the boundaries rather than
/// discovering them**, so a span that crosses one is exactly detectable instead
/// of a thing to hope about.
pub struct Joined {
    text: String,
    /// One character range per leaf, in the order given. Characters, never
    /// bytes: the detector reports characters and `Mapping::mask` slices them,
    /// so a byte-based offset here would put a span inside the wrong leaf
    /// rather than out of range, and nothing would fail.
    ranges: Vec<std::ops::Range<usize>>,
}

impl Joined {
    pub fn of(leaves: &[&str]) -> Self {
        let mut text = String::new();
        let mut ranges = Vec::with_capacity(leaves.len());
        let mut at = 0usize;
        for (index, leaf) in leaves.iter().enumerate() {
            // Between leaves, never after the last: one leaf has to join to
            // itself byte for byte, or the detection cache stops recognising a
            // text it has already seen and every second turn pays again.
            if index > 0 {
                text.push_str(JOIN_SEPARATOR);
                at += JOIN_SEPARATOR.chars().count();
            }
            let length = leaf.chars().count();
            ranges.push(at..at + length);
            text.push_str(leaf);
            at += length;
        }
        Self { text, ranges }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// How many characters joining adds to what the detector reads, beyond the
    /// leaves themselves. Counted against `max_tool_chars` because the detector
    /// reads them: that bound is denominated in characters sent, and separators
    /// are characters sent.
    pub fn separator_chars(leaf_count: usize) -> usize {
        leaf_count.saturating_sub(1) * JOIN_SEPARATOR.chars().count()
    }

    /// Spans over the joined text, each returned to the leaf it came from and
    /// rebased to that leaf's start.
    ///
    /// The result has one entry per leaf, empty where the detector found
    /// nothing — `replace_text_leaves` corresponds by position, so a leaf
    /// without spans still has to occupy its place.
    pub fn split(&self, spans: &[Span]) -> Result<Vec<Vec<Span>>, MappingError> {
        let mut out = vec![Vec::new(); self.ranges.len()];
        for span in spans {
            let leaf = self
                .ranges
                .iter()
                .position(|range| {
                    range.start < range.end && range.start <= span.start && span.end <= range.end
                })
                // Inside no single leaf: either straddling two of them or
                // landing in a separator we inserted. Neither can be applied,
                // and neither may be dropped.
                .ok_or(MappingError::BadSpan("across a joined boundary"))?;
            let start = self.ranges[leaf].start;
            out[leaf].push(Span {
                entity_type: span.entity_type.clone(),
                start: span.start - start,
                end: span.end - start,
            });
        }
        Ok(out)
    }
}

pub fn json_leaves(value: &Value, shape: Shape) -> Result<Vec<Leaf>, MappingError> {
    let mut leaves = Vec::new();
    let mut nodes = 0usize;
    walk(value, shape, 0, &mut nodes, &mut leaves)?;
    Ok(leaves)
}

fn walk(
    value: &Value,
    shape: Shape,
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
                walk(item, shape, depth + 1, nodes, leaves)?;
            }
        }
        Value::Object(fields) => {
            // `fields` is iterated, never yielded: a key is the client's
            // dispatch, and masking one would break the call it dispatches.
            // `descend_into` says whether this field's *value* is one too.
            for (key, item) in fields {
                if let Some(inner) = descend_into(shape, key, item) {
                    walk(item, inner, depth + 1, nodes, leaves)?;
                }
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
pub fn replace_text_leaves(
    value: &Value,
    masked: &[String],
    shape: Shape,
) -> Result<Value, MappingError> {
    let mut next = 0usize;
    let mut nodes = 0usize;
    let result = replace(value, shape, 0, &mut nodes, masked, &mut next)?;
    if next != masked.len() {
        return Err(MappingError::MaskCountMismatch(
            "more masked strings than text leaves",
        ));
    }
    Ok(result)
}

fn replace(
    value: &Value,
    shape: Shape,
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
                .map(|item| replace(item, shape, depth + 1, nodes, masked, next))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        // The skipped fields are copied through untouched rather than dropped:
        // `descend_into` decides what is *scanned*, never what is kept.
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| {
                    let replaced = match descend_into(shape, key, item) {
                        Some(inner) => replace(item, inner, depth + 1, nodes, masked, next)?,
                        None => item.clone(),
                    };
                    Ok((key.clone(), replaced))
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

/// One step of a walk over a string: a run of ordinary characters, or a token
/// shaped like one this gateway issues.
#[derive(Debug, PartialEq, Eq)]
enum Piece<'a> {
    Text(&'a str),
    Placeholder(&'a str),
}

/// The one reading of *where the placeholders in this string are*.
///
/// Three callers ask it — `reserve_literals` on the way up, `restore` on the way
/// back, and `key_carries_an_issued_placeholder` — and they have to agree. They
/// were three copies of the same loop, and a copy is where the answers drift:
/// what one reserves the other must find. The `[` that begins nothing is
/// consumed one byte at a time rather than skipped to its `]`, because
/// swallowing the whole run would step over a real placeholder nested inside
/// it, as in `[see [PERSON_1]]`.
struct Pieces<'a> {
    rest: &'a str,
}

fn pieces(text: &str) -> Pieces<'_> {
    Pieces { rest: text }
}

impl<'a> Iterator for Pieces<'a> {
    type Item = Piece<'a>;

    fn next(&mut self) -> Option<Piece<'a>> {
        if self.rest.is_empty() {
            return None;
        }
        let Some(open) = self.rest.find('[') else {
            return Some(Piece::Text(std::mem::take(&mut self.rest)));
        };
        if open > 0 {
            let (before, from_open) = self.rest.split_at(open);
            self.rest = from_open;
            return Some(Piece::Text(before));
        }
        // An opening bracket with no closing one closes nothing: the rest of
        // the string is ordinary text.
        let Some(close) = self.rest.find(']') else {
            return Some(Piece::Text(std::mem::take(&mut self.rest)));
        };
        let candidate = &self.rest[..=close];
        if is_placeholder(candidate) {
            self.rest = &self.rest[close + 1..];
            Some(Piece::Placeholder(candidate))
        } else {
            self.rest = &self.rest[1..];
            Some(Piece::Text("["))
        }
    }
}

/// Every placeholder-shaped token the request body carries, from every string
/// in it — values, keys, and fields no slot addresses.
///
/// It looks for a lexical shape rather than for meaning, so it needs no
/// provider knowledge and **nothing may be exempt from it**. Exempting a field
/// is how `lookup_[PERSON_1]` in a tool name — dispatch, and deliberately not a
/// slot — would have been missed, and the echoed name restored to
/// `lookup_Martina Weber`.
#[allow(dead_code)]
pub fn placeholder_literals(value: &Value) -> HashSet<String> {
    let mut found = HashSet::new();
    collect_literals(value, &mut found);
    found
}

fn collect_literals(value: &Value, found: &mut HashSet<String>) {
    match value {
        Value::String(text) => {
            for piece in pieces(text) {
                if let Piece::Placeholder(candidate) = piece {
                    found.insert(candidate.to_owned());
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_literals(item, found);
            }
        }
        Value::Object(fields) => {
            for (key, item) in fields {
                // Keys as well as values: a property name is a string the
                // caller chose, and it reaches the response the same way.
                for piece in pieces(key) {
                    if let Piece::Placeholder(candidate) = piece {
                        found.insert(candidate.to_owned());
                    }
                }
                collect_literals(item, found);
            }
        }
        _ => {}
    }
}

/// `[TYPE_N]`: **one** opening bracket, upper-case type, underscore, digits,
/// **one** closing bracket.
///
/// The counting is the finding. This trimmed *every* leading `[` and *every*
/// trailing `]`, so `[[PERSON_1]]` — and, through the walk above, the
/// `[[PERSON_1]` it is read as — were judged tokens this gateway had issued.
/// At the key check that was a 502 on a legitimate property name; in `restore`
/// it was a 502 on text carrying a placeholder inside brackets, because the
/// candidate was taken as a token, looked up, and found in no mapping. Both
/// measured at `6a06391`. A placeholder this gateway issues has exactly one
/// bracket at each end, so that is what is read.
fn is_placeholder(candidate: &str) -> bool {
    placeholder_type(candidate).is_some()
}

/// The type name a placeholder carries, or `None` if the candidate is not one.
///
/// `is_placeholder` is this function asked a coarser question, so the two
/// cannot disagree about what a token is: reading the name out and deciding
/// whether there is a name to read are one walk.
fn placeholder_type(candidate: &str) -> Option<&str> {
    let inner = candidate
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))?;
    let (entity_type, number) = inner.rsplit_once('_')?;
    let well_formed = is_type_characters(entity_type)
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit());
    well_formed.then_some(entity_type)
}

/// The characters a type name is made of: non-empty, ASCII upper-case and
/// underscore.
///
/// It lives here rather than at either of its call sites because both of them
/// have to agree — the placeholder grammar above, which decides what this
/// gateway will restore, and `audit::is_entity_type`, which decides what may
/// become a key in the journal. Written out twice they were two grammars that
/// happened to match, and `audit`'s comment claimed a sharing that did not
/// exist until this function did.
///
/// The length bound is deliberately *not* here. `MAX_ENTITY_TYPE` is a bound on
/// what this gateway may issue and on what may reach a journal line; a longer
/// token in the caller's own text is still a token to be reserved and restored,
/// and folding the two rules together would quietly change that.
pub(crate) fn is_type_characters(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c == '_')
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
    fn a_value_is_named_as_its_type_only_when_the_provider_saw_that_name() {
        // What the journal's `redacted` is computed from. Three ways for the
        // detector's name not to reach the provider, and the field counts all
        // three because the provider does not distinguish them: the type was
        // not one we declare; the value already carried a placeholder issued
        // for another type; and the value was never masked at all.
        let mut mapping = Mapping::new();
        mapping
            .mask("Weber", &[span("PERSON", 0, 5)])
            .expect("masked");
        mapping
            .mask("Meier", &[span("WEBER", 0, 5)])
            .expect("masked");

        assert!(mapping.named_as("Weber", "PERSON"));
        assert!(
            !mapping.named_as("Weber", "ORG"),
            "the provider received [PERSON_1], whatever a second span called it"
        );
        assert!(
            !mapping.named_as("Meier", "WEBER"),
            "an undeclared type went up as [REDACTED_n] and the name went nowhere"
        );
        assert!(
            !mapping.named_as("4111111111111111", "PERSON"),
            "a value that was never masked carries no name at all"
        );
    }

    #[test]
    fn the_journal_and_the_placeholder_grammar_are_one_grammar() {
        // `audit::is_entity_type`'s comment claimed this sharing for as long as
        // the two predicates were written out separately, which is exactly the
        // arrangement in which they drift. `is_type_characters` is now the one
        // copy, and this is the assertion that a change to it is felt on both
        // sides rather than only where it was made.
        for name in ["PERSON", "DE_STEUER_ID"] {
            assert!(is_type_characters(name));
            assert!(is_placeholder(&format!("[{name}_1]")));
        }
        for name in ["", "person", "PHONE_E164", "PERSON-1"] {
            assert!(!is_type_characters(name), "{name}");
            assert!(!is_placeholder(&format!("[{name}_1]")), "{name}");
        }
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
    fn the_deterministic_types_are_a_subset_of_the_vocabulary() {
        // Two arrays that must agree, held together here rather than by the
        // care of whoever edits one of them. A name in `DETERMINISTIC_TYPES`
        // and not in `ENTITY_TYPES` would be a type the gateway refuses a
        // number for and then masks as REDACTED everywhere else.
        for entity_type in DETERMINISTIC_TYPES {
            assert!(
                ENTITY_TYPES.contains(&entity_type),
                "{entity_type} is deterministic but is not in this gateway's vocabulary at all"
            );
        }
    }

    #[test]
    fn the_two_halves_of_the_vocabulary_add_up() {
        // The subset check above passes if someone adds a ninth identifier to
        // `ENTITY_TYPES` alone: it is still a superset. This is the half that
        // notices, because a type added to one list and not the other changes
        // which side of `proxy::mask_all`'s numeric refusal it lands on — and
        // the side that does not refuse is the silent one.
        //
        // `scripts/check_entity_types.py` is the check that says *which*
        // catalog a name came from; this one needs no catalogs and runs in the
        // same `cargo test` as everything else.
        assert_eq!(
            ENTITY_TYPES.len() - DETERMINISTIC_TYPES.len(),
            14,
            "ENTITY_TYPES is eight identifiers from identifiers.yaml and fourteen NER types \
             from ner.yaml; if the detector gained one, decide deliberately which list it \
             belongs in and run `make check-entity-types`"
        );
        for entity_type in DETERMINISTIC_TYPES {
            assert_eq!(
                DETERMINISTIC_TYPES
                    .iter()
                    .filter(|name| **name == entity_type)
                    .count(),
                1,
                "{entity_type} is listed twice, which would make the arithmetic above agree \
                 while the partition is wrong"
            );
        }
    }

    #[test]
    fn a_schema_does_not_yield_the_property_names_it_states_as_values() {
        // The whole reason `Shape` exists. "Keys are never masked" is a rule
        // about position, and JSON Schema states property names in value
        // position too: masked, `required` names a property that does not
        // exist and the schema can no longer be satisfied.
        let schema = json!({
            "type": "object",
            "$id": "https://example.invalid/Weber",
            "$ref": "#/definitions/Weber",
            "required": ["Weber"],
            "pattern": "^Weber-[0-9]+$",
            "format": "date-time",
            "propertyNames": {"pattern": "^Weber$"},
            "dependentRequired": {"Weber": ["Schmidt"]},
            "description": "Belongs to Weber"
        });
        assert_eq!(
            json_leaves(&schema, Shape::Schema).unwrap(),
            vec![Leaf::Text("Belongs to Weber".to_owned())],
            "only the prose is data"
        );
        // The same document read as an instance has no keywords in it at all,
        // and every one of those strings is an ordinary value.
        assert!(
            json_leaves(&schema, Shape::Instance).unwrap().len() > 1,
            "Shape::Instance must not inherit the schema exemptions"
        );
    }

    #[test]
    fn a_schemas_instance_valued_keywords_go_back_to_scanning_everything() {
        // Without this the exemption list would be unsound rather than merely
        // conservative: `default` holds an *instance*, so a property called
        // `required` inside one is data, and skipping it by name would forward
        // a real name unmasked.
        let schema = json!({
            "type": "object",
            "default": {"required": "Martina Weber", "type": "Zurich"},
            "enum": ["Weber"],
            "const": "Schmidt"
        });
        let leaves = json_leaves(&schema, Shape::Schema).unwrap();
        assert!(
            leaves.contains(&Leaf::Text("Martina Weber".to_owned()))
                && leaves.contains(&Leaf::Text("Zurich".to_owned())),
            "a value under `default` is data whatever its key is called: {leaves:?}"
        );
        assert!(
            leaves.contains(&Leaf::Text("Weber".to_owned()))
                && leaves.contains(&Leaf::Text("Schmidt".to_owned())),
            "enum and const are values the model chooses among: {leaves:?}"
        );
    }

    #[test]
    fn a_property_named_like_a_keyword_is_still_a_property() {
        // The other way the exemption list can be wrong. Under `properties`
        // the keys are the *caller's* parameter names, and `type` is an
        // ordinary thing to call a parameter. Reading one as a keyword skips
        // its whole subschema, and the prose inside it reaches the provider
        // unmasked — the leak this list exists to avoid, arrived at from the
        // opposite direction.
        let schema = json!({
            "type": "object",
            "properties": {
                "type": {"type": "string", "description": "Weber chose this"},
                "required": {"type": "string", "default": "Schmidt"}
            },
            "$defs": {
                "format": {"title": "Bern"}
            }
        });
        let leaves = json_leaves(&schema, Shape::Schema).unwrap();
        assert!(
            leaves.contains(&Leaf::Text("Weber chose this".to_owned()))
                && leaves.contains(&Leaf::Text("Schmidt".to_owned()))
                && leaves.contains(&Leaf::Text("Bern".to_owned())),
            "a subschema's prose is data whatever the property is called: {leaves:?}"
        );
    }

    /// What a real agent's `tools` payload costs this gateway, counted by the
    /// walks that will actually count it rather than estimated.
    ///
    /// `testdata/claude_code_tools.json` is a transcription of ten tool
    /// definitions loaded in one Claude Code session — Read, Write, Edit, Bash,
    /// Skill, ToolSearch, Agent, Artifact, WebFetch, SendMessage — copied from
    /// the definitions as that session presented them. **It is a real tool set
    /// and it is still a floor.** A stock session also carries Glob, Grep,
    /// WebSearch, TodoWrite, NotebookEdit and others, and an MCP server adds
    /// more; these are the ten this session could transcribe verbatim rather
    /// than reconstruct from memory, which would not be measurement.
    ///
    /// | | measured |
    /// |---|---|
    /// | tools | 10 |
    /// | detector calls | **20** |
    /// | text leaves | 77 |
    /// | numeric leaves | 12 |
    /// | characters, text | 9 005 |
    /// | characters, numeric | 50 |
    /// | characters, join separators | 138 |
    /// | **characters charged** | **9 193** |
    /// | serialized bytes | 13 177 |
    ///
    /// Three things fall out. **The ratio is 1.43x**, so a bound charging
    /// serialized size charges nearly half again what detection costs. **The
    /// numbers are noise**: twelve leaves and fifty characters, four ten-
    /// thousandths of the payload, so joining them in — which is what makes
    /// them detectable at all — costs the bound nothing worth measuring. And
    /// **cost per tool is about 900 characters and 2 calls**, of which roughly
    /// 500 characters is the definition's own prose; the rest is schema. What
    /// makes a tool expensive is `enum`, whose every member is a value the
    /// model may choose and so is scanned — `Agent` and `Artifact` cost 21 and
    /// 24 leaves against three to six for a plain tool, so a per-tool average
    /// underestimates a payload carrying enum-heavy tools. Leaves stopped being
    /// what a call costs, but they are still what a *character* count is made
    /// of.
    ///
    /// **Both defaults are set to admit twice this payload**, which is the
    /// stated headroom: a stock fifteen-tool session extrapolates to roughly
    /// 13 800 characters and 30 calls, and doubling the floor leaves room for
    /// that plus a small MCP server. It is not room for an arbitrary one — see
    /// `config::max_tool_chars` for what that costs and why the answer to a
    /// bigger payload is issue #28 rather than a bigger number.
    ///
    /// **This test does not pin the charging rule; it illustrates it.** It sums
    /// the way `proxy::handle` sums rather than calling `handle`, so a change to
    /// `handle`'s counting passes here untouched — which is exactly what
    /// happened once. `proxy::tests::numeric_leaves_count_against_both_tool_bounds`
    /// goes through the request handler and is the test that fails when the rule
    /// moves. Read this one for the measurement and that one for the guarantee.
    #[test]
    fn a_real_tool_payload_fits_the_bounds_this_gateway_ships_with() {
        let tools: Vec<Value> =
            serde_json::from_str(include_str!("testdata/claude_code_tools.json")).unwrap();
        assert_eq!(tools.len(), 10);
        // Both kinds, counted apart, because they are charged the same and
        // read very differently: the numbers here are schema bounds, and
        // `numeric_leaves_are_a_rounding_error_in_a_real_tool_payload` below
        // is where what that costs and what it risks are written down.
        let mut text_leaves = 0usize;
        let mut numeric_leaves = 0usize;
        for tool in &tools {
            // One for the definition's own `description`, which is a slot of
            // its own rather than part of any join.
            text_leaves += usize::from(tool.get("description").is_some());
            for leaf in json_leaves(&tool["input_schema"], Shape::Schema).unwrap() {
                match leaf {
                    Leaf::Text(_) => text_leaves += 1,
                    Leaf::Number(_) => numeric_leaves += 1,
                }
            }
        }
        assert_eq!(text_leaves, 77, "text leaves");
        assert_eq!(
            numeric_leaves, 12,
            "numeric leaves, joined into the same calls"
        );
        // The headroom rule, asserted rather than described. The comment above
        // says both defaults are set to admit twice this payload; without this
        // that is a note somebody can quietly falsify, and `<= measured` alone
        // would pass at a default of 78.
        // What the bound counts now is *calls*, not leaves: one per tool
        // description, and one per schema that holds any leaf at all — of
        // either kind, since numbers go into the same join. Leaves stopped
        // being what costs the moment a document became one call.
        let calls: usize = tools
            .iter()
            .map(|tool| {
                usize::from(tool.get("description").is_some())
                    + usize::from(
                        !json_leaves(&tool["input_schema"], Shape::Schema)
                            .unwrap()
                            .is_empty(),
                    )
            })
            .sum();
        assert_eq!(calls, 20, "the figure the call bound is set from");
        // **This holds at 40 >= 40, exactly, with no headroom**, so one more
        // tool in the testdata breaks it. That is the assertion working, not a
        // fragility to file down: the payload is documented above as a floor,
        // and a floor that moved is a measurement to retake and a default to
        // reconsider. This branch has already been served once by exactly this
        // kind of failure, at 18 010 against 18 000. Raise the default or
        // re-measure; do not relax the rule.
        assert!(
            crate::config::default_max_tool_calls() >= 2 * calls,
            "the default bound must admit twice a real tool payload: {calls} calls \
             against a bound of {}. A gateway that refuses the tool set its own users \
             run is not configured conservatively, it is broken.",
            crate::config::default_max_tool_calls()
        );

        // The other bound, summed the way `proxy::handle` sums it: every leaf
        // of either kind, plus the separators the join inserts between them,
        // because the detector reads those too. Both figures are asserted so
        // that the gap between them stays visible — a bound charging serialized
        // size charges 1.43 characters for every one the detector reads.
        let mut chars = 0usize;
        let mut numeric_chars = 0usize;
        let mut separators = 0usize;
        for tool in &tools {
            chars += tool["description"]
                .as_str()
                .map_or(0, |d| d.chars().count());
            let leaves = json_leaves(&tool["input_schema"], Shape::Schema).unwrap();
            separators += Joined::separator_chars(leaves.len());
            for leaf in &leaves {
                match leaf {
                    Leaf::Text(text) => chars += text.chars().count(),
                    Leaf::Number(number) => numeric_chars += number.chars().count(),
                }
            }
        }
        assert_eq!(chars, 9_005, "characters of text");
        assert_eq!(
            numeric_chars, 50,
            "and of number — four ten-thousandths of the payload, which is what \
             detecting them costs the bound"
        );
        assert_eq!(separators, 138, "and what joining the leaves adds");
        let charged = chars + numeric_chars + separators;
        assert_eq!(charged, 9_193, "the figure the text bound is set from");
        let serialized: usize = tools
            .iter()
            .map(|tool| {
                tool["input_schema"].to_string().len() + tool["description"].to_string().len()
            })
            .sum();
        assert_eq!(
            serialized, 13_177,
            "and what charging structure would have cost"
        );
        assert!(
            crate::config::default_max_tool_chars() >= 2 * charged,
            "the default bound must admit twice a real tool payload: {charged} characters \
             against a bound of {}",
            crate::config::default_max_tool_chars()
        );
    }

    /// What the real payload's *numbers* are, and what a detector could make of
    /// them.
    ///
    /// The false-positive class this task opens, named rather than left to be
    /// discovered: a schema bound is a digit string and so is a card number.
    /// `9007199254740991` is `Number.MAX_SAFE_INTEGER`, it appears twice in
    /// `testdata/claude_code_tools.json`, and it is sixteen digits — inside the
    /// credit-card recognizer's 14-19 window. What keeps it out is the Luhn
    /// checksum, which is a property of the detector's recognizers rather than
    /// of this gateway: nothing here would stop a recognizer that matched on
    /// length alone from refusing every Claude Code request that carries a
    /// large integer bound.
    ///
    /// **Measured against the running detector**, not reasoned about: all ten
    /// schemas were joined exactly as `mask_all` joins them and sent to
    /// `/detect`. Zero spans landed on any number — including both
    /// `9007199254740991`, which the `credit_card` validator rejects because it
    /// fails Luhn. A real card in the same position *is* found, so the refusal
    /// works; it does not fire on this payload because nothing in this payload
    /// is one.
    ///
    /// One thing the measurement found that reading could not. Detection is not
    /// only the checksum layer: NER runs on the same joined text, and on
    /// `"9007199254740991\n\n9007199254740991"` — two big bounds with no prose
    /// between them — gliner returns `PERSON` at 0.723. **That measurement is
    /// what narrowed the refusal.** A number refuses only on a
    /// `DETERMINISTIC_TYPES` span now, so this label costs nobody a request;
    /// the reasoning, and the evidence against it, are written at the predicate
    /// in `proxy::mask_all`, which is where someone reversing the decision has
    /// to read.
    ///
    /// Two claims the earlier version of this comment made that later
    /// measurement did not support, kept here because they are the kind that
    /// get repeated. Prose does **not** reliably supply the missing context:
    /// the same two bounds behind "Maximum number of items" return nothing, but
    /// behind "The maximum number of items to return." still return `PERSON`.
    /// And *three* repeated bounds return nothing where two fire. The class is
    /// narrower than "repeated long digit runs" and less orderly.
    #[test]
    fn numeric_leaves_are_a_rounding_error_in_a_real_tool_payload() {
        let tools: Vec<Value> =
            serde_json::from_str(include_str!("testdata/claude_code_tools.json")).unwrap();
        let numbers: Vec<String> = tools
            .iter()
            .flat_map(|tool| json_leaves(&tool["input_schema"], Shape::Schema).unwrap())
            .filter_map(|leaf| match leaf {
                Leaf::Number(number) => Some(number),
                Leaf::Text(_) => None,
            })
            .collect();
        assert_eq!(
            numbers,
            [
                "0",
                "9007199254740991",
                "9007199254740991",
                "0",
                "5",
                "1000",
                "32",
                "1",
                "60",
                "50",
                "1",
                "200"
            ],
            "every number a real tool payload carries is a schema bound"
        );
        assert_eq!(
            numbers.iter().filter(|n| n.len() >= 14).count(),
            2,
            "and two of them are long enough for the credit-card recognizer to \
             consider, which is why the Luhn check is load-bearing rather than a detail"
        );
    }

    #[test]
    fn a_definition_map_under_property_names_still_reads_its_keys_as_names() {
        // The third instance of one fault: a shape that re-implements another
        // minus a few arms loses the arms it did not copy. `NameSchema` was
        // written without the map arm, so a definition called `type` under
        // `propertyNames` was read as a keyword and its whole subschema
        // skipped — the `properties` bug from two rounds earlier, reproduced
        // inside the newer shape. Both shapes now share one body.
        let schema = json!({
            "propertyNames": {
                "$defs": {"type": {"description": "Weber wrote this"}},
                "enum": ["credit_card"]
            }
        });
        let leaves = json_leaves(&schema, Shape::Schema).unwrap();
        assert!(
            leaves.contains(&Leaf::Text("Weber wrote this".to_owned())),
            "a definition's name is a name, not a keyword: {leaves:?}"
        );
        assert!(
            !leaves.contains(&Leaf::Text("credit_card".to_owned())),
            "and the enum here still lists property names: {leaves:?}"
        );
    }

    #[test]
    fn a_keyword_this_gateway_does_not_know_holds_data_rather_than_structure() {
        // The three lists are not symmetric, and this is the direction that
        // leaks. A missing identifier or map keyword over-masks — visible, and
        // it breaks a call. A keyword holding an *instance* that nothing
        // recognizes is walked as though it were schema structure, and then the
        // identifier list skips the client's own data inside it.
        //
        // OpenAPI 3.0's `example` is singular where JSON Schema's `examples` is
        // plural, so it matched nothing; a vendor extension matches nothing by
        // construction and no list can ever enumerate them.
        let schema = json!({
            "type": "object",
            "properties": {
                "user": {
                    "type": "object",
                    "example": {"id": "Martina Weber", "required": "Schmidt"},
                    "x-sample": {"format": "Zurich"}
                }
            }
        });
        let leaves = json_leaves(&schema, Shape::Schema).unwrap();
        for expected in ["Martina Weber", "Schmidt", "Zurich"] {
            assert!(
                leaves.contains(&Leaf::Text(expected.to_owned())),
                "{expected} is a value the client wrote, not schema structure: {leaves:?}"
            );
        }
    }

    #[test]
    fn the_structural_keywords_still_read_as_structure_underneath_it() {
        // The other half, and what makes "unknown means data" safe rather than
        // a trade of one fault for another: the keywords that really do hold
        // subschemas are named, so a property name nested under one of them is
        // still a property name. Treating unknown keys as data without this
        // list would mask every `required` below an `items` or an `allOf`.
        let schema = json!({
            "items": {"required": ["Weber"], "description": "one"},
            "allOf": [{"$ref": "#/definitions/Weber"}],
            "if": {"required": ["Schmidt"]},
            // A real type name, because after this round it has to be one:
            // `Zurich` stood here and is not one of the seven, so it stated no
            // type and is scanned like any other string. The keyword is still
            // read as structure, which is what this test is about.
            "not": {"type": "string", "required": ["Zurich"]},
            "additionalProperties": {"required": ["Meier"]}
        });
        assert_eq!(
            json_leaves(&schema, Shape::Schema).unwrap(),
            vec![Leaf::Text("one".to_owned())],
            "only the prose; every property name below an applicator is dispatch"
        );
    }

    #[test]
    fn a_property_names_subschema_carries_prose_and_names_and_is_split_by_position() {
        // `propertyNames` was skipped whole because *part* of it is dispatch.
        // Only part of it is: it is a subschema like any other, and its
        // `description` is prose that the same word elsewhere in the document
        // gets masked for. What is dispatch is what it *constrains* — the
        // strings it matches are property names — so its instance-valued
        // keywords list names rather than data, and those stay excluded.
        let schema = json!({
            "propertyNames": {
                "description": "Owned by Martina Weber",
                "pattern": "^Weber-[0-9]+$",
                "enum": ["credit_card", "billing_address"],
                "const": "Weber"
            }
        });
        assert_eq!(
            json_leaves(&schema, Shape::Schema).unwrap(),
            vec![Leaf::Text("Owned by Martina Weber".to_owned())],
            "the prose is scanned; the pattern and the names it may match are not"
        );
        // And it is copied through rather than dropped, like every other skip.
        let rebuilt = replace_text_leaves(&schema, &["MASKED".to_owned()], Shape::Schema).unwrap();
        assert_eq!(rebuilt["propertyNames"]["description"], "MASKED");
        assert_eq!(rebuilt["propertyNames"]["pattern"], "^Weber-[0-9]+$");
        assert_eq!(
            rebuilt["propertyNames"]["enum"],
            json!(["credit_card", "billing_address"])
        );
        assert_eq!(rebuilt["propertyNames"]["const"], "Weber");
    }

    #[test]
    fn an_unknown_keyword_under_property_names_holds_data_and_not_a_name() {
        // The lesson `SCHEMA_INSTANCE_KEYWORDS` was deleted for, applied to the
        // shape that deletion did not reach. Under `propertyNames` an
        // unrecognized keyword was skipped as though it stated a name, so a
        // vendor extension travelled verbatim while the `description` beside it
        // was masked — measured through the proxy as
        // `{"description":"[PERSON_1]","enum":["Weber"],"x-note":"Weber"}`.
        //
        // The keywords that really do state names are still skipped, because
        // that is what this shape exists for.
        let schema = json!({
            "propertyNames": {
                "x-note": "Martina Weber",
                "description": "Owned by Martina Weber",
                "enum": ["credit_card"],
                "const": "billing_address",
                "default": "billing_address",
                "examples": ["billing_address"]
            }
        });
        assert_eq!(
            json_leaves(&schema, Shape::Schema).unwrap(),
            vec![
                Leaf::Text("Owned by Martina Weber".to_owned()),
                Leaf::Text("Martina Weber".to_owned()),
            ],
            "an unknown keyword under propertyNames is the client's data"
        );
        let rebuilt = replace_text_leaves(
            &schema,
            &["MASKED".to_owned(), "ALSO MASKED".to_owned()],
            Shape::Schema,
        )
        .unwrap();
        assert_eq!(rebuilt["propertyNames"]["x-note"], "ALSO MASKED");
        assert_eq!(rebuilt["propertyNames"]["enum"], json!(["credit_card"]));
        assert_eq!(rebuilt["propertyNames"]["const"], "billing_address");
        assert_eq!(rebuilt["propertyNames"]["default"], "billing_address");
        assert_eq!(
            rebuilt["propertyNames"]["examples"],
            json!(["billing_address"])
        );
    }

    #[test]
    fn a_name_keyword_under_property_names_states_a_name_only_when_it_holds_strings() {
        // The second keyword whose meaning is in its value, and the same
        // lesson `dependencies` taught. `SCHEMA_NAME_INSTANCE_KEYWORDS` decided
        // from the key alone, so an `examples` holding an object skipped the
        // whole subtree — a legal annotation that states no property name
        // anywhere, and `Weber` reached the provider verbatim (measured through
        // the proxy, at 200).
        //
        // A property name is a string. So: a string is a name, an array of
        // strings is names, and everything else is the client's data.
        let schema = json!({
            "propertyNames": {
                "const": "billing_address",
                "enum": ["credit_card", "billing_address"],
                "default": "billing_address",
                "examples": [{"owner": "Martina Weber"}]
            }
        });
        assert_eq!(
            json_leaves(&schema, Shape::Schema).unwrap(),
            vec![Leaf::Text("Martina Weber".to_owned())],
            "a structured value under a name keyword is not a name and is scanned"
        );
        let rebuilt = replace_text_leaves(&schema, &["MASKED".to_owned()], Shape::Schema).unwrap();
        assert_eq!(
            rebuilt["propertyNames"]["examples"][0]["owner"], "MASKED",
            "and it is masked in place, with its keys untouched"
        );
        // The three that do state names are still skipped, whole.
        assert_eq!(rebuilt["propertyNames"]["const"], "billing_address");
        assert_eq!(
            rebuilt["propertyNames"]["enum"],
            json!(["credit_card", "billing_address"])
        );
        assert_eq!(rebuilt["propertyNames"]["default"], "billing_address");
    }

    #[test]
    fn an_identifier_keyword_states_an_identifier_only_when_its_value_has_that_shape() {
        // The third arm of this one `match` to be corrected for deciding from
        // the key alone, and the third occurrence of one mistake.
        // `dependencies` got a per-value rule, `SCHEMA_NAME_INSTANCE_KEYWORDS`
        // got one the round after — and this list sat four lines above the arm
        // that was fixed and was not touched, twice. `{"required": {"owner":
        // "Martina Weber"}}` states no property name anywhere and reached the
        // provider verbatim (measured through the proxy, at 200).
        let malformed = json!({
            "type": {"owner": "Martina Weber"},
            "required": {"owner": "Elif Yilmaz"},
            "$ref": ["Sofia Rossi"],
            "dependentRequired": {"a": {"owner": "Jan Novak"}}
        });
        assert_eq!(
            json_leaves(&malformed, Shape::Schema).unwrap(),
            vec![
                Leaf::Text("Sofia Rossi".to_owned()),
                Leaf::Text("Jan Novak".to_owned()),
                Leaf::Text("Elif Yilmaz".to_owned()),
                Leaf::Text("Martina Weber".to_owned()),
            ],
            "a value the keyword does not define states no identifier and is scanned"
        );

        // And the keywords go on meaning what they mean: an identifier the
        // keyword really does define is the caller's dispatch, skipped whole.
        // `required: ["Martina Weber"]` is in here deliberately — a property
        // genuinely named that way is a name, and masking it would break the
        // schema. That residual is unchanged by this fix.
        let well_formed = json!({
            "type": ["object", "null"],
            "required": ["Martina Weber"],
            "$ref": "#/$defs/Person",
            "dependentRequired": {"card": ["billing_address"]},
            "pattern": "^Martina Weber$",
            "format": "email"
        });
        assert!(json_leaves(&well_formed, Shape::Schema).unwrap().is_empty());

        // Each keyword is held to *its own* shape and not to the union of the
        // four, and this is where that is pinned. The population guard below
        // drives every keyword with the value its own classification names, so
        // it cannot see a keyword classified too *loosely* — `required` read
        // as `NameOrNames` would skip a bare string and every assertion in
        // that guard would still pass (measured: the mutation ran green). The
        // distinctions come one by one from the drafts instead.
        for (keyword, wrong_shape) in [
            // An array of names, never a bare name.
            ("required", json!("billing_address")),
            // A map of names to lists, never a bare list.
            ("dependentRequired", json!(["billing_address"])),
            // One string, never a list of them.
            ("$ref", json!(["#/$defs/Person"])),
            // A string or a list of strings, never a map.
            ("type", json!({"card": ["billing_address"]})),
        ] {
            let document = Value::Object([(keyword.to_owned(), wrong_shape)].into_iter().collect());
            assert_eq!(
                json_leaves(&document, Shape::Schema).unwrap().len(),
                1,
                "{keyword} skipped a shape another keyword on the list defines, not its own"
            );
        }
    }

    #[test]
    fn every_identifier_keyword_is_held_to_the_shape_its_draft_defines() {
        // The population rather than a case, which is what the last three
        // rounds of this branch keep proving is the difference between fixing
        // an instance and ending a class. Each keyword is driven with a value
        // its own draft defines and with one no keyword on this list defines;
        // the first must be skipped and the second must be scanned. A keyword
        // added here cannot compile without a shape, and one given the wrong
        // shape fails on one half or the other.
        for (keyword, shape, content) in SCHEMA_IDENTIFIER_KEYWORDS {
            // **Round 3's useful negative, in the column round 8 added.**
            // The content half below asserts that an `Arbitrary` keyword
            // *skips* a person's name, so a keyword loosened to `Arbitrary`
            // passes both halves and its grammar quietly stops being checked.
            // The keywords that hold a string the caller chose are named here,
            // so widening the third kind is an edit to this line.
            //
            // **`format` was added to this list in round 10 and the line is
            // the reason the round had to be deliberate about it.** Loosening
            // a keyword to `Arbitrary` buys a residual — that keyword now
            // forwards a person's name verbatim — and this assertion is where
            // that purchase is recorded, one name per line, rather than
            // arrived at by a check quietly going soft. `format` is here
            // because no draft constrains a format attribute past "MUST be a
            // string"; `required` and `dependentRequired` because property
            // names are the caller's own vocabulary.
            assert_eq!(
                content == Content::Arbitrary,
                ["dependentRequired", "format", "required"].contains(&keyword),
                "{keyword} is classified as holding a string the caller chose, and only \
                 property names and a format attribute are"
            );
            // One string that really is an identifier of this kind, out of the
            // draft that publishes the kind — and one that is an identifier of
            // no kind at all, which is the half round 8 added and the half
            // this round had to make per-content. `Martina Weber` was that
            // string for every keyword, and it stopped being one when
            // `pattern` moved off `Arbitrary`: a person's name is a perfectly
            // well-formed regular expression matching that literal, so the
            // near-miss for a regular expression has to be a *structural*
            // one. `None` means "nothing a caller can write fails this check",
            // and only the caller's own vocabulary may answer that.
            let (stated, unstated) = match content {
                Content::TypeName => ("object", Some("Martina Weber")),
                Content::UriReference => ("#/$defs/Person", Some("Martina Weber")),
                Content::Anchor => ("person", Some("Martina Weber")),
                Content::DynamicAnchor => ("person", Some("Martina Weber")),
                Content::MediaType => ("application/json", Some("Martina Weber")),
                Content::Token => ("base64", Some("Martina Weber")),
                Content::RegularExpression => ("^Martina Weber$", Some("Martina Weber(")),
                Content::Arbitrary => ("billing_address", None),
            };
            assert_eq!(
                unstated.is_none(),
                content == Content::Arbitrary,
                "{keyword}: only the caller's own vocabulary may have no near-miss, and \
                 every other kind must be driven with one"
            );
            let of_shape = |name: &str| match shape {
                Identifier::Name => json!(name),
                Identifier::NameOrNames | Identifier::Names => json!([name]),
                Identifier::NamesPerName => json!({ "card": [name] }),
            };
            let document =
                |value: Value| Value::Object([(keyword.to_owned(), value)].into_iter().collect());
            assert!(
                json_leaves(&document(of_shape(stated)), Shape::Schema)
                    .unwrap()
                    .is_empty(),
                "{keyword} scanned a value its own draft defines, which over-masks a valid schema"
            );
            // The content half. Same container, and a string that is an
            // identifier of no kind at all — which is what `{"type": "Martina
            // Weber"}` was, skipped on the strength of being a string.
            match unstated {
                Some(unstated) => assert_eq!(
                    json_leaves(&document(of_shape(unstated)), Shape::Schema).unwrap(),
                    vec![Leaf::Text(unstated.to_owned())],
                    "{keyword} skipped a string that states no identifier of its kind"
                ),
                None => assert!(
                    json_leaves(&document(of_shape("Martina Weber")), Shape::Schema)
                        .unwrap()
                        .is_empty(),
                    "{keyword} masked a string the caller chose, which breaks a schema that \
                     was correct"
                ),
            }
            // The one shape no keyword on this list defines, and the shape
            // Codex sent: an object whose values are the caller's prose.
            assert_eq!(
                json_leaves(&document(json!({"owner": "Martina Weber"})), Shape::Schema).unwrap(),
                vec![Leaf::Text("Martina Weber".to_owned())],
                "{keyword} skipped a subtree nothing scans"
            );
        }
    }

    #[test]
    fn a_dependency_list_states_names_only_when_every_member_is_a_string() {
        // Found by auditing the arms rather than by being shown the leak.
        // `dependencies`' array half was the *first* arm here to get a
        // per-value rule, and it asked only whether the value was an array —
        // half of the question its own comment poses. An array holding an
        // object states no property name anywhere, and it was skipped whole
        // (measured through the proxy, at 200).
        let schema = json!({"dependencies": {
            "card": ["billing_address", "owner"],
            "owner": [{"name": "Martina Weber"}]
        }});
        assert_eq!(
            json_leaves(&schema, Shape::Schema).unwrap(),
            vec![Leaf::Text("Martina Weber".to_owned())]
        );
        let rebuilt = replace_text_leaves(&schema, &["MASKED".to_owned()], Shape::Schema).unwrap();
        assert_eq!(rebuilt["dependencies"]["owner"][0]["name"], "MASKED");
        assert_eq!(
            rebuilt["dependencies"]["card"],
            json!(["billing_address", "owner"]),
            "and a real list of property names is still skipped whole"
        );
    }

    #[test]
    fn a_type_union_states_types_only_when_every_member_is_one() {
        // `type`'s array form is a union, and one member that names no type
        // means the array states no union. Scanned whole, `object` with it —
        // the same ruling `a_mixed_array_under_a_name_keyword_is_scanned_whole`
        // makes for a list of property names, and for the same reason: an
        // over-masked malformed schema is the mistake the caller can see.
        let schema = json!({"type": ["object", "Martina Weber"]});
        assert_eq!(
            json_leaves(&schema, Shape::Schema).unwrap(),
            vec![
                Leaf::Text("object".to_owned()),
                Leaf::Text("Martina Weber".to_owned()),
            ],
            "a union with a non-type in it states no union"
        );
        assert!(
            json_leaves(&json!({"type": ["object", "null"]}), Shape::Schema)
                .unwrap()
                .is_empty(),
            "and a real union is still a union"
        );
    }

    #[test]
    fn a_grammar_is_checked_where_its_own_draft_puts_the_boundary() {
        // **Found by mutation, and it is the guard's blind spot.** The
        // population guard drives every keyword with one string — a person's
        // name — and a person's name has a space in it, so *every* grammar
        // here rejects it for the same reason. Emptying `contentMediaType`'s
        // check down to "one token" left all 463 tests green: the `type/subtype`
        // half was doing real work and nothing was asking it to. So each
        // grammar gets a near-miss that only that grammar can catch.
        for schema in [
            // A media type is two tokens with a slash between them. One token
            // is a word, and `Weber` is a word.
            json!({"contentMediaType": "Weber"}),
            // And exactly one slash — the subtype is a token too.
            json!({"contentMediaType": "text/plain/Weber"}),
            // A parameter is `token=value`, and a bare word is neither half of
            // one. This is the boundary the essence check cannot see, and the
            // round that wrote the essence check said so in its own report.
            json!({"contentMediaType": "text/plain; Martina Weber"}),
            // The attribute is a token too, so a space inside it states no
            // parameter even when the `=` is there.
            json!({"contentMediaType": "text/plain; Martina Weber=1"}),
            // And a quoted string has to close.
            json!({"contentMediaType": "text/plain; name=\"Martina Weber"}),
            // **Found by mutation, and it is the same blind spot round 8
            // found once already.** Emptying the *value* check down to
            // "non-empty" left all 464 green: every other case here is caught
            // by the attribute half or by the essence, so nothing was asking
            // the value half anything. An unquoted value is a token, and a
            // token has no space in it — quoting is what a value with a space
            // needs, and the quoted form beside it in
            // `a_grammar_admits_what_clients_actually_write` is the control.
            json!({"contentMediaType": "text/plain; name=Martina Weber"}),
            // The same half at the end of the value, where the next parameter
            // would start: text after a closing quote states no parameter.
            json!({"contentMediaType": "text/plain; name=\"utf 8\" Martina Weber"}),
            // The anchor pattern starts at a letter or an underscore.
            json!({"$anchor": "1Weber"}),
            // ...and continues in a character class that has no slash in it.
            json!({"$dynamicAnchor": "Weber/Martina"}),
            // **The one character the two anchor drafts disagree about.**
            // 2019-09 permits `:` and 2020-12 does not, and `$dynamicAnchor`
            // exists only in 2020-12, so this is the boundary that separates
            // the two keywords. `$anchor` with the same string is a control in
            // `a_grammar_admits_what_clients_actually_write` — the pair is the
            // whole of H3.
            json!({"$dynamicAnchor": "Martina:Weber"}),
            // A group has to close. `Martina Weber` is a valid regular
            // expression and this is not, which is the sentence round 8 got
            // backwards.
            json!({"pattern": "Martina Weber("}),
            // A stray `)` is invalid in ECMA-262 too, Annex B included.
            json!({"pattern": "Martina Weber)"}),
            // A character class has to close.
            json!({"pattern": "[Martina Weber"}),
            // And a backslash has to have something to escape.
            json!({"pattern": "Martina Weber\\"}),
            // The seven type names are lowercase, and the drafts are
            // case-sensitive about them.
            json!({"type": "Object"}),
            // A token is non-empty. An empty `contentEncoding` names no
            // encoding in either RFC the drafts cite.
            //
            // **This case read `format` until round 10 and could not stay
            // there.** The two keywords shared `Content::Token` on an analogy
            // round 8 drew and no draft supports: `format`'s only constraint
            // is "MUST be a string", so the near-miss for a token grammar has
            // to be asked of the keyword that still has one. The value is the
            // same and the keyword is not.
            json!({"contentEncoding": ""}),
            // The other half of the same grammar, and the one that made the
            // token check worth keeping for this keyword: a space. RFC 2045
            // §6.1's `mechanism` is open at `ietf-token` and `x-token`, and
            // both of those are tokens.
            json!({"contentEncoding": "Martina Weber"}),
            // A URI-reference has no control character in it either, which is
            // the half of that rule a space does not reach.
            json!({"$ref": "#/defs/Weber\u{7f}"}),
        ] {
            assert!(
                !json_leaves(&schema, Shape::Schema).unwrap().is_empty(),
                "a string that is no identifier of this keyword's kind was skipped: {schema}"
            );
        }
        // The map container, asked of `Content` directly because no keyword
        // reaches it with a grammar: `dependentRequired` is the only keyword
        // whose value is a map of lists and it holds property names, so the
        // recursion's object arm always lands on `Arbitrary` and always says
        // yes. Emptying that arm to `true` leaves every other test green
        // (measured). This is what makes it a covered backstop rather than an
        // unreachable one.
        assert!(
            !Content::TypeName.stated_by(&json!({"card": ["Martina Weber"]})),
            "the recursion has to reach the strings inside a map, whatever container a \
             later keyword pairs with a grammar"
        );
        assert!(Content::TypeName.stated_by(&json!({"card": ["object"]})));
    }

    #[test]
    fn openapis_discriminator_is_walked_as_data_and_the_reason_is_recorded() {
        // **The ledger's other deferred name, closed as "nowhere".** It sat
        // beside `$recursiveRef` from Task 4 to round 10 and they are not the
        // same kind of item: `$recursiveRef` is one string with one published
        // grammar and belongs on `SCHEMA_IDENTIFIER_KEYWORDS`, and this is an
        // object holding three different kinds at once.
        //
        // The four leaves below are the whole argument. Two are identifiers
        // OpenAPI publishes as such — `mapping`'s right-hand side is "schema
        // names or references" — and masking one rewrites a reference, which
        // is precisely `$recursiveRef`'s bug. One is a property name, the
        // third kind. And the fourth is prose in an `x-` extension, which the
        // Discriminator Object explicitly permits ("This object MAY be
        // extended with Specification Extensions").
        //
        // A list entry is a verdict on the whole subtree, so entering
        // `discriminator` skips the prose with the references. Closing this
        // properly needs a `Shape` — a rule per field, which is what
        // `propertyNames` got — and until one exists the over-masked `$ref` is
        // the cost, in the direction this walk chooses when it must be wrong.
        // Pinned here so that changing it is a decision rather than a drift.
        let schema = json!({"oneOf": [{"type": "object"}], "discriminator": {
            "propertyName": "petType",
            "mapping": {"Weber": "#/$defs/Weber"},
            "x-note": "Martina Weber wrote this"
        }});
        assert_eq!(
            json_leaves(&schema, Shape::Schema).unwrap(),
            vec![
                Leaf::Text("#/$defs/Weber".to_owned()),
                Leaf::Text("petType".to_owned()),
                Leaf::Text("Martina Weber wrote this".to_owned()),
            ],
            "an exemption for `discriminator` would have to skip the extension holding a \
             sentence in order to spare the reference beside it"
        );
    }

    #[test]
    fn a_grammar_admits_what_clients_actually_write() {
        // The other direction, which is the one this branch has already been
        // wrong in once this week: a content check tighter than the drafts
        // masks a valid schema and breaks a working client. Each of these is a
        // real thing a real generator emits, and none of them may be scanned.
        for schema in [
            // RFC 3986 excludes `{` and `}` from a URI-reference, and every
            // OpenAPI document that references a templated path contains
            // them. The check reads whitespace and control characters only.
            json!({"$ref": "#/paths/~1pets~1{petId}/get"}),
            // A media type carries RFC 2045 parameters, and a parameter holds
            // a space. Only the essence is read.
            json!({"contentMediaType": "text/plain; charset=utf-8"}),
            // `format` is an extensible registry, so membership is not the
            // check: a format nobody has registered is still a format.
            json!({"format": "x-weber-account-id"}),
            // draft-04's `$id`, and an anchor written to either draft that
            // publishes the pattern.
            json!({"id": "https://example.invalid/schemas/person.json"}),
            json!({"$anchor": "person_1"}),
            // **This line read `{"$dynamicAnchor": "node.left:1"}` and it was
            // wrong.** It was written to pin the union as a *client-facing*
            // rule — an anchor written to 2019-09 must not be masked — and the
            // union is right for `$anchor`, which both drafts define. It is
            // not right for `$dynamicAnchor`, which only 2020-12 defines: a
            // `:` in one is not an older draft's spelling, it is no spelling
            // at all, and this assertion is what let `Martina:Weber` past.
            // Kept, moved to the keyword whose drafts earn it, and given the
            // 2020-12 form beside it so the pair still says what the line was
            // for.
            json!({"$anchor": "node.left:1"}),
            json!({"$dynamicAnchor": "node.left_1"}),
            // The empty URI-reference: the current document, which is how a
            // schema refers to itself.
            json!({"$ref": ""}),
            // A media type's parameters, which are read now and were not
            // before. All three of these are RFC 2045 and all three are
            // written by real clients: a token value, a quoted one holding
            // the space that is the whole reason quoting exists, and two
            // parameters in a row.
            json!({"contentMediaType": "text/plain; charset=\"utf 8\""}),
            json!({"contentMediaType": "multipart/form-data; boundary=abc; charset=utf-8"}),
            json!({"contentMediaType": "application/vnd.api+json; profile=\"a b\""}),
            // A regular expression the `regex` crate refuses and ECMA-262
            // accepts. Lookaround and backreferences are the reason this check
            // is structural rather than a parse: `regex-syntax` rejects both
            // (measured), and JSON Schema defines `pattern` as ECMA-262.
            json!({"pattern": "(?=Martina)Weber"}),
            json!({"pattern": "(?<=Weber)Martina"}),
            json!({"pattern": "(Martina) \\1"}),
            // And the dialects that disagree about brackets and braces: `[^]]`
            // is the ECMA-262 empty negated class then a literal `]`, and `a{`
            // is a literal brace. Rust's parser rejects `a{` and reads `[^]]`
            // as a different expression; neither may be masked.
            json!({"pattern": "[^]]+"}),
            json!({"pattern": "Martina Weber{"}),
            // The escapes, inside a class and out of it.
            json!({"pattern": "Martina Weber\\("}),
            json!({"pattern": "[\\]]Martina"}),
            json!({"pattern": "[()]Martina"}),
            // And a pattern that spells a person out, which is valid, is the
            // caller's own regular expression, and is forwarded. Unchanged by
            // this round and stated so that changing it is a decision.
            json!({"pattern": "^Martina Weber$"}),
            // **Round 10, and both of these were masked before it.**
            // `$recursiveRef` was on no list at all, so 2019-09's own way of
            // writing a recursive schema was walked as prose and a detection
            // rewrote the reference target. The draft says URI-reference; the
            // `"#"` everyone writes is one, and so is a file beside it.
            json!({"$recursiveRef": "#"}),
            json!({"$recursiveRef": "person.json"}),
            json!({"$recursiveRef": "https://example.invalid/schemas/person.json#/$defs/A"}),
            // A format attribute with a space in it. No draft forbids one —
            // "MUST be a string" is the whole rule and §7.2.3 invites custom
            // attributes — so the token check that refused it was stricter
            // than every draft, which is the mistake three lines of this test
            // already exist to catch in the other keywords.
            json!({"format": "Martina Weber"}),
        ] {
            assert!(
                json_leaves(&schema, Shape::Schema).unwrap().is_empty(),
                "a valid schema was masked, which breaks a client that was working: {schema}"
            );
        }
    }

    #[test]
    fn a_mixed_array_under_a_name_keyword_is_scanned_whole() {
        // The judgement call, pinned so that changing it is a decision. An
        // array holding anything that is not a string is not a list of property
        // names, so it is scanned — *including* the strings in it that would
        // have been names. Over-masking a schema the caller can see is the
        // cheaper of the two mistakes, and it is the direction this walk
        // chooses everywhere else.
        //
        // A number in there is a leaf too, which is what puts it in front of
        // the numeric refusal rather than past it.
        let schema = json!({
            "propertyNames": {"enum": ["billing_address", {"owner": "Martina Weber"}, 42]}
        });
        assert_eq!(
            json_leaves(&schema, Shape::Schema).unwrap(),
            vec![
                Leaf::Text("billing_address".to_owned()),
                Leaf::Text("Martina Weber".to_owned()),
                Leaf::Number("42".to_owned()),
            ]
        );
    }

    #[test]
    fn an_applicator_grants_schema_rules_only_to_the_container_its_draft_defines() {
        // **The counterexample to round 3's audit criterion.** That audit read
        // every arm of `descend_into` for whether it could return `None`, on
        // the stated ground that "an arm that always returns `Some` cannot
        // leak". This arm always returns `Some`, and it leaked: `allOf` must be
        // an *array* of schemas, and given an object it carried the schema
        // shape into it anyway, so the identifier arm one level down skipped
        // `type`'s value as a type name. `Martina Weber` reached the provider
        // with nothing having looked at it (measured through the proxy, at
        // 200). Which shape an arm returns decides which skip rules apply
        // below it, and the skip rules are the leak.
        let malformed = json!({"allOf": {"type": "Martina Weber"}});
        assert_eq!(
            json_leaves(&malformed, Shape::Schema).unwrap(),
            vec![Leaf::Text("Martina Weber".to_owned())],
            "an object is not an allOf, so nothing in it states a type"
        );
        // The well-formed twin, **corrected one round later and this is the
        // point of the round**. It read `{"allOf": [{"type": "Martina
        // Weber"}]}` and asserted the name was skipped, under a sentence
        // saying a real applicator states what its draft says it states. The
        // applicator was real; the type name was not. `Martina Weber` is not
        // one of the seven, so a well-formed `allOf` around it changes nothing
        // — the document still states no type.
        let well_formed = json!({"allOf": [{"required": ["Martina Weber"]}]});
        assert!(
            json_leaves(&well_formed, Shape::Schema).unwrap().is_empty(),
            "a real applicator still states what its draft says it states, and a property \
             name is one of those things"
        );
        let stating_no_type = json!({"allOf": [{"type": "Martina Weber"}]});
        assert_eq!(
            json_leaves(&stating_no_type, Shape::Schema).unwrap(),
            vec![Leaf::Text("Martina Weber".to_owned())],
            "a real container does not make its contents an identifier"
        );
        // The same mistake the other way round, which the brief's example does
        // not reach: `not` is a single schema and was given an array.
        let inverted = json!({"not": [{"required": ["Martina Weber"]}]});
        assert_eq!(
            json_leaves(&inverted, Shape::Schema).unwrap(),
            vec![Leaf::Text("Martina Weber".to_owned())],
        );
        // And the one keyword that publishes both containers keeps both.
        // Draft-07 gives `items` a tuple form; refusing it would mask every
        // property name in a draft-07 schema that uses one.
        for tuple in [
            json!({"items": [{"required": ["billing_address"]}]}),
            json!({"items": {"required": ["billing_address"]}}),
        ] {
            assert!(
                json_leaves(&tuple, Shape::Schema).unwrap().is_empty(),
                "both of `items`' published containers are `items`: {tuple}"
            );
        }
    }

    #[test]
    fn every_applicator_keyword_is_held_to_the_container_its_draft_defines() {
        // The population rather than the case, as the identifier list has. Each
        // keyword is driven with the container its own draft defines and with
        // the one it does not; the first must be skipped and the second must be
        // scanned. A keyword added to the list cannot compile without a
        // container, and one classified wrongly fails on one half or the other.
        //
        // The value inside is the same either way — a `required` naming
        // `Martina Weber` — so the only thing that can move the answer is the
        // container.
        let inner = || json!({"required": ["Martina Weber"]});
        for (keyword, container) in SCHEMA_APPLICATOR_KEYWORDS {
            // **Round 3's useful negative, applied before it could bite
            // again.** A keyword classified *too loosely* is invisible to a
            // guard that drives each keyword with the value its own
            // classification names: `SchemaOrSchemaArray` admits both, so both
            // halves pass and nothing says the keyword only publishes one.
            // `items` is the one keyword with two published containers —
            // draft-07's tuple form and 2020-12's single schema — so being that
            // keyword is the condition, and it is asserted rather than assumed.
            assert_eq!(
                container == Container::SchemaOrSchemaArray,
                keyword == "items",
                "{keyword} is classified as taking either container, and `items` is the only \
                 keyword whose drafts publish both"
            );
            let document =
                |value: Value| Value::Object([(keyword.to_owned(), value)].into_iter().collect());
            let defined: Vec<Value> = match container {
                Container::Schema => vec![inner()],
                Container::SchemaArray => vec![json!([inner()])],
                Container::SchemaOrSchemaArray => vec![inner(), json!([inner()])],
                Container::Map => unreachable!("no applicator takes a map"),
            };
            for value in defined {
                assert!(
                    json_leaves(&document(value), Shape::Schema)
                        .unwrap()
                        .is_empty(),
                    "{keyword} scanned a container its own draft defines, which over-masks a valid schema"
                );
            }
            let undefined = match container {
                Container::Schema => Some(json!([inner()])),
                Container::SchemaArray => Some(inner()),
                // Nothing is left to withhold from the keyword that takes both.
                Container::SchemaOrSchemaArray | Container::Map => None,
            };
            if let Some(value) = undefined {
                assert_eq!(
                    json_leaves(&document(value), Shape::Schema).unwrap(),
                    vec![Leaf::Text("Martina Weber".to_owned())],
                    "{keyword} granted schema rules to a container it does not define"
                );
            }
        }
    }

    #[test]
    fn a_keyword_whose_value_is_a_map_of_names_applies_only_to_a_map() {
        // The three arms round 3's audit listed as "left" on the criterion this
        // round corrected. All three decide the shape from the key and all
        // three grant rules that skip; an array reaches those rules through
        // `walk`'s propagation into elements, which is the route none of them
        // was read against. Each pair is the malformed container and its
        // well-formed twin, and the twin is what says this is a fix rather than
        // a narrowing of the schema language.
        for (malformed, well_formed) in [
            // Arm 6. Round 3 *recorded* this one and left it, on the argument
            // that it skips nothing the well-formed path would not also skip.
            // That is the argument `allOf` falsifies: the well-formed path
            // skips a name because a name is stated, and here none is.
            (
                json!({"propertyNames": [{"enum": ["Martina Weber"]}]}),
                json!({"propertyNames": {"enum": ["Martina Weber"]}}),
            ),
            // Arm 7 — `dependencies`, whose array half is the branch's own
            // precedent for per-value rules, reached through an element.
            (
                json!({"dependencies": [{"a": ["Martina Weber"]}]}),
                json!({"dependencies": {"a": ["Martina Weber"]}}),
            ),
            // Arm 8, one level deeper: `properties` as an array puts a
            // subschema under an element's key.
            (
                json!({"properties": [{"a": {"required": ["Martina Weber"]}}]}),
                json!({"properties": {"a": {"required": ["Martina Weber"]}}}),
            ),
        ] {
            assert_eq!(
                json_leaves(&malformed, Shape::Schema).unwrap(),
                vec![Leaf::Text("Martina Weber".to_owned())],
                "the keyword's rules were applied to a container it does not define: {malformed}"
            );
            assert!(
                json_leaves(&well_formed, Shape::Schema).unwrap().is_empty(),
                "and the container it does define is unchanged: {well_formed}"
            );
        }
    }

    #[test]
    fn a_draft_07_dependency_is_read_by_its_value_rather_than_its_name() {
        // The one keyword whose meaning is in its value's *type*. Draft-07's
        // `dependencies` holds, per key, either an array of property names
        // (2020-12 split this off as `dependentRequired`) or a subschema
        // (`dependentSchemas`). A rule that reads the key alone cannot tell
        // them apart, and getting it wrong either masks a property name or
        // stops scanning a subschema's prose.
        let schema = json!({
            "type": "object",
            "dependencies": {
                "credit_card": ["billing_address", "Weber"],
                "shipping": {"required": ["Weber"], "description": "Weber pays"}
            },
            "id": "https://example.invalid/Weber"
        });
        assert_eq!(
            json_leaves(&schema, Shape::Schema).unwrap(),
            vec![Leaf::Text("Weber pays".to_owned())],
            "the array lists property names; the object is a subschema whose prose is data"
        );
        // Both walks read the same rule, so the half that is skipped comes back
        // intact and the half that is scanned comes back replaced.
        let rebuilt = replace_text_leaves(&schema, &["MASKED".to_owned()], Shape::Schema).unwrap();
        assert_eq!(
            rebuilt["dependencies"]["credit_card"],
            json!(["billing_address", "Weber"])
        );
        assert_eq!(
            rebuilt["dependencies"]["shipping"]["required"],
            json!(["Weber"])
        );
        assert_eq!(rebuilt["dependencies"]["shipping"]["description"], "MASKED");
    }

    #[test]
    fn a_draft_04_id_is_an_identifier_like_its_prefixed_successor() {
        // draft-04 spelled `$id` as `id`. Masked, every `$ref` that resolves
        // against it becomes a pointer into a base URI that does not exist.
        let schema = json!({"id": "https://example.invalid/Weber", "title": "Weber"});
        assert_eq!(
            json_leaves(&schema, Shape::Schema).unwrap(),
            vec![Leaf::Text("Weber".to_owned())],
            "the title is prose; the id is a base URI"
        );
    }

    #[test]
    fn a_skipped_schema_keyword_is_copied_through_rather_than_dropped() {
        // `descend_into` decides what is scanned. It must never decide what is
        // kept: a schema that came back missing its `required` would be as
        // broken as one whose `required` was masked.
        let schema = json!({
            "required": ["Weber"],
            "$ref": "#/definitions/Weber",
            "description": "prose"
        });
        let rebuilt = replace_text_leaves(&schema, &["MASKED".to_owned()], Shape::Schema).unwrap();
        assert_eq!(rebuilt["required"], json!(["Weber"]));
        assert_eq!(rebuilt["$ref"], "#/definitions/Weber");
        assert_eq!(rebuilt["description"], "MASKED");
    }

    #[test]
    fn both_walks_agree_on_position_in_schema_shape_too() {
        // The correspondence the masking path rests on, under the shape that
        // makes the two walks skip things. They skip through one shared
        // function precisely so this cannot come apart.
        let schema = json!({
            "type": "object",
            "required": ["a", "b"],
            "description": "one",
            "properties": {
                "a": {"type": "string", "description": "two", "default": "three"},
                "b": {"$ref": "#/definitions/x", "title": "four"}
            }
        });
        let leaves = json_leaves(&schema, Shape::Schema).unwrap();
        let texts: Vec<String> = leaves
            .iter()
            .filter_map(|leaf| match leaf {
                Leaf::Text(text) => Some(text.clone()),
                Leaf::Number(_) => None,
            })
            .collect();
        // Arity agreeing is what `MaskCountMismatch` would otherwise catch;
        // asserting it here says the two walks agree rather than that one of
        // them noticed they did not.
        let masked: Vec<String> = texts.iter().map(|text| format!("<{text}>")).collect();
        let rebuilt = replace_text_leaves(&schema, &masked, Shape::Schema).unwrap();
        assert_eq!(rebuilt["description"], "<one>");
        assert_eq!(rebuilt["properties"]["a"]["description"], "<two>");
        assert_eq!(rebuilt["properties"]["a"]["default"], "<three>");
        assert_eq!(rebuilt["properties"]["b"]["title"], "<four>");
        assert_eq!(rebuilt["required"], json!(["a", "b"]));
        assert_eq!(rebuilt["properties"]["a"]["type"], "string");
    }

    fn span_at(entity_type: &str, start: usize, end: usize) -> Span {
        Span {
            entity_type: entity_type.to_owned(),
            start,
            end,
        }
    }

    #[test]
    fn one_leaf_joins_to_itself_exactly() {
        // The no-op case, and it has to be exact rather than equivalent: the
        // detection cache keys on the text, so a joined text that differs from
        // the leaf by even a trailing separator silently stops serving every
        // second turn, and nothing fails while it happens.
        let joined = Joined::of(&["Martina Weber"]);
        assert_eq!(joined.text(), "Martina Weber");
        assert_eq!(
            joined.split(&[span_at("PERSON", 0, 13)]).unwrap(),
            vec![vec![span_at("PERSON", 0, 13)]],
            "and the span comes back untouched"
        );
    }

    #[test]
    fn leaves_join_with_a_separator_and_spans_come_back_rebased() {
        let joined = Joined::of(&["Weber", "Bern", "Schmidt"]);
        assert_eq!(joined.text(), "Weber\n\nBern\n\nSchmidt");
        //                          0..5     7..11    13..20
        let split = joined
            .split(&[
                span_at("PERSON", 0, 5),
                span_at("LOCATION", 7, 11),
                span_at("PERSON", 13, 20),
            ])
            .unwrap();
        assert_eq!(
            split,
            vec![
                vec![span_at("PERSON", 0, 5)],
                vec![span_at("LOCATION", 0, 4)],
                vec![span_at("PERSON", 0, 7)],
            ],
            "each span is returned to its leaf and rebased to that leaf's start"
        );
    }

    #[test]
    fn a_leaf_with_no_spans_gets_an_empty_list_and_keeps_its_position() {
        // Positional correspondence is what `replace_text_leaves` rests on, so
        // a leaf the detector found nothing in must still occupy its slot.
        let joined = Joined::of(&["nothing", "Weber", "nothing"]);
        // "nothing" 0..7, "Weber" 9..14, "nothing" 16..23.
        let split = joined.split(&[span_at("PERSON", 9, 14)]).unwrap();
        assert_eq!(split.len(), 3);
        assert!(split[0].is_empty() && split[2].is_empty());
        assert_eq!(split[1], vec![span_at("PERSON", 0, 5)]);
    }

    #[test]
    fn offsets_are_characters_and_not_bytes() {
        // The whole join is arithmetic on offsets, and the detector speaks
        // characters while `String` speaks bytes. "Müller" is 6 characters and
        // 7 bytes, so a byte-based join puts every later leaf one place out —
        // and the failure would be a span landing inside the wrong leaf rather
        // than an error.
        let joined = Joined::of(&["Müller", "Weber"]);
        assert_eq!(joined.text().chars().count(), 6 + 2 + 5);
        assert_eq!(
            joined.text().len(),
            7 + 2 + 5,
            "and it really is longer in bytes"
        );
        assert_eq!(
            joined.split(&[span_at("PERSON", 8, 13)]).unwrap()[1],
            vec![span_at("PERSON", 0, 5)],
            "the second leaf starts at character 8, not byte 9"
        );
    }

    #[test]
    fn a_span_across_a_boundary_is_refused_rather_than_applied() {
        // The seam, and the reason the boundaries are recorded rather than
        // searched for: a span the detector believes covers two leaves cannot
        // be applied to either. Masking it in both invents a decision nobody
        // made and splits one placeholder across two values; dropping it
        // forwards the data. `BadSpan` already means exactly this — a span this
        // gateway cannot apply — and is already the 502 that names the
        // detector as the party that produced it.
        let joined = Joined::of(&["Weber", "Bern"]);
        assert!(matches!(
            joined.split(&[span_at("PERSON", 3, 9)]),
            Err(MappingError::BadSpan("across a joined boundary"))
        ));
        // A span that lies entirely in the separator we inserted is the same
        // failure: it belongs to no leaf.
        assert!(matches!(
            joined.split(&[span_at("PERSON", 5, 7)]),
            Err(MappingError::BadSpan("across a joined boundary"))
        ));
    }

    #[test]
    fn a_leaf_containing_the_separator_changes_nothing() {
        // The separator is not a delimiter and nothing scans for it, so a leaf
        // that happens to contain one splits exactly as any other would.
        let joined = Joined::of(&["first\n\nstill first", "Weber"]);
        // The first leaf is 18 characters whatever is inside it, so the second
        // starts at 20 — the embedded separator is just text.
        assert_eq!(
            joined.split(&[span_at("PERSON", 20, 25)]).unwrap()[1],
            vec![span_at("PERSON", 0, 5)]
        );
    }

    #[test]
    fn a_walk_reports_string_leaves_in_order_and_ignores_keys() {
        let document = json!({
            "b_second": "Weber",
            "a_first": {"nested": "Meier"},
            "list": ["Schmidt", 7, true, null]
        });
        let leaves = json_leaves(&document, Shape::Instance).unwrap();
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
        let result = replace_text_leaves(&document, &masked, Shape::Instance).unwrap();
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
        let text_leaf_count = json_leaves(&document, Shape::Instance)
            .unwrap()
            .into_iter()
            .filter(|leaf| matches!(leaf, Leaf::Text(_)))
            .count();
        // Each replacement encodes the position it should land in.
        let masked: Vec<String> = (0..text_leaf_count).map(|i| format!("leaf-{i}")).collect();
        let replaced = replace_text_leaves(&document, &masked, Shape::Instance).unwrap();
        let text_leaves_after: Vec<String> = json_leaves(&replaced, Shape::Instance)
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
        assert!(matches!(
            json_leaves(&document, Shape::Instance),
            Err(MappingError::TooDeep)
        ));
    }

    #[test]
    fn a_document_past_the_node_bound_is_refused() {
        let wide: Vec<Value> = (0..=MAX_JSON_NODES).map(|n| json!(n)).collect();
        assert!(matches!(
            json_leaves(&Value::Array(wide), Shape::Instance),
            Err(MappingError::TooLarge)
        ));
    }

    #[test]
    fn a_placeholder_shaped_key_in_a_response_is_refused_rather_than_restored() {
        // Nothing this gateway sends up ever masks a key, so a placeholder in
        // key position was written by the model — which sees placeholders in
        // the prose and may echo one anywhere. Restoring it renames the
        // property the client's tool reads its argument from; leaving it hands
        // the client our own token as a property name. Both break dispatch, so
        // the response is refused instead.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask(
                "Weber",
                &[Span {
                    entity_type: "PERSON".to_owned(),
                    start: 0,
                    end: 5,
                }],
            )
            .unwrap();
        assert_eq!(masked, "[PERSON_1]");

        assert!(matches!(
            mapping.restore_value(&json!({"[PERSON_1]": "anything"})),
            Err(MappingError::PlaceholderKey(key)) if key == "[PERSON_1]"
        ));
        // Nested just as much as at the top: the walk reaches every object.
        assert!(matches!(
            mapping.restore_value(&json!({"outer": [{"[PERSON_1]": 1}]})),
            Err(MappingError::PlaceholderKey(_))
        ));
        // A placeholder we never issued is refused too. Whether it names one of
        // ours is not the question — the client cannot dispatch on it either
        // way, and asking would leak which numbers this session has issued.
        assert!(matches!(
            mapping.restore_value(&json!({"[LOCATION_97]": 1})),
            Err(MappingError::PlaceholderKey(_))
        ));

        // A key that merely looks placeholder-ish is an ordinary key. The
        // brackets are the grammar, and `PERSON_1` without them is a perfectly
        // ordinary thing to call a field.
        assert_eq!(
            mapping
                .restore_value(&json!({"PERSON_1": "x", "[not a placeholder]": "y"}))
                .unwrap(),
            json!({"PERSON_1": "x", "[not a placeholder]": "y"})
        );
    }

    #[test]
    fn a_bracket_pair_is_counted_rather_than_trimmed() {
        // `is_placeholder` trimmed every leading `[` and every trailing `]`, so
        // `[[PERSON_1]]` and the `[[PERSON_1]` the walk reads it as were both
        // judged tokens this gateway had issued. One bracket at each end is
        // what it issues, so one is what is read.
        assert!(is_placeholder("[PERSON_1]"));
        assert!(is_placeholder("[REDACTED_12]"));
        assert!(!is_placeholder("[[PERSON_1]]"));
        assert!(!is_placeholder("[[PERSON_1]"));
        assert!(!is_placeholder("[PERSON_1]]"));
        assert!(!is_placeholder("PERSON_1"));
    }

    #[test]
    fn a_placeholder_nested_in_brackets_is_found_rather_than_stepped_over() {
        // The `[` that begins nothing is consumed one byte at a time so a real
        // placeholder inside it is still found — the rule the walk's own
        // comment always stated, and which the loose reading defeated for the
        // one case it most obviously covers. `[[PERSON_1]]` restored to
        // nothing: the candidate `[[PERSON_1]` was taken for a token, looked
        // up, found in no mapping, and the whole response refused (measured at
        // `6a06391`: 502, "no mapping for a placeholder in the upstream
        // response").
        let mut mapping = Mapping::new();
        assert_eq!(
            mapping.mask("Weber", &[span("PERSON", 0, 5)]).unwrap(),
            "[PERSON_1]"
        );
        assert_eq!(mapping.restore("[[PERSON_1]]").unwrap(), "[Weber]");
        assert_eq!(mapping.restore("[see [PERSON_1]]").unwrap(), "[see Weber]");
        assert_eq!(mapping.restore("[PERSON_1]").unwrap(), "Weber");
        // An opening bracket that closes nothing closes nothing.
        assert_eq!(mapping.restore("[PERSON_1").unwrap(), "[PERSON_1");
    }

    #[test]
    fn a_literal_nested_in_brackets_is_reserved_by_the_same_reading() {
        // `reserve_literals` and `restore` walk the same string and must agree
        // about where the tokens in it are: what one reserves the other has to
        // find. Under the loose reading the caller's own `[[PERSON_1]]`
        // reserved `[[PERSON_1]` and left the token inside it free to be issued
        // for somebody's name.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask("[[PERSON_1]] Weber", &[span("PERSON", 13, 18)])
            .unwrap();
        assert_eq!(
            masked, "[[PERSON_1]] [PERSON_2]",
            "the literal's own number was issued for a detected value"
        );
        assert_eq!(
            mapping.restore(&masked).unwrap(),
            "[[PERSON_1]] Weber",
            "the caller's literal came back as something else"
        );
    }

    #[test]
    fn a_key_carrying_a_token_this_session_issued_is_refused() {
        // The trap in the P2's obvious fix. `[[PERSON_1]]` stopped being a
        // placeholder when the brackets were counted, so an exact-match check
        // serves it — carrying `[PERSON_1]`, which this session issued for a
        // real name, to the client. Restoration exists to prevent exactly that.
        let mut mapping = Mapping::new();
        assert_eq!(
            mapping.mask("Weber", &[span("PERSON", 0, 5)]).unwrap(),
            "[PERSON_1]"
        );
        for key in ["[[PERSON_1]]", "owner[PERSON_1]", "a [PERSON_1] b"] {
            assert!(
                matches!(
                    mapping.restore_value(&json!({key: "x"})),
                    Err(MappingError::PlaceholderKey(_))
                ),
                "a key carried our own token to the client: {key} -> {:?}",
                mapping.restore_value(&json!({key: "x"}))
            );
        }
    }

    #[test]
    fn a_key_carrying_no_token_of_ours_is_served() {
        // Codex's P2. `[[PERSON_1]]` could never equal a token this gateway
        // issued, and the shape-only reading refused it anyway — a 502 on a
        // legitimate property name, which a templating client can plausibly
        // spell `[[ROW_1]]`. Nothing is renamed by serving it and nothing of
        // ours leaves in it.
        let mut mapping = Mapping::new();
        assert_eq!(
            mapping.mask("Weber", &[span("PERSON", 0, 5)]).unwrap(),
            "[PERSON_1]"
        );
        let served = json!({"[[ROW_1]]": "x", "[[PERSON_2]]": "y", "owner[ROW_1]": "z"});
        assert_eq!(
            mapping.restore_value(&served).unwrap(),
            served,
            "a key naming nothing this session issued was refused"
        );
    }

    #[test]
    fn a_key_carrying_the_callers_own_literal_is_served() {
        // `reserve_literals` maps a caller's literal to itself, so it is in
        // `by_placeholder` — and refusing on presence alone would refuse the
        // caller their own text back. The token has to be one we issued *for a
        // value*, which is what the self-mapping is not.
        let mut mapping = Mapping::new();
        let masked = mapping
            .mask("[[ROW_1]] and Weber", &[span("PERSON", 14, 19)])
            .unwrap();
        assert_eq!(masked, "[[ROW_1]] and [PERSON_1]");
        assert_eq!(
            mapping.restore_value(&json!({"[[ROW_1]]": "x"})).unwrap(),
            json!({"[[ROW_1]]": "x"}),
            "the caller's own literal came back as a refusal"
        );
    }

    #[test]
    fn a_number_survives_replacement_with_its_type_intact() {
        let document = json!({"count": 7, "nested": {"ratio": 1.5}});
        let result = replace_text_leaves(&document, &[], Shape::Instance).unwrap();
        assert_eq!(
            result, document,
            "a number is copied through untouched — examined by the detector, \
             never rewritten by the rebuild"
        );
    }

    #[test]
    fn a_token_is_restorable_only_if_this_request_issued_it_and_the_caller_did_not_write_it() {
        let issued = HashSet::from(["[PERSON_1]".to_owned(), "[IBAN_2]".to_owned()]);
        let written = HashSet::from(["[PERSON_1]".to_owned(), "[ORG_9]".to_owned()]);
        let provenance = Provenance::new(issued, written);

        // Issued and not written: the ordinary case, and the whole point.
        assert!(provenance.restorable("[IBAN_2]"));
        // Issued *and* written: the two occurrences are the same bytes in the
        // response, so neither can be told from the other. Left.
        assert!(!provenance.restorable("[PERSON_1]"));
        // Written only: the caller's own literal.
        assert!(!provenance.restorable("[ORG_9]"));
        // Neither: the model invented it.
        assert!(!provenance.restorable("[PERSON_7]"));
    }

    #[test]
    fn the_literal_walk_reads_every_string_including_the_ones_no_slot_addresses() {
        let body = json!({
            "model": "gpt",
            // A dispatch string. `reserve_literals` never sees one, because
            // dispatch is deliberately not a slot — this is the case that
            // would echo back as a broken tool name.
            "tools": [{"type": "function", "function": {"name": "lookup_[PERSON_1]"}}],
            "messages": [
                {"role": "user", "content": "see [ORG_2]"},
                // Nested, and in key position.
                {"role": "user", "content": {"[IBAN_3]": ["deep [PERSON_4]"]}}
            ],
            // Not placeholder-shaped: no type, no number.
            "metadata": {"note": "[not a token]"}
        });

        assert_eq!(
            placeholder_literals(&body),
            HashSet::from([
                "[PERSON_1]".to_owned(),
                "[ORG_2]".to_owned(),
                "[IBAN_3]".to_owned(),
                "[PERSON_4]".to_owned(),
            ])
        );
    }
}
