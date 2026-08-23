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
    #[error(
        "masked strings do not correspond to a document's text leaves ({0}); the request is \
             refused rather than served with a value misplaced or left in"
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
                        if key.starts_with('[') && key.ends_with(']') && is_placeholder(key) {
                            return Err(MappingError::PlaceholderKey(key.clone()));
                        }
                        Ok((key.clone(), self.restore_value(item)?))
                    })
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
const SCHEMA_IDENTIFIER_KEYWORDS: [&str; 14] = [
    "$anchor",
    "$dynamicAnchor",
    "$dynamicRef",
    "$id",
    "$ref",
    "$schema",
    "contentEncoding",
    "contentMediaType",
    "dependentRequired",
    "format",
    // draft-04 spelled `$id` this way. A schema written to that draft is
    // still a schema a client may send, and a masked base URI breaks every
    // `$ref` that resolves against it.
    "id",
    "pattern",
    "required",
    "type",
];

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
const SCHEMA_APPLICATOR_KEYWORDS: [&str; 15] = [
    "additionalItems",
    "additionalProperties",
    "allOf",
    "anyOf",
    "contains",
    "contentSchema",
    "else",
    "if",
    "items",
    "not",
    "oneOf",
    "prefixItems",
    "then",
    "unevaluatedItems",
    "unevaluatedProperties",
];

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

/// What the walk does with one field of an object, given the shape it is in.
/// Shared by both walks so they cannot drift: they correspond by position, and
/// a field one of them skipped and the other did not would silently put a
/// masked string somewhere it does not belong.
///
/// The `value` is here for one keyword. Every other rule reads the key alone,
/// which works while a keyword means one thing — and draft-07's `dependencies`
/// does not: per key it holds either an array of property names or a subschema,
/// and only its type says which. A list of names cannot express that, so the
/// decision has to be able to look.
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
        Shape::DependencyMap => {
            return match value {
                Value::Array(_) => None,
                Value::Object(_) => Some(Shape::Schema),
                _ => Some(Shape::Instance),
            }
        }
        Shape::Schema => false,
        Shape::NameSchema => true,
    };
    match key {
        "propertyNames" => Some(Shape::NameSchema),
        "dependencies" => Some(Shape::DependencyMap),
        key if SCHEMA_MAP_KEYWORDS.contains(&key) => Some(Shape::SchemaMap),
        key if SCHEMA_IDENTIFIER_KEYWORDS.contains(&key) => None,
        // Prose either way. Under `propertyNames` the values are names, but a
        // description of them is still a sentence.
        key if SCHEMA_ANNOTATION_KEYWORDS.contains(&key) => Some(Shape::Instance),
        // A subschema, so the shape carries — modifier included, because an
        // `enum` inside an `allOf` inside a `propertyNames` still lists names.
        key if SCHEMA_APPLICATOR_KEYWORDS.contains(&key) => Some(shape),
        // Everything else: `enum`, `default`, `example`, `x-whatever`, and any
        // keyword written after this line. It holds an instance, and an
        // instance is the client's data — unless we are under `propertyNames`,
        // where the instances are property names and masking one breaks the
        // schema.
        _ if names => None,
        _ => Some(Shape::Instance),
    }
}

/// A value a document carries, in the order the walk finds it. Keys are absent
/// by construction: this walk descends into values and never yields a name.
#[derive(Debug, Clone, PartialEq)]
pub enum Leaf {
    Text(String),
    /// Rendered as the client wrote it, and **nothing looks at it yet**.
    ///
    /// The walk yields numbers because they will have to be inspected: a credit
    /// card, a German tax ID and a French NIR are digits alone, so a document
    /// whose personal data sits in a numeric leaf is not covered by anything
    /// this gateway does today — `mask_all` matches this variant and does
    /// nothing with it, and the rebuild copies the number straight through.
    /// Task 6 is where the plan schedules detecting them and refusing the
    /// request when a span is found; until it lands, a numeric leaf reaches the
    /// provider verbatim. Do not read this variant's existence as coverage.
    ///
    /// What will not change is that a number is never *replaced*: a schema that
    /// declared a number may reject a string, so the outcome for a number that
    /// carries an identifier has to be a refusal rather than a placeholder.
    Number(String),
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
    /// | leaves (detector calls) | **77** |
    /// | characters detected | **9 005** |
    /// | serialized bytes | 13 177 |
    ///
    /// Two things fall out. **The ratio is 1.46x**, so a bound charging
    /// serialized size charges half again what detection costs. And **cost per
    /// tool is about 900 characters and 7.7 calls**, of which roughly 500
    /// characters is the definition's own prose; the rest is schema. What makes
    /// a tool expensive is `enum`, whose every member is a value the model may
    /// choose and so is scanned — `Agent` and `Artifact` cost 21 and 24 leaves
    /// against three to six for a plain tool, so a per-tool average
    /// underestimates a payload carrying enum-heavy tools.
    ///
    /// **Both defaults are set to admit twice this payload**, which is the
    /// stated headroom: a stock fifteen-tool session extrapolates to roughly
    /// 13 500 characters and 116 calls, and doubling the floor leaves room for
    /// that plus a small MCP server. It is not room for an arbitrary one — see
    /// `config::max_tool_chars` for what that costs and why the answer to a
    /// bigger payload is issue #28 rather than a bigger number.
    #[test]
    fn a_real_tool_payload_fits_the_bounds_this_gateway_ships_with() {
        let tools: Vec<Value> =
            serde_json::from_str(include_str!("testdata/claude_code_tools.json")).unwrap();
        assert_eq!(tools.len(), 10);
        let leaves: usize = tools
            .iter()
            .map(|tool| {
                // One for the definition's own `description`, which is a slot
                // of its own, plus every text leaf the schema yields.
                usize::from(tool.get("description").is_some())
                    + json_leaves(&tool["input_schema"], Shape::Schema)
                        .unwrap()
                        .iter()
                        .filter(|leaf| matches!(leaf, Leaf::Text(_)))
                        .count()
            })
            .sum();
        assert_eq!(leaves, 77, "the figure the leaf bound is set from");
        // The headroom rule, asserted rather than described. The comment above
        // says both defaults are set to admit twice this payload; without this
        // that is a note somebody can quietly falsify, and `<= measured` alone
        // would pass at a default of 78.
        assert!(
            crate::config::default_max_tool_leaves() >= 2 * leaves,
            "the default bound must admit twice a real tool payload: {leaves} leaves \
             against a bound of {}. A gateway that refuses the tool set its own users \
             run is not configured conservatively, it is broken.",
            crate::config::default_max_tool_leaves()
        );

        // The other bound, on the basis it is actually denominated in. Both
        // figures are asserted so that the gap between them stays visible: a
        // bound charging serialized size charges 1.49 characters for every one
        // the detector reads, and would refuse this payload at 10 970 against a
        // ceiling of 10 000 while the real cost is 7 379.
        let chars: usize = tools
            .iter()
            .map(|tool| {
                tool["description"]
                    .as_str()
                    .map_or(0, |d| d.chars().count())
                    + json_leaves(&tool["input_schema"], Shape::Schema)
                        .unwrap()
                        .iter()
                        .map(|leaf| match leaf {
                            Leaf::Text(text) => text.chars().count(),
                            // Charged nothing because nothing sends it. Task 6
                            // changes that, and this sum with it.
                            Leaf::Number(_) => 0,
                        })
                        .sum::<usize>()
            })
            .sum();
        assert_eq!(chars, 9_005, "the figure the text bound is set from");
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
            crate::config::default_max_tool_chars() >= 2 * chars,
            "the default bound must admit twice a real tool payload: {chars} characters \
             against a bound of {}",
            crate::config::default_max_tool_chars()
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
            "not": {"type": "Zurich"},
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
    fn a_number_survives_replacement_with_its_type_intact() {
        let document = json!({"count": 7, "nested": {"ratio": 1.5}});
        let result = replace_text_leaves(&document, &[], Shape::Instance).unwrap();
        assert_eq!(
            result, document,
            "a number is copied through untouched — and, until task 6, unexamined"
        );
    }
}
