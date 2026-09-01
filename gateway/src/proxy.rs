use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::audit::{Detected, Record};
use crate::config::Config;
use crate::detector::{DetectorClient, DetectorError};
use crate::mapping;
use crate::mapping::{Mapping, MappingError, Provenance, Span};
use crate::provider::{
    as_response_error, read_document, read_pointer, write_document, write_pointer, Anthropic,
    OpenAi, Provider, ShapeError, Slot,
};
use crate::session::{key_from, Limits, SessionError, SessionStore};

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("{0}")]
    Shape(#[from] ShapeError),
    #[error("{0}")]
    Detector(#[from] DetectorError),
    #[error("{0}")]
    Mapping(#[from] MappingError),
    #[error("upstream request failed: {0}")]
    Upstream(String),
    #[error("{0}")]
    Session(#[from] crate::session::SessionError),
    #[error("{0}")]
    Audit(#[from] crate::audit::AuditError),
    /// Neither message names the detector timeout, though both once did. That
    /// timeout bounds each detector call on its own; these two bound the work
    /// of one request across all of its calls, which is a different thing.
    #[error(
        "this request's tool structures carry more text than this gateway will detect for a \
         single request; it is refused rather than forwarded"
    )]
    ToolTooLarge,
    #[error(
        "this request's tool structures need more detector calls than this gateway will \
         make for a single request; it is refused rather than forwarded"
    )]
    TooManyToolCalls,
    /// The message names the failure and never the value. This variant exists
    /// *because* the number is personal data, so interpolating it into a body
    /// the client reads would hand back the thing the refusal was for — the
    /// mistake `MappingError::PlaceholderKey` and `MappingError::Unknown` were
    /// both corrected for earlier on this branch.
    #[error(
        "a number in this request's tool arguments carries personal data; it cannot be \
         masked without changing the field from a number to a string, so the request is \
         refused rather than forwarded with the value still in it"
    )]
    NumericPersonalData,
}

impl ProxyError {
    fn status(&self) -> StatusCode {
        match self {
            // A body we cannot read is the client's to fix, and it is refused
            // rather than forwarded unmasked. So is a session the gateway
            // cannot honour as asked. So is a tool document nested or sized
            // past what this gateway will walk: retrying will not help, and
            // 502 would tell the caller the upstream got it wrong, which is
            // false — they need to send something smaller instead.
            ProxyError::Shape(ShapeError::Request(_))
            | ProxyError::Shape(ShapeError::Unsupported(_, _))
            | ProxyError::Shape(ShapeError::MalformedDocument(_, _))
            | ProxyError::Mapping(MappingError::TooDeep)
            | ProxyError::Mapping(MappingError::TooLarge)
            | ProxyError::Session(SessionError::BadId)
            | ProxyError::Session(SessionError::Disabled)
            | ProxyError::Session(SessionError::NoCredential(_))
            | ProxyError::ToolTooLarge
            | ProxyError::TooManyToolCalls
            | ProxyError::NumericPersonalData => StatusCode::BAD_REQUEST,
            // Saturation is this gateway's own capacity rather than anything
            // the caller got wrong, and the same request may well succeed a
            // moment later. No `Retry-After`: the wait is another request's
            // detector round-trip, and the gateway has no honest number for it.
            // A journal that cannot be written is the same kind of fact about
            // this gateway rather than about the caller.
            ProxyError::Session(SessionError::Saturated) | ProxyError::Audit(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            // Everything the upstream or the detector got wrong, every shape
            // failure on the way back, and this gateway's own internal
            // mapping defects: an unresolvable placeholder, a detector span
            // that cannot be applied, or the two document walks disagreeing
            // with each other about how many leaves a document has. Written
            // out rather than left to a wildcard so that a new variant has to
            // be given a status here, as `audit_class` already makes it be
            // given a class: a wildcard turns a variant somebody forgot into
            // a silent 502.
            //
            // `Unrestorable` belongs in this arm and not in the 400 above, and
            // the two documents it is raised for say why: a serialized document
            // with two members of the same name, and one whose restored keys
            // would collide. Both are *the upstream's* bytes — the caller's
            // request is well formed, and there is nothing smaller or
            // differently shaped for them to send that would change the
            // answer. It is this gateway declining to serve a response it
            // cannot serve faithfully, which is what 502 is for.
            ProxyError::Shape(ShapeError::Response(_))
            | ProxyError::Shape(ShapeError::Pointer(_))
            | ProxyError::Detector(_)
            | ProxyError::Mapping(MappingError::Unknown(_))
            | ProxyError::Mapping(MappingError::BadSpan(_))
            | ProxyError::Mapping(MappingError::MaskCountMismatch(_))
            | ProxyError::Mapping(MappingError::PlaceholderKey(_))
            | ProxyError::Mapping(MappingError::Unrestorable(_))
            | ProxyError::Upstream(_) => StatusCode::BAD_GATEWAY,
        }
    }

    /// The fixed vocabulary the journal records. A class rather than the
    /// message, so that no expression in the audit writer could interpolate
    /// submitted text even if a message one day carried it.
    ///
    /// Matched with no `_` arm anywhere, over `ShapeError`, `DetectorError`,
    /// `MappingError` and `SessionError` as well as over this enum's own
    /// variants. `Shape(_)` used to collapse the last two of five, and it cost
    /// more than tidiness twice over.
    ///
    /// **`shape_response` blamed the provider for a defect of ours.** `status()`
    /// puts `ShapeError::Pointer` in the arm it describes as "this gateway's own
    /// internal defects", and it is: all four sites that raise it —
    /// `read_pointer`, `write_pointer`, `read_document`, `write_document` — are
    /// a pointer *this gateway produced* failing to resolve in a body it has
    /// already walked. The wildcard recorded that as `shape_response`, which is
    /// the class for a body the upstream sent, so the one line an auditor has
    /// pointed at the provider for a bug that is here. Two of those four sites
    /// arrived with this slice (`c230fb8`), and tool slots are where the gateway
    /// builds the most pointers of its own, so the population grew at the same
    /// time. `README.md` names which classes mean a defect here rather than
    /// there, because the prefix cannot: `shape_request` is the caller's body,
    /// `shape_response` the provider's, and `shape_pointer` neither.
    ///
    /// **And a new variant would have compiled**, taking a correct status from
    /// `status()` and a wrong class from this function, silently. Measured at
    /// `41eb85e` with a probe variant: one compile error, in `status()`, and
    /// none here. With the wildcard gone the same probe gives three, the third
    /// in `StreamError::audit_class`.
    fn audit_class(&self) -> &'static str {
        match self {
            ProxyError::Shape(ShapeError::Request(_)) => "shape_request",
            ProxyError::Shape(ShapeError::Unsupported(_, _)) => "shape_unsupported",
            ProxyError::Shape(ShapeError::MalformedDocument(_, _)) => "tool_arguments_malformed",
            ProxyError::Shape(ShapeError::Response(_)) => "shape_response",
            ProxyError::Shape(ShapeError::Pointer(_)) => "shape_pointer",
            ProxyError::Detector(DetectorError::Transport(_)) => "detector_transport",
            ProxyError::Detector(DetectorError::Status(_)) => "detector_status",
            ProxyError::Mapping(MappingError::Unknown(_)) => "mapping_unknown_placeholder",
            ProxyError::Mapping(MappingError::BadSpan(_)) => "mapping_bad_span",
            ProxyError::Mapping(MappingError::TooDeep) => "mapping_too_deep",
            ProxyError::Mapping(MappingError::TooLarge) => "mapping_too_large",
            ProxyError::Mapping(MappingError::MaskCountMismatch(_)) => "mapping_mask_mismatch",
            ProxyError::Mapping(MappingError::PlaceholderKey(_)) => "mapping_placeholder_key",
            // Named for the loss that was about to be taken rather than for
            // the variant, because the variant's name is already spoken for
            // one level down: `stream_unrestorable` is `MappingError::Unknown`
            // on the streamed path, and a second class one word longer would
            // read as its document-shaped sibling when it is a different
            // failure entirely. `lossy_document` is what the code calls the
            // question, so the class and the seam that raises it grep to each
            // other.
            ProxyError::Mapping(MappingError::Unrestorable(_)) => "mapping_lossy_document",
            ProxyError::Upstream(_) => "upstream_failed",
            ProxyError::Session(SessionError::BadId) => "session_bad_id",
            ProxyError::Session(SessionError::Disabled) => "session_disabled",
            ProxyError::Session(SessionError::NoCredential(_)) => "session_no_credential",
            ProxyError::Session(SessionError::Saturated) => "session_saturated",
            ProxyError::Audit(_) => "audit_write_failed",
            ProxyError::ToolTooLarge => "tool_too_large",
            ProxyError::TooManyToolCalls => "tool_too_many_calls",
            ProxyError::NumericPersonalData => "tool_numeric_personal_data",
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        // The reason names the failure. It never carries the submitted text.
        (self.status(), Json(json!({ "error": self.to_string() }))).into_response()
    }
}

/// Headers the upstream needs to authenticate and route the call. An allowlist
/// rather than a passthrough: `host`, `content-length` and friends belong to
/// the hop we are making, not the one we received, and forwarding a client's
/// cookies to a model provider is nobody's intent.
/// Per provider, so a caller holding credentials for both does not send the
/// Anthropic key to OpenAI. A secret crossing a provider boundary is a leak of
/// a different kind than the one this proxy was built for, but a leak.
const OPENAI_HEADERS: [&str; 4] = [
    "authorization",
    "openai-organization",
    "openai-project",
    "openai-beta",
];
const ANTHROPIC_HEADERS: [&str; 3] = ["x-api-key", "anthropic-version", "anthropic-beta"];

/// Response headers a client needs to behave well against a provider that is
/// pushing back.
const RETURNED_HEADERS: [&str; 6] = [
    "retry-after",
    "x-ratelimit-limit-requests",
    "x-ratelimit-remaining-requests",
    "x-ratelimit-reset-requests",
    "anthropic-ratelimit-requests-remaining",
    "anthropic-ratelimit-requests-reset",
];

pub struct AppState {
    pub detector: DetectorClient,
    pub upstream: reqwest::Client,
    pub openai_base: String,
    pub anthropic_base: String,
    pub sessions: SessionStore,
    pub audit: Arc<crate::audit::Audit>,
    /// How much tool structure one request may carry. Not a session limit and
    /// not a mapping limit: `mapping.rs`'s bounds stop a single document from
    /// exhausting the stack, and this one stops a request whose detection would
    /// outlast the detector timeout — a different failure, so a different
    /// number, and it lives here because `handle` is where it is asked.
    pub max_tool_chars: usize,
    /// How many detector round-trips that structure may need — one per tool
    /// description and one per schema holding any text, since a document is
    /// detected in a single call. Bounds *calls* where `max_tool_chars` bounds
    /// *size*.
    pub max_tool_calls: usize,
}

impl AppState {
    pub fn from_config(config: &Config, audit: Arc<crate::audit::Audit>) -> Self {
        Self {
            detector: DetectorClient::new(
                config.detector_url.clone(),
                Duration::from_secs(config.detector_timeout_secs),
                config.detection_cache_entries,
                config.max_spans_per_entry,
            ),
            upstream: reqwest::Client::new(),
            openai_base: config.openai_base.clone(),
            anthropic_base: config.anthropic_base.clone(),
            sessions: SessionStore::new(Limits {
                idle: Duration::from_secs(config.session_idle_secs),
                max_sessions: config.max_sessions,
                max_values: config.max_session_values,
            }),
            audit,
            max_tool_chars: config.max_tool_chars,
            max_tool_calls: config.max_tool_calls,
        }
    }

    fn base_for(&self, provider: &dyn Provider) -> &str {
        match provider.name() {
            "anthropic" => &self.anthropic_base,
            _ => &self.openai_base,
        }
    }
}

/// One text's occurrences, tallied per distinct (type, value) into the
/// caller's map.
///
/// A free function so both slot kinds count the same way: inlined twice they
/// would drift, and the counts are what the journal reports about the request.
/// The values themselves live in that map only long enough to be counted and
/// never leave `mask_all`.
///
/// **The keys are the findings and the counts are the occurrences**, because
/// the journal reports both and they answer different questions: `types` is
/// how many distinct values were found under a name, and `redacted` and
/// `forwarded` are how many *occurrences* met each fate. One value can meet
/// two fates in one request — masked in a string leaf, forwarded in a numeric
/// one — so a fate cannot be a property of the finding, and counting fates
/// per key was a partition over an object that does not have exactly one.
fn count_occurrences(text: &str, spans: &[Span], found: &mut HashMap<(String, String), usize>) {
    // The `Vec<char>` is a copy of the whole text, and a conversation
    // history is many texts: it is built only when there is a span to
    // address into it.
    if spans.is_empty() {
        return;
    }
    let characters: Vec<char> = text.chars().collect();
    for span in spans {
        // Offsets are in characters, and a span the mapping would
        // reject is counted only if it addresses real text.
        if let Some(value) = characters.get(span.start..span.end) {
            *found
                .entry((span.entity_type.clone(), value.iter().collect()))
                .or_default() += 1;
        }
    }
}

/// Detect and mask every text the provider pointed at, and report what was
/// found and what became of it. Shared by both branches of `handle`: inline it
/// would exist twice and diverge at the first edit.
///
/// (This paragraph was attached to `count_distinct` below, which is not the
/// function it describes. Moved rather than rewritten.)
///
/// **Every count describes *this request*.** Reading one off the mapping
/// instead reports a session's running total, or — worse, because it looks
/// right — a number that quietly depends on what an earlier turn happened to
/// allocate: that is exactly what `redacted` was, and two turns of identical
/// traffic wrote two different evidence lines because of it. The mapping is
/// still consulted, but only ever with a value from this request in hand, and
/// only to ask what the provider received for it.
async fn mask_all(
    provider: &'static str,
    detector: &DetectorClient,
    body: &Value,
    slots: &[Slot],
    mapping: &mut Mapping,
    credential: Option<&[u8]>,
) -> Result<(Value, Detected), ProxyError> {
    let mut masked = body.clone();
    let mut total = 0usize;
    let mut found: HashMap<(String, String), usize> = HashMap::new();
    // Occurrences on a numeric leaf, which this gateway deliberately forwards
    // rather than masking — see the long comment at the exemption below. They
    // are collected structurally, from the leaf the span landed in, rather than
    // inferred at the end from "no placeholder was issued for this value": a
    // value that is both a number here and a string somewhere else in the same
    // request has a placeholder *and* went out verbatim, and the journal has to
    // say both. Keyed like `found` and counted the same way, so the two are
    // subtractable: what this map does not hold of a finding's occurrences is
    // exactly what was masked.
    let mut forwarded: HashMap<(String, String), usize> = HashMap::new();
    // What this request asked a detection for, which is not the same as what
    // the request holds: a `Slot::Json` asks one detection however many leaves
    // it has, and asks none at all when it has none.
    //
    // Nor is it the same as what the *detector* was shown. Both counters sit
    // beside `detect`, which answers a repeat text under the same credential
    // from the detection cache without a call, so two identical messages are
    // `texts: 2` against one request at the detector's door — measured, and
    // pinned by `two_texts_the_cache_answers_once_are_two_detections_asked_for`.
    // The README says "detections asked for" for this reason, and it was
    // corrected to say it. Three sentences written in the same commit were not:
    // this comment and the two field docs in `audit::Detected` all said "one
    // detector call", and so did the README's own paragraph two sentences
    // earlier. The cache that falsified one falsified all four.
    let mut texts = 0usize;
    let mut documents = 0usize;
    // **Every literal in the request, reserved before the first allocation.**
    // `Mapping::mask` reserves the text it is handed, which is one string; the
    // collision is between two. With `{"a": "Martina Weber", "z":
    // "[PERSON_1]"}` leaf `a` is allocated `[PERSON_1]` first, and
    // `reserve_literals` then finds the key taken and cannot map the caller's
    // own literal to itself — so the provider's echo of it restores to
    // `Martina Weber`, in a tool argument the client's agent may execute.
    // Measured at `a1ccd3d`, before this pass existed: the client received
    // `{"z": "Martina Weber"}` for a `z` it had written as `[PERSON_1]`, and
    // the upstream saw *both* leaves as the same token — the two strings had
    // already stopped being distinguishable on the way up.
    //
    // **Slots, not leaves.** Every `Slot::Text` masks too, so a plain message
    // ahead of a document takes the number the document's literal needed, and
    // a document ahead of a `tool_result` does the same to it. Two loops over
    // the same slots is the price, and it is walking only: no detector call is
    // made here.
    //
    // **A document is read as text, not walked.** Reservation is not masking:
    // it renames nothing and only claims a number, so it has no business
    // knowing which positions are data. Serializing and scanning covers the
    // positions the walk deliberately skips — a key, and a schema identifier
    // such as `{"required": ["[PERSON_1]"]}`, both of which are forwarded
    // verbatim and so can have their number issued to somebody's name behind
    // their back. Measured with the leaf walk here instead: the schema above
    // sent `Martina Weber` up as `[PERSON_1]` and the client got her name back
    // in a tool argument. Reading `Shape::Instance` leaves instead would cover
    // the identifiers and not the keys, and would refuse besides — `walk`
    // counts only the nodes it descends into, so an `Instance` reading of a
    // schema can reach `MAX_JSON_NODES` where the `Schema` reading `handle`
    // charged did not, and a pass that only claims numbers must never refuse a
    // request the masking pass would have served.
    //
    // JSON's own brackets forge nothing: `["[PERSON_1]"]` scans as the
    // candidate `["[PERSON_1]`, which the grammar rejects, and the walk then
    // advances one byte and finds the token inside — the rule `pieces` already
    // exists for. No quote, comma or colon can sit inside `[TYPE_N]`.
    //
    // **A session outlives a request, and this pass does not.** A literal the
    // caller writes on turn three against a number issued on turn one is not
    // reachable from here in any order — the allocation happened before the
    // literal existed — and it restores to that turn's value. Measured, not
    // fixed, and recorded rather than papered over.
    for slot in slots {
        match slot {
            Slot::Text { pointer, .. } => {
                mapping.reserve_literals(&read_pointer(body, pointer)?);
            }
            Slot::Json {
                pointer,
                embedded,
                shape: _,
            } => {
                let document = read_document(body, pointer, *embedded, provider)?;
                mapping.reserve_literals(&document.to_string());
            }
        }
    }
    for slot in slots {
        match slot {
            Slot::Text { pointer, .. } => {
                let text = read_pointer(body, pointer)?;
                let spans = detector.detect(&text, credential).await?;
                texts += 1;
                total += spans.len();
                count_occurrences(&text, &spans, &mut found);
                write_pointer(&mut masked, pointer, &mapping.mask(&text, &spans)?)?;
            }
            // A document's string leaves are detected one at a time and put
            // back by position: `json_leaves` and `replace_text_leaves` walk
            // the same shape in the same order, and neither yields a key in key
            // position. `shape` is what handles the positions where a schema
            // names a property as a *value* — `required`, `$ref` and the rest —
            // which position alone cannot tell from prose.
            Slot::Json {
                pointer,
                embedded,
                shape,
            } => {
                let document = read_document(body, pointer, *embedded, provider)?;
                // Already counted against `max_tool_calls` in `handle`, before
                // this round-trip was made.
                let leaves = mapping::json_leaves(&document, *shape)?;
                // **One call for the whole document.** Per string it was not
                // merely slow — 77 calls against a real tool payload measured
                // 54.9 s, of which 30 s was per-call overhead — it was blind:
                // a two-character leaf detected alone carries no context, and
                // inside its schema it carries the whole of it.
                //
                // Joining is safe here in a way a general chunker cannot be,
                // because the boundaries are ours: `Joined` records where each
                // leaf went and `split` returns every span to the leaf it came
                // from, refusing the ones that cross. Masking below is still
                // leaf by leaf, so `mask`, `check_spans`, `reserve_literals`,
                // placeholder allocation and the positional correspondence
                // `replace_text_leaves` rests on are all untouched.
                //
                // **Numbers join the same call.** A credit card, a German tax
                // ID and a French NIR are digits alone, so a document whose
                // personal data sits in a numeric leaf needs the detector to
                // read it — and it reads it here, in the call the strings are
                // already making, rather than in a round-trip of its own that
                // would cost a second charge against `max_tool_calls` and give
                // back the overhead this whole slice spent itself removing.
                //
                // Two consequences, written down rather than hidden. A number
                // now supplies context to the text leaves around it, so
                // detection results for documents that already worked may
                // change; that is the trade batching already made, extended by
                // one leaf kind. And a span may now straddle a text/number
                // boundary, refused as `BadSpan("across a joined boundary")` —
                // the same mechanism as between two strings, and no new
                // variant.
                let rendered: Vec<&str> = leaves
                    .iter()
                    .map(|leaf| match leaf {
                        mapping::Leaf::Text(text) => text.as_str(),
                        mapping::Leaf::Number(number) => number.as_str(),
                    })
                    .collect();
                let per_leaf = if rendered.is_empty() {
                    // A document with no leaves at all asks the detector
                    // nothing: `{}`, or one whose every field is a boolean or a
                    // null, both of which `json_leaves` skips.
                    Vec::new()
                } else {
                    let joined = mapping::Joined::of(&rendered);
                    let spans = detector.detect(joined.text(), credential).await?;
                    documents += 1;
                    total += spans.len();
                    // Read off the joined text with the spans as the detector
                    // reported them, before any rebasing — the values are the
                    // same either way and this cannot disagree with `split`
                    // about which leaf they fell in.
                    count_occurrences(joined.text(), &spans, &mut found);
                    // Bounds and ordering checked against the text the detector
                    // actually saw, where "past the end" still means something.
                    // After rebasing, an out-of-range span could land plausibly
                    // inside the wrong leaf.
                    mapping::check_spans(joined.text(), &spans)?;
                    joined.split(&spans)?
                };
                // `split` returns one entry per leaf of either kind, so this
                // and the loop below line up with `leaves` position for
                // position. Nothing downstream would notice if they did not:
                // `replace_text_leaves` counts its replacements against *text*
                // leaves, so a `per_leaf` short by trailing **numeric** entries
                // produces exactly as many replacements as there are text
                // leaves and passes clean, while the `zip`s below silently drop
                // the numbers off the end. Measured, not reasoned: with an
                // `out.pop()` in `split`, a spanned card in
                // `{"anote": "Weber", "card": 4111111111111111}` was forwarded
                // at 200 with nothing erroring.
                //
                // The two lengths cannot disagree as the code stands — `ranges`
                // is sized from `rendered`, `rendered` is a 1:1 map over
                // `leaves`, and `split` returns one entry per range — so this
                // is a guard on the next change to the join rather than on a
                // reachable input. It refuses rather than asserting because a
                // `debug_assert` compiles out of release, which is the build
                // where a leaked card matters.
                if per_leaf.len() != leaves.len() {
                    return Err(ProxyError::Mapping(MappingError::MaskCountMismatch(
                        "the detector's spans were split across a different number of leaves",
                    )));
                }
                // **A number is looked at and never replaced.** `[CREDIT_CARD_1]`
                // in a field a schema declared numeric is a type error the
                // client sees, and one the model may imitate in the next turn.
                // So the only two outcomes for a number are forwarding it and
                // refusing the request, and which one a span produces is the
                // question the rest of this comment answers.
                //
                // **Only a known NER type forwards; everything else refuses.**
                // The exemption is written as a hole in the refusal rather than
                // as the refusal itself, and the difference is the whole of it:
                // a span whose type is in `ENTITY_TYPES` and not in
                // `DETERMINISTIC_TYPES` — one of `ner.yaml`'s fourteen — leaves
                // the number alone; a span of one of `identifiers.yaml`'s eight
                // refuses; and a span of a type in **neither** half refuses too.
                //
                // *A label we know to be ungrounded on digits is not the same
                // thing as a label we know nothing about.* The argument below
                // licenses exempting the fourteen, because it is an argument
                // about what those fourteen mean. It licenses nothing about a
                // name no catalog declares: a version-skewed, misconfigured or
                // compromised detector saying "there is personal data here, of
                // a kind you do not recognise" is a thing to fail closed on.
                // The asymmetry is what gave this away — on a *string* leaf an
                // unknown type has always masked, as `REDACTED_n`, with a
                // `tracing::warn!` beside it, so the same span was honoured for
                // a string and discarded for a number.
                //
                // The reason is what the two halves of the vocabulary are
                // evidence of. A catalog hit is grounded in the value:
                // `4111111111111111` is a card because it passes Luhn, and
                // `9007199254740991` is not because it fails. An NER label on a
                // bare digit run is grounded in nothing — the model is reading
                // a shape with no context to read it against. Measured against
                // the live detector: `"9007199254740991\n\n9007199254740991"`
                // comes back `PERSON` at 0.723, and that number is
                // `Number.MAX_SAFE_INTEGER`, which sits twice in this repo's
                // own tool payload as the `maximum` of `limit` and of `offset`.
                // Refusing on a type that cannot be decided from the digits
                // themselves buys no detection and spends real requests:
                // `{"invoice_ids": [98765432109876, 98765432109877]}` is an
                // ordinary tool call.
                //
                // The evidence that cuts the other way, so that a reader can
                // weigh it rather than take this on trust. The false-positive
                // class is narrow: paired unix timestamps, millisecond
                // timestamps, 14-digit ids, other large powers of two, byte
                // offsets and `0`/`2000` all come back with nothing, and
                // *three* repeated bounds come back with nothing where two
                // fire. The 14-digit ids are the sharpest of these, because
                // they are the `invoice_ids` case above and they are clean.
                // Nor does prose reliably suppress it — the two bounds behind
                // "Maximum number of items" return nothing, but behind "The
                // maximum number of items to return." they still return
                // `PERSON`, at 0.784. So the argument here is not frequency. It
                // is that labelling `Number.MAX_SAFE_INTEGER` a person is wrong
                // however rarely it happens, and unstable in a way that makes
                // "how rarely" not a number anyone can hold this to. Someone
                // who weighs an ungrounded refusal as cheaper than a missed
                // one should widen this back to `!spans.is_empty()` and say so
                // here.
                //
                // `EMAIL` is in the eight and cannot occur in a numeric leaf.
                // It stays: the set is "what the detector decides
                // deterministically", and narrowing it to the subset someone
                // judged capable of being numeric is the hand-maintained list
                // this branch has corrected five times. For the same reason the
                // exemption is derived from the two constants rather than
                // written out as a third array of fourteen names — those two
                // are what `scripts/check_entity_types.py` holds to the
                // catalogs, and a list nothing holds to anything is the
                // failure this predicate is being corrected for.
                //
                // The gap that remains is the vocabulary's, not the predicate's:
                // there is no telephone entity in either catalog, so a phone
                // number written as a JSON number is forwarded. That is the
                // same gap it has in ordinary text, and detection-quality work
                // rather than this slice's.
                //
                // Refused here, before the masking loop rather than inside it:
                // a refusal partway through would have allocated placeholders
                // for a request that is never sent. `handle` masks into a clone
                // of the session's mapping and only commits it after the last
                // `?`, so those placeholders would in fact be discarded — but
                // this check is also strictly cheaper than reaching the same
                // answer later, and it does not depend on that being true.
                for (leaf, spans) in leaves.iter().zip(&per_leaf) {
                    let mapping::Leaf::Number(number) = leaf else {
                        continue;
                    };
                    if spans.iter().any(|span| {
                        let entity_type = span.entity_type.as_str();
                        !mapping::ENTITY_TYPES.contains(&entity_type)
                            || mapping::DETERMINISTIC_TYPES.contains(&entity_type)
                    }) {
                        return Err(ProxyError::NumericPersonalData);
                    }
                    // Past that `return`, every span on this leaf is one the
                    // exemption above lets through, and the digits under it
                    // reach the provider unchanged. The journal is told here,
                    // where the decision is made: without it the line reads
                    // `spans: 1`, `types: {"PERSON": 1}`, `redacted: 0` — which
                    // is what a *masked* value looks like — for a value that
                    // was never masked at all. The exemption is deliberate; the
                    // evidence claiming a pseudonymization that did not happen
                    // was not.
                    //
                    // **Counted per occurrence, not per value.** The same
                    // digits in two numeric leaves are two occurrences
                    // forwarded, and the same digits in a numeric leaf and a
                    // string leaf are one of each — which is the case that made
                    // this a count of occurrences rather than of findings.
                    let characters: Vec<char> = number.chars().collect();
                    for span in spans {
                        if let Some(value) = characters.get(span.start..span.end) {
                            *forwarded
                                .entry((span.entity_type.clone(), value.iter().collect()))
                                .or_default() += 1;
                        }
                    }
                }
                let mut replacements = Vec::new();
                for (leaf, spans) in leaves.iter().zip(&per_leaf) {
                    // The number's own entry is dropped: the rebuild copies it
                    // through untouched, and `replace_text_leaves` asks for a
                    // replacement per *text* leaf.
                    if let mapping::Leaf::Text(text) = leaf {
                        replacements.push(mapping.mask(text, spans)?);
                    }
                }
                let rebuilt = mapping::replace_text_leaves(&document, &replacements, *shape)?;
                write_document(&mut masked, pointer, &rebuilt, *embedded)?;
            }
        }
    }
    // **One pass, two counts, and a partition over occurrences.** Every
    // finding — one distinct (type, value) pair the detector reported anywhere
    // in this request — is named in `types`. Every *occurrence* of one is in
    // exactly one of three states: forwarded verbatim, masked under a name that
    // is not the one `types` gives it, or masked under that name. The third is
    // the ordinary case and is not counted, because `spans` counts occurrences
    // and it is what remains.
    //
    // **Why the partition is over occurrences and not over findings.** A
    // finding does not have one fate. The same value can occupy a numeric leaf,
    // where the exemption above forwards it, and a string leaf, where it is
    // masked — a `maximum` and a `description` holding the same number is the
    // ordinary shape of it, and `9007199254740991` sits twice in this repo's
    // own tool payload. Counting fates per finding made one line say the
    // provider had received the digits *instead of* the placeholder when it had
    // received both, and no third counter fixes that: a finding can be
    // forwarded and *also* masked under another type's name, so "mixed" is not
    // one state either. An occurrence has exactly one fate by construction —
    // `Joined::split` puts every span in exactly one leaf, and a leaf is either
    // masked or forwarded whole — so that is the object the counters partition.
    //
    // **Why one `named_as` decides all of a finding's masked occurrences.** The
    // mapping is keyed by value, so every masked occurrence of one value
    // carries the same placeholder whatever span found it. The fate of a masked
    // occurrence is therefore a function of (type, value) alone, and the
    // multiplication below is exact rather than an estimate.
    //
    // **Why this is asked here and not read off the mapping.**
    // `Mapping::redacted_count` counts *allocations*: `placeholder_for` returns
    // on a `by_value` hit before it looks at the type, so a value already
    // mapped — a repeat inside one request, or anything an earlier turn of the
    // session seeded — never reaches the counter. Two turns of identical
    // traffic, both sending `[REDACTED_1]`, wrote `redacted: 1` and then
    // `redacted: 0`; and a value found twice under two names in one request
    // wrote both names with `redacted: 0`, when only the first name was issued.
    // The journal describes a request, so it asks about this request's
    // findings, and it asks the mapping what the provider actually got.
    let mut types: BTreeMap<String, usize> = BTreeMap::new();
    let mut redacted = 0usize;
    let mut forwarded_count = 0usize;
    for (finding, occurrences) in &found {
        let (entity_type, value) = finding;
        *types.entry(entity_type.clone()).or_default() += 1;
        let verbatim = forwarded.get(finding).copied().unwrap_or(0);
        forwarded_count += verbatim;
        // What the exemption did not forward, the masking loop masked: both
        // maps are filled from the spans of the same detector calls, and
        // `split` gives each of those spans to exactly one leaf. The saturation
        // is unreachable for that reason; if it were ever reached it would
        // under-report `redacted` rather than panic on an evidence line, and
        // `a_line_accounts_for_every_span_it_reports` is what would notice.
        let masked_here = occurrences.saturating_sub(verbatim);
        if masked_here > 0 && !mapping.named_as(value, entity_type) {
            redacted += masked_here;
        }
    }
    if mapping.redacted_count() > 0 {
        // The count, never the name: the name is the untrusted string this
        // check exists to keep out of anything we write down. A detector and a
        // gateway that disagree about what a type is should not wait for an
        // audit to be noticed.
        tracing::warn!(
            count = mapping.redacted_count(),
            "the detector reported entity types outside this gateway's vocabulary"
        );
    }
    Ok((
        masked,
        Detected {
            texts,
            documents,
            spans: total,
            types,
            redacted,
            forwarded: forwarded_count,
        },
    ))
}

/// Which branch `handle` took. `serve` uses this to decide whether it is the
/// one that gets to record this request's outcome: a streamed response's own
/// handle — the clone `handle` hands to `restore_stream` — already claims
/// that job, and `serve` calling `completed` too would race the drop that
/// actually decides it, leaving whichever call ran last to overwrite whatever
/// the other one wrote.
enum Handled {
    Buffered(Response),
    Streamed(Response),
}

async fn handle(
    state: Arc<AppState>,
    provider: &'static dyn Provider,
    headers: HeaderMap,
    body: Value,
    record: &Record,
) -> Result<Handled, ProxyError> {
    // Attribution first, before even the shape check: a request refused for
    // its body or its session id should still say whose it was, and its
    // outcome line is the only line such a request leaves. Read once, so the
    // digest sent here and the one sent below cannot drift apart into two
    // different readings of the same header.
    let credential = crate::session::credential_of(&headers, provider);
    if let Some(credential) = credential {
        record.attribute(state.audit.digest(&[credential]), None);
    }

    // Where is the text? A shape we do not recognize is refused, not forwarded.
    let slots = provider.request_pointers(&body)?;

    // Every tool structure this request newly scans, counted twice over: how
    // many times the detector will be called, and how many characters it will
    // be given across those calls. One walk answers both, because both are
    // questions about the same text leaves.
    //
    // **Characters, not serialized bytes.** What costs the detector time is the
    // text it reads; braces, quotes and property names are structure it never
    // sees. Measured on `mapping`'s real tool payload the difference is 1.49x —
    // 10 970 serialized against 7 379 detected — so charging serialized size
    // refuses payloads a third smaller than the ceiling it claims to enforce.
    // `mapping::a_real_tool_payload_fits_the_bounds_this_gateway_ships_with`
    // pins both figures.
    //
    // Arguments count, not only results: `Write` and `Edit` carry whole files
    // in arguments, and a tool call the model produced is restored to real
    // values and echoed back in the next turn's history — text the cache has
    // never seen, because the cache holds the masked request rather than the
    // restored response. A `tool_result` is a `Text` slot, because it is a bare
    // string, and it is the largest surface here rather than the smallest: a
    // coding agent's file reads arrive in one. Counting only the documents
    // would have left results to axum's 2 MB body default.
    //
    // Ordinary prompt text is counted by neither, for the same reason: the tool
    // bounds have no business limiting how long a conversation may be.
    //
    // Both are counted here, before a single round-trip is made, rather than
    // spent down as the masking loop goes — a bound that charges each document
    // just before detecting it spends most of itself and *then* refuses, and
    // the caller waits out nearly the whole cost to be told no. `json_leaves`
    // is a pure walk with no I/O, so the price is walking each document twice.
    //
    // A numeric leaf is charged like any other, because it is joined into the
    // same detection call the strings make: its rendered digits are characters
    // the detector reads, and it occupies a place in the join that costs a
    // separator. A document of numbers and nothing else is therefore one call
    // rather than none.
    let mut tool_calls = 0usize;
    let mut tool_chars = 0usize;
    for slot in &slots {
        match slot {
            Slot::Text { tool: false, .. } => {}
            Slot::Text {
                pointer,
                tool: true,
            } => {
                tool_calls += 1;
                tool_chars += read_pointer(&body, pointer)?.chars().count();
            }
            Slot::Json {
                pointer,
                embedded,
                shape,
            } => {
                let document = read_document(&body, pointer, *embedded, provider.name())?;
                let mut leaves = 0usize;
                for leaf in mapping::json_leaves(&document, *shape)? {
                    leaves += 1;
                    tool_chars += match leaf {
                        mapping::Leaf::Text(text) => text.chars().count(),
                        mapping::Leaf::Number(rendered) => rendered.chars().count(),
                    };
                }
                // One call for the document however many leaves are in it, and
                // none at all for a document holding none — `{}`, or one whose
                // every field is a boolean or a null, which the walk skips.
                // This is the whole of what batching changed about the bounds:
                // the count moved from strings to round-trips, and the strings
                // stopped being what costs.
                if leaves > 0 {
                    tool_calls += 1;
                }
                // The separators are charged because the detector reads them,
                // and this bound is denominated in characters sent. Leaving
                // them out would also leave the join itself unbounded: a
                // million empty leaves are no characters of the caller's and
                // two million of ours.
                tool_chars += mapping::Joined::separator_chars(leaves);
            }
        }
    }
    if tool_chars > state.max_tool_chars {
        return Err(ProxyError::ToolTooLarge);
    }
    if tool_calls > state.max_tool_calls {
        return Err(ProxyError::TooManyToolCalls);
    }

    if body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        record.streaming();
    }

    // Resolved before detection: a malformed header must cost nothing, not a
    // second per 1 200 characters.
    let key = key_from(&headers, provider, state.sessions.enabled())?;

    if let (Some(credential), Some(key)) = (credential, key.as_ref()) {
        record.attribute(
            state.audit.digest(&[credential]),
            Some(state.audit.digest(&[credential, key.id().as_bytes()])),
        );
    }

    // One mapping for the whole request so a value keeps one name; seeded from
    // the conversation's table so it keeps that name across turns too.
    let (masked, mapping) = match key {
        Some(key) => {
            let claimed = state.sessions.acquire(&key)?;
            let mut guard = match claimed.guard {
                Some(guard) => guard,
                None => Arc::clone(&claimed.session.mapping).lock_owned().await,
            };
            let mut work = guard.clone();
            // On the clone, so the boundary is this request's. The session's
            // own copy never carries an issuance record to clear.
            work.begin_request();
            let (masked, found) = mask_all(
                provider.name(),
                &state.detector,
                &body,
                &slots,
                &mut work,
                credential,
            )
            .await?;
            record.detected(found);
            // Durable before anything leaves the perimeter, and before the
            // session commits: this is the last expression that can refuse the
            // request, and a request that never left must leave the session
            // exactly as it was. It costs holding the session's lock across an
            // fsync — a few milliseconds against a detector round-trip of about
            // a second, and the alternative is a caller's values sitting in the
            // store for `session_idle_secs` on behalf of a request nobody was
            // allowed to make.
            record.masked().await?;
            // After the last `?`, and on a copy until here: a refused request
            // leaves the session exactly as it was, so a client whose detector
            // blinked does not carry a hole in its numbering for the rest of
            // the conversation.
            guard.absorb(&work, state.sessions.max_values());
            (masked, work)
            // `guard` is dropped here — before the upstream call, so a stream
            // that runs for minutes holds no lock on its session.
        }
        None => {
            let mut work = Mapping::new();
            work.begin_request();
            let (masked, found) = mask_all(
                provider.name(),
                &state.detector,
                &body,
                &slots,
                &mut work,
                credential,
            )
            .await?;
            record.detected(found);
            // The same ordering with nothing to commit: the journal is still
            // durable before the upstream call, which is what it exists for.
            record.masked().await?;
            (masked, work)
        }
    };

    // Which tokens in the response this gateway may claim as its own. Both
    // arms above leave `mapping` holding this request's issuance, so it is
    // read here rather than threaded out of the match.
    //
    // Built from the request as it arrived, before anything rewrote it:
    // `masked` carries the placeholders this request just issued, and reading
    // the literals off that would put every one of them in both sets and leave
    // nothing restorable at all.
    let provenance = Provenance::new(mapping.issued(), mapping::placeholder_literals(&body));

    // Only what is masked leaves the process.
    let mut request = state.upstream.post(format!(
        "{}{}",
        state.base_for(provider),
        provider.upstream_path()
    ));
    let allowed: &[&str] = match provider.name() {
        "anthropic" => &ANTHROPIC_HEADERS,
        _ => &OPENAI_HEADERS,
    };
    for name in allowed {
        let header = HeaderName::from_static(name);
        if let Some(value) = headers.get(&header) {
            request = request.header(header, value);
        }
    }
    let response = request.json(&masked).send().await.map_err(|error| {
        // A connection that was never established carried no bytes, which
        // is the one failure here that says so for certain. Every other
        // way `send` can fail — a timeout, a reset, a truncated body —
        // may have left bytes on the wire, and those keep the claim
        // `masked` made.
        if error.is_connect() {
            record.did_not_reach_upstream();
        }
        ProxyError::Upstream(error.to_string())
    })?;

    let status = StatusCode::from_u16(response.status().as_u16())
        .map_err(|error| ProxyError::Upstream(error.to_string()))?;
    // The rate-limit headers are what let a client back off as the provider
    // asked; rebuilding the response without them silently drops that.
    let mut returned = HeaderMap::new();
    for name in RETURNED_HEADERS {
        let header = HeaderName::from_static(name);
        if let Some(value) = response.headers().get(&header) {
            returned.insert(header, value.clone());
        }
    }
    // A stream is restored as it arrives. A non-success status is not a stream,
    // whatever its content type, and keeps the buffered path below.
    if status.is_success()
        && response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            // Media types are case-insensitive, and a parameter may follow.
            .and_then(|value| value.split(';').next())
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/event-stream"))
    {
        return Ok(Handled::Streamed(crate::stream::restore_stream(
            response,
            provider,
            mapping,
            returned,
            record.clone(),
        )));
    }

    let raw = response
        .bytes()
        .await
        .map_err(|error| ProxyError::Upstream(error.to_string()))?;

    if !status.is_success() {
        // The provider's status and error envelope carry retry semantics the
        // client needs; turning a 429 into a generic 502 loses them. The body
        // may still echo what we sent, so it is restored before it goes back —
        // and an error body is not always JSON, so text is handled too.
        return Ok(Handled::Buffered(
            match serde_json::from_slice::<Value>(&raw) {
                Ok(parsed) => {
                    (status, returned, Json(mapping.restore_value(&parsed)?)).into_response()
                }
                // Reached when *this* reader will not parse the body, which is
                // not the same as the body not being one — a document nested
                // past the parser's limit or carrying an exponent it calls out
                // of range arrives here and a client reads it. `restore` alone
                // has no escaping rule and no structural retry, so a mapped
                // value carrying a `"` closed the string it landed in, here,
                // for the same reason it did in the arm above before this fix.
                // The strict door asks the question this arm cannot: it
                // substitutes as text only where it can prove nothing can be
                // closed, and refuses rather than guessing otherwise.
                Err(_) => {
                    let text = String::from_utf8_lossy(&raw);
                    (status, returned, mapping.restore_in_string_strictly(&text)?).into_response()
                }
            },
        ));
    }

    let upstream: Value =
        serde_json::from_slice(&raw).map_err(|error| ProxyError::Upstream(error.to_string()))?;

    // Restore, and refuse rather than hand a placeholder to the client.
    //
    // Every shape failure from here down is re-blamed by `as_response_error`.
    // The functions doing the reading are shared with the request path, where
    // an unreadable shape is the caller's mistake and 400 is the answer; on the
    // way back the same failure is the upstream's. A model emitting truncated
    // `arguments` is the live case — models do it — and without this the caller
    // would be told the request they sent was malformed and would go looking
    // for it there forever.
    //
    // The sweep runs on the untouched upstream body and the slots below are
    // computed from that same body, so neither pass reads the other's output
    // and no value is restored twice. Sweeping the *result* would corrupt a
    // multi-turn session: strict restoration puts back a stored value carrying
    // a literal the caller wrote in an earlier turn, and `written` describes
    // only this request, so a second pass reads that literal as a token this
    // request issued and overwrites the caller's own text with this turn's
    // value. That case is
    // `a_literal_a_stored_value_carries_survives_a_later_turn_reusing_its_number`,
    // which is what fails if this order is ever changed.
    //
    // Order matters the other way too: sweeping and stopping would make
    // `content` lenient, so an unmappable token there would be served instead
    // of refusing. The slot loop's strict result wins wherever the two overlap.
    //
    // **The sweep does not reach everywhere, and the exception is a rule
    // rather than a list.** Where restoring would change something else the
    // upstream sent, the bytes are left: an object whose restored keys would
    // collide keeps the keys and values it came with, and a string that is a
    // serialized document is left whole when the round trip that restores it
    // would not reproduce it — see `restore_sweep`, and
    // `restore_in_string_with` for why that set is argued from what JSON is
    // made of rather than enumerated. A placeholder can still reach the client
    // from inside any of them, and everything around them restores.
    // `a_colliding_key_costs_its_own_object_and_nothing_around_it` holds the
    // first to its size, having once been the whole body;
    // `a_serialized_document_with_duplicate_members_reaches_the_client_intact`
    // is the second, which exists because re-serializing that string dropped a
    // member the upstream sent.
    //
    // **Every one of them stops at the slot loop below.** They are the *sweep's*
    // answers, and the loop overwrites the sweep wherever the two address the
    // same bytes: a slot carrying any of these shapes refuses the whole response
    // rather than serving it, because leaving those bytes in a described field
    // leaves the placeholder in them. So the sentences above are about a
    // field no slot addresses, and `restore_sweep`'s doc says the same. The
    // refusal is `MappingError::Unrestorable`, and
    // `a_nested_document_that_cannot_be_restored_faithfully_refuses_the_response`
    // is what holds the line between the two answers.
    //
    // `upstream` came out of `from_slice` above, which is the parse
    // `restore_sweep` requires: its recursion is bounded by nothing else.
    //
    // **And that parse is a round trip this gateway does not guard, which is
    // the widest statement of the problem the guards below answer.** The whole
    // body is read into a `Value` here and written back by `Json(restored)` at
    // the end of this function, so every number in every buffered response goes
    // through `i64`/`u64`/`f64` — Anthropic's `input`, OpenAI's `usage`, and
    // every field nobody describes. Measured on a body with no placeholder in
    // it at all: `"total":123456789012345678901234567890` came back
    // `1.2345678901234568e+29` and `"cost":0.12345678901234567890123456789`
    // came back `0.12345678901234568`, on a 200, with nothing restored and no
    // tool traffic involved.
    //
    // **It is not closed here and the reason is scope rather than difficulty.**
    // The two guards below cover the round trips this slice *adds* — a
    // described `arguments`, and a document nested inside a restored string —
    // where the alternative was corrupting a document a client executes.
    // Covering this one means either refusing any response carrying a number
    // this reader cannot reproduce, which is a refusal on ordinary traffic that
    // no finding asks for, or not representing a response body as a `Value` at
    // all, which is a different slice with its own additivity argument to make.
    // Recorded here rather than in a commit message so the next reader of these
    // guards finds the frame above them.
    let mut restored = mapping.restore_sweep(&upstream, &provenance);
    for slot in provider
        .response_pointers(&upstream)
        .map_err(as_response_error)?
    {
        match slot {
            // `restore_in_string_strictly` rather than `restore`, and the
            // reason is the same one the `Json` arm has: this loop overwrites
            // the sweep's answer wherever the two overlap, so a text slot that
            // takes the plain call takes back the escaping the sweep had
            // already applied to the same string. A text slot is prose most of
            // the time and the two calls then agree byte for byte — but
            // `content` under `response_format: json_object` is a serialized
            // document the client parses, and a restored value carrying a `"`
            // substituted into it as text closes the string it lands in. Same
            // hazard, same door, same token policy: strict either way.
            //
            // **The streamed twin of this line does not get this protection,
            // and the reason is that it cannot.** A streamed `content` is
            // restored by `RestoreBuffer`, fragment by fragment, with plain
            // `restore` — so the `response_format: json_object` case above is
            // closed here and open there. It is not an oversight to fix by
            // routing that call through the strict door: the protection is a
            // parse of the whole string, a delta is a fragment of one, and the
            // buffer holds text back only far enough not to split a placeholder
            // (`safe_prefix_len`, which looks for `[`), so what it releases is
            // an arbitrary prefix with no relation to a document boundary. See
            // `RestoreBuffer`, which says the same from the other side.
            //
            // **What is closed there is the `arguments` half, by a different
            // mechanism.** `reject_streamed_tools` refuses `stream: true` on
            // any request carrying tool traffic, so a described `arguments` —
            // the case the whole recursion was written for — never streams at
            // all. The residue is a JSON-mode `content` on a streamed
            // completion, where a value carrying a `"` can still reach the
            // client mid-document. Two other things sit near that path and
            // neither closes it: the hold-back buffer is about placeholders and
            // not documents, and the slot blanking in `stream::handle` is what
            // routes delta text *away* from the escaping-aware `restore_value`
            // in the first place.
            Slot::Text { pointer, .. } => {
                let text = read_pointer(&upstream, &pointer)?;
                write_pointer(
                    &mut restored,
                    &pointer,
                    &mapping.restore_in_string_strictly(&text)?,
                )?;
            }
            // Restored whole rather than leaf by leaf: nothing here has to
            // agree with a detector about positions, so the walk that already
            // knows how to replace placeholders inside a value does the job.
            //
            // **`read_document` and `write_document` are themselves a
            // `parse → Value → to_string`, and it is this pair that handles
            // `arguments`.** That is one frame above anything `restore_value`
            // can see: the guards inside it are asked of the strings *within*
            // the parsed document, so a described `arguments` — the field the
            // finding was about — had its own round trip unguarded while a
            // document nested one level deeper inside it was covered. Measured
            // through the pair: an `amount` of
            // `0.12345678901234567890123456789` came back
            // `0.12345678901234568`, an integer past 64 bits came back in
            // exponent form, and a duplicate `mode` was collapsed — the loss
            // the round before this one existed to close, on the path it was
            // named for.
            Slot::Json {
                pointer, embedded, ..
            } => {
                let document = read_document(&upstream, &pointer, embedded, provider.name())
                    .map_err(as_response_error)?;
                let restored_document = mapping.restore_value(&document)?;
                // Nothing to put back means nothing to write, and the upstream's
                // own bytes are already in `restored` from the sweep. This is
                // the same rule `restore_in_string_with` applies one level down
                // and it is what keeps the guard below off ordinary traffic:
                // most `arguments` carry no placeholder at all, and refusing
                // one over a number nobody was going to rewrite would be a
                // refusal bought for nothing.
                if embedded && restored_document == document {
                    let text = read_pointer(&upstream, &pointer)?;
                    // **The comparison above is between two `Value`s, and a
                    // `Value` is what `read_document`'s parse already lost
                    // something to.** A placeholder in a member that parse
                    // collapsed — `{"name":"[PERSON_1]","name":"fixed"}` — is
                    // on neither side of it, so the skip concluded there was
                    // nothing to restore and served the upstream's own bytes,
                    // token and all, in a field the client executes. The sweep
                    // had already left that string for the same duplicate, so
                    // nothing downstream was going to catch it.
                    //
                    // So the skip is taken on the text's evidence, not the
                    // document's, and only where it is entitled to be: a
                    // faithful round trip means the parse hid nothing, and no
                    // placeholder means there was nothing for it to hide.
                    // Refusing needs both, which is what keeps this off the
                    // ordinary case the skip was added for — most `arguments`
                    // carry no token at all, and those still go back byte for
                    // byte however their numbers are spelled.
                    match mapping::round_trip_loses(&text) {
                        Some(cause) if mapping::carries_a_placeholder(&text) => {
                            return Err(MappingError::Unrestorable(cause).into());
                        }
                        _ => continue,
                    }
                }
                // `write_document` is about to re-serialize, so the round trip
                // has to be known to reproduce what `read_document` read. Only
                // `embedded` asks it: without it there is no text at this
                // pointer, the value is part of the body, and the body's own
                // round trip is a wider question this slot cannot answer — see
                // the note above `restore_sweep`'s call.
                if embedded {
                    let text = read_pointer(&upstream, &pointer)?;
                    if let Some(cause) = mapping::round_trip_loses(&text) {
                        return Err(MappingError::Unrestorable(cause).into());
                    }
                }
                write_document(&mut restored, &pointer, &restored_document, embedded)?;
            }
        }
    }
    // The same quota headers matter on a 200: a client that only learns its
    // remaining budget from errors learns it too late.
    Ok(Handled::Buffered(
        (returned, Json(restored)).into_response(),
    ))
}

async fn openai(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    serve(state, &OpenAi, "/v1/chat/completions", headers, body).await
}

async fn anthropic(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    serve(state, &Anthropic, "/v1/messages", headers, body).await
}

/// Where the request's outcome becomes a value one can record.
///
/// The record is constructed here rather than inside `handle` because a
/// `ProxyError` has no status until `into_response` runs, and a bare `?` inside
/// `handle` unwinds past any guard without saying which failure occurred. A
/// guard that dropped on `handle`'s return would have to invent both fields.
async fn serve(
    state: Arc<AppState>,
    provider: &'static dyn Provider,
    route: &'static str,
    headers: HeaderMap,
    body: Value,
) -> Response {
    let record = Record::new(Arc::clone(&state.audit), provider.name(), route);
    match handle(state, provider, headers, body, &record).await {
        // The wrapper is the only handle a buffered response ever gets, so
        // this is the whole outcome.
        Ok(Handled::Buffered(response)) => {
            record.completed(response.status().as_u16());
            response
        }
        // `restore_stream` holds its own clone of `record` and calls
        // `completed` or `stream_failed` itself once the stream actually
        // ends. Recording anything here too would race that drop: whichever
        // call happened to run last would overwrite the other's answer, and
        // a wrapper that always wins would put back the bug this exists to
        // fix — an outcome decided before the stream ever ran.
        Ok(Handled::Streamed(response)) => response,
        Err(error) => {
            record.refused(error.status().as_u16(), error.audit_class());
            error.into_response()
        }
    }
}

/// Liveness for an orchestrator: this process is up, and it is up *with* a
/// journal, since `main` opens the journal before it binds and a failure
/// there stops the process rather than starting one that proves nothing.
///
/// It deliberately reports nothing about the detector. This endpoint takes no
/// credential, so probing the detector from here would be a way to drive
/// detection without one; and a detector outage refuses individual requests
/// by design rather than making this gateway unhealthy.
async fn health() -> Response {
    (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(openai))
        .route("/v1/messages", post(anthropic))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use wiremock::matchers::path as path_matcher;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::session::{SessionKey, SESSION_HEADER};

    const SECRET: &str = "Weber";
    /// The span cap tests that are not about the cap itself pass this, so a
    /// handful of spans never accidentally brushes against it.
    const UNCAPPED: usize = usize::MAX;
    /// The production default, so tests exercise the bound callers get rather
    /// than one chosen to make a test convenient. Asserted below rather than
    /// promised here.
    const TEST_MAX_TOOL_CHARS: usize = 20_000;
    const TEST_MAX_TOOL_CALLS: usize = 40;

    #[test]
    fn the_bounds_these_tests_exercise_are_the_bounds_callers_get() {
        // The comment above has claimed this since the constants were added and
        // nothing checked it. `c0c6d3d` reverted `TEST_MAX_TOOL_CHARS` to a
        // stale value by collision and **all 380 tests stayed green**: every use
        // of it is relative — `+ 1`, `- 100`, `< `, `> ` — so the suite cannot
        // feel the difference between one number and another. The claim was
        // true only for as long as somebody kept it true by hand.
        //
        // The same shape as the headroom rule one commit earlier: stated in a
        // comment, unasserted, and quietly false. This is the assertion that
        // makes the constants self-defending instead.
        assert_eq!(
            TEST_MAX_TOOL_CHARS,
            crate::config::default_max_tool_chars(),
            "the tool character bound under test has drifted from the shipped default"
        );
        assert_eq!(
            TEST_MAX_TOOL_CALLS,
            crate::config::default_max_tool_calls(),
            "the tool call bound under test has drifted from the shipped default"
        );
    }

    fn person_span() -> Value {
        json!([{"entity_type": "PERSON", "start": 0, "end": 5, "confidence": 1.0,
                "recognizer": "ner:fake", "tier": 2, "boosted": false}])
    }

    // Direct construction, not a request through the router. Both bounds are
    // reachable the whole way through and each has a test that drives it
    // there — `a_tool_document_nested_past_the_walks_bound_is_refused` for
    // `TooDeep` and `a_tool_document_past_the_walks_node_bound_is_refused` for
    // `TooLarge`.
    //
    // This comment used to say `TooLarge` could not fire first at any sane
    // configuration, because a document of 10 000 nodes is upwards of 20 000
    // bytes and `max_tool_chars` — then 10 000 — had already refused it. Both
    // halves went stale. `handle` calls `json_leaves` to count the document
    // *before* it compares anything against either bound, so `TooLarge`
    // propagates first regardless; and the node bound counts nodes while the
    // character bound counts what the detector reads, so 10 001 empty arrays
    // reach one while charging nothing at all against the other.
    //
    // `MaskCountMismatch` fires only if this gateway's two walks disagree with
    // each other, which no input can arrange; it is reachable by mutating one
    // walk, and doing so is how the keys invariant was proved.
    //
    // These pin `status()` for all three regardless.

    #[test]
    fn a_too_deep_or_too_large_document_is_the_callers_mistake_not_the_upstreams() {
        assert_eq!(
            ProxyError::Mapping(MappingError::TooDeep).status(),
            StatusCode::BAD_REQUEST,
            "retrying will not help; the caller needs to send something smaller"
        );
        assert_eq!(
            ProxyError::Mapping(MappingError::TooLarge).status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn a_mask_count_mismatch_is_this_gateways_own_defect() {
        // Unlike TooDeep/TooLarge, this fires when json_leaves and
        // replace_text_leaves disagree with each other about a document
        // this gateway already accepted — its own fault, not the caller's.
        assert_eq!(
            ProxyError::Mapping(MappingError::MaskCountMismatch(
                "fewer masked strings than text leaves"
            ))
            .status(),
            StatusCode::BAD_GATEWAY
        );
    }

    /// A detector whose runs are complete and identified, so every answer is
    /// eligible for the cache: a second call with the same text under the
    /// same credential is served from memory and never reaches this mock. A
    /// test that must observe the detector called twice for the same text
    /// needs a different credential or text per call — or
    /// `detector_returning_expecting`, to pin the count directly rather than
    /// leaving it to whatever the cache happens to do.
    async fn detector_returning(spans: Value) -> MockServer {
        detector_returning_expecting(spans, None).await
    }

    /// As `detector_returning`, but pins how many times the mock may be
    /// called. `None` asserts nothing, matching `detector_returning` itself.
    async fn detector_returning_expecting(spans: Value, expect: Option<u64>) -> MockServer {
        let server = MockServer::start().await;
        let mock = Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "spans": spans,
                "layers_run": ["deterministic", "ner"],
                "version": "test-version"
            })));
        let mock = match expect {
            Some(count) => mock.expect(count),
            None => mock,
        };
        mock.mount(&server).await;
        server
    }

    async fn failing_detector() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        server
    }

    async fn upstream_returning(route: &str, body: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    fn test_limits() -> Limits {
        Limits {
            idle: Duration::from_secs(1800),
            max_sessions: 8,
            max_values: 8,
        }
    }

    /// A state whose journal is a fresh file, returned alongside it so a test
    /// can read what was written.
    fn state_with(
        detector: &MockServer,
        upstream: &MockServer,
        limits: Limits,
    ) -> (Arc<AppState>, tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        let audit = Arc::new(crate::audit::Audit::open(&path).expect("opens"));
        let state = Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5), 16, UNCAPPED),
            upstream: reqwest::Client::new(),
            openai_base: upstream.uri(),
            anthropic_base: upstream.uri(),
            sessions: SessionStore::new(limits),
            audit,
            max_tool_chars: TEST_MAX_TOOL_CHARS,
            max_tool_calls: TEST_MAX_TOOL_CALLS,
        });
        (state, dir, path)
    }

    /// The journal's lines, parsed. The `TempDir` must outlive the call.
    fn journal(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .expect("readable")
            .lines()
            .map(|line| serde_json::from_str(line).expect("one JSON object per line"))
            .collect()
    }

    /// The third arm of the line's partition, computed the way an auditor
    /// holding the line and nothing else has to compute it: the occurrences
    /// that reached the provider under the name `types` gives them are the ones
    /// neither counter claims. It is not a field, because it is the ordinary
    /// case and a line that reported it would be reporting `spans` twice.
    fn residual(line: &Value) -> u64 {
        let count = |name: &str| line[name].as_u64().expect("a count");
        count("spans") - count("redacted") - count("forwarded")
    }

    /// A detector that reports the given spans over any text holding
    /// `Number.MAX_SAFE_INTEGER`, and nothing over anything else.
    ///
    /// The spans are offsets into one document's *joined* leaves, so every
    /// other text in the request — a message, a tool description — has to come
    /// back empty rather than have those offsets applied to it.
    async fn detector_finding_the_max_safe_integer(spans: Value) -> MockServer {
        use wiremock::matchers::body_string_contains;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .and(body_string_contains("9007199254740991"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "spans": spans,
                "layers_run": ["deterministic", "ner"],
                "version": "test-version"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "spans": [],
                "layers_run": ["deterministic", "ner"],
                "version": "test-version"
            })))
            .mount(&server)
            .await;
        server
    }

    fn session_headers<'a>(credential: &'a str, id: &'a str) -> [(&'a str, &'a str); 2] {
        [("authorization", credential), (SESSION_HEADER, id)]
    }

    fn test_key(id: &str, credential: &str) -> SessionKey {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", credential.parse().unwrap());
        headers.insert(HeaderName::from_static(SESSION_HEADER), id.parse().unwrap());
        key_from(&headers, &OpenAi, true).unwrap().unwrap()
    }

    /// A detector that finds "Weber" and nothing else. wiremock takes the
    /// first mount that matches, so the specific rule is mounted first.
    async fn detector_finding_weber() -> MockServer {
        use wiremock::matchers::body_string_contains;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .and(body_string_contains(SECRET))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"spans": person_span(), "layers_run": ["deterministic"]}),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"spans": [], "layers_run": ["deterministic"]})),
            )
            .mount(&server)
            .await;
        server
    }

    fn state(detector: &MockServer, upstream: &MockServer) -> Arc<AppState> {
        state_with(detector, upstream, test_limits()).0
    }

    async fn call(state: Arc<AppState>, route: &str, body: Value) -> (StatusCode, String) {
        call_with_headers(state, route, body, &[]).await
    }

    async fn call_with_headers(
        state: Arc<AppState>,
        route: &str,
        body: Value,
        headers: &[(&str, &str)],
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(route)
            .header("content-type", "application/json");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let response = router(state)
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn the_upstream_never_sees_the_original() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "Hallo [PERSON_1]"}}]}),
        )
        .await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let received = &upstream.received_requests().await.unwrap()[0];
        let sent = String::from_utf8(received.body.clone()).unwrap();
        assert!(
            !sent.contains(SECRET),
            "the original reached the upstream: {sent}"
        );
        assert!(sent.contains("[PERSON_1]"));
    }

    #[tokio::test]
    async fn a_value_returned_as_its_own_type_never_reaches_the_upstream() {
        // The leak this slice exists for, at the boundary that defines it: a
        // detector returning the span's own value as its `entity_type` would
        // put that value in the placeholder's name, and the placeholder is what
        // the provider receives. `mapping.rs` asserts what `mask` returns;
        // nothing but this asserts what leaves the process.
        let detector = detector_returning(json!([
            {"entity_type": "WEBER", "start": 0, "end": 5, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
        ]))
        .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "WEBER schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let received = &upstream.received_requests().await.unwrap()[0];
        let sent = String::from_utf8(received.body.clone()).unwrap();
        assert!(
            !sent.contains("WEBER"),
            "the value rode to the provider inside the type name: {sent}"
        );
        assert!(sent.contains("[REDACTED_1]"), "not masked at all: {sent}");
    }

    #[tokio::test]
    async fn the_journal_says_a_type_it_counts_was_masked_generically() {
        // `types` is built from the detector's response, before the mapping
        // rules on it, so the two checks stay independent and the journal can
        // report a type the provider never received. What must not happen is
        // the divergence going unrecorded, leaving an auditor to reconcile a
        // name against traffic that never carried it.
        //
        // **This test used to assert `types["WEBER"] == 1`** — the name written
        // out — on the argument that `redacted` beside it made the line
        // unambiguous. It did, and it was the wrong half of the problem:
        // `WEBER` is a surname, and the check that admitted it admits
        // `MARTINA_WEBER_HAUPTSTRASSE_VIER` on the same terms. The divergence is
        // now recorded as a count under `unvalidated`, which is what an operator
        // acts on, and the name stays outside the evidence file.
        let detector = detector_returning(json!([
            {"entity_type": "WEBER", "start": 0, "end": 5, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
        ]))
        .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());

        let (status, _) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "WEBER schreibt"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let lines = journal(&path);
        assert!(
            lines[0]["types"]["WEBER"].is_null(),
            "the detector's own string is a key in the evidence: {}",
            lines[0]
        );
        assert_eq!(lines[0]["types"]["unvalidated"], 1);
        assert_eq!(
            lines[0]["redacted"], 1,
            "the line counts a type no placeholder carried and does not say so: {}",
            lines[0]
        );
    }

    #[tokio::test]
    async fn a_response_keying_arguments_by_a_placeholder_is_refused_not_forwarded() {
        // The model sees placeholders in the prose and can echo one anywhere,
        // including as a property name it invents. Restoration walks values, so
        // a placeholder in key position would travel to the client untouched
        // and its tool would be called with an argument it cannot read.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [
                {"type": "tool_use", "id": "t1", "name": "read_file",
                 "input": {"[PERSON_1]": "notes.txt"}}
            ]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "read_file",
                     "input": {"path": "Weber"}}
                ]}]
            }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "the upstream produced something we cannot serve: {returned}"
        );
        assert!(
            !returned.contains("PERSON_1"),
            "and the placeholder must not reach the client even in the refusal: {returned}"
        );
    }

    #[tokio::test]
    async fn a_document_of_numbers_alone_is_one_detector_call_and_reaches_the_upstream_intact() {
        // This test used to assert the opposite — that a numeric leaf is never
        // shown to the detector — and it was written to fail on the day that
        // stopped being true rather than let the old promise persist beside the
        // new one. This is that day, and this is what replaces it.
        //
        // The float earns its place separately from the card: `1.5` is a leaf
        // whose rendering has to survive a round trip through the detector's
        // view of it and come back a float, not the string `"1.5"`.
        // `a_number_survives_replacement_with_its_type_intact` in `mapping`
        // pins the same claim one layer down.
        let detector = detector_returning_expecting(json!([]), Some(1)).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "charge",
                     "input": {"card": 4_111_111_111_111_111i64, "ratio": 1.5}}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        let seen = &detector.received_requests().await.unwrap()[0];
        let asked: Value = serde_json::from_slice(&seen.body).expect("a JSON body");
        assert_eq!(
            asked["text"], "4111111111111111\n\n1.5",
            "both numbers went in one call, rendered as the client wrote them"
        );
        let sent = sent_to(&upstream).await;
        assert_eq!(
            sent["messages"][0]["content"][0]["input"],
            json!({"card": 4_111_111_111_111_111i64, "ratio": 1.5}),
            "and every number reached the provider exactly as the client wrote it"
        );
    }

    #[tokio::test]
    async fn a_number_carrying_personal_data_refuses_the_request() {
        // The number is the document's only leaf, so a fake detector returning
        // a fixed span cannot land it anywhere but on the number: if this
        // passes for the wrong reason it is not because the span drifted into
        // a neighbouring string.
        let detector = detector_returning(json!([
            {"entity_type": "CREDIT_CARD", "start": 0, "end": 16}
        ]))
        .await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "pay",
                     "input": {"card": 4_111_111_111_111_111u64}}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{returned}");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "and the number never left the process"
        );
        let line = &journal(&path)[0];
        assert_eq!(line["error"], "tool_numeric_personal_data");
        assert_eq!(line["status"], 400);
        assert!(
            !returned.contains("4111"),
            "the refusal exists because the value is personal data, so it must \
             not be quoted back: {returned}"
        );
    }

    #[tokio::test]
    async fn an_ner_label_on_a_number_does_not_refuse_the_request() {
        // The ruling, pinned. `PERSON` is a judgement about meaning, and on a
        // bare digit run there is no meaning to judge — measured against the
        // live detector, `"9007199254740991\n\n9007199254740991"` comes back
        // `PERSON`, and that number is `Number.MAX_SAFE_INTEGER`, which this
        // repo's own tool payload carries twice. So the request goes through,
        // and the number arrives as a number rather than as its rendering.
        //
        // The number is the document's only leaf, so a fixed span cannot land
        // anywhere but on it.
        let detector = detector_returning(json!([
            {"entity_type": "PERSON", "start": 0, "end": 16}
        ]))
        .await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "page",
                     "input": {"limit": 9_007_199_254_740_991i64}}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        let sent = sent_to(&upstream).await;
        let limit = &sent["messages"][0]["content"][0]["input"]["limit"];
        assert_eq!(
            limit,
            &json!(9_007_199_254_740_991i64),
            "an ungrounded label must not cost the client the request"
        );
        assert!(
            limit.is_number(),
            "and it arrives as a number, not a string"
        );
    }

    #[tokio::test]
    async fn an_unknown_entity_type_on_a_number_refuses_the_request() {
        // The exemption is for the fourteen NER types we know, not for every
        // type that is not one of the eight. A version-skewed, misconfigured or
        // compromised detector returning a name in neither catalog is saying
        // "personal data, of a kind you do not recognise", and on a *string*
        // leaf that has always been honoured — the value masks as
        // `REDACTED_n`. On a numeric leaf the same span was ignored and the
        // number reached the upstream verbatim (measured, at 200).
        //
        // The number is the document's only leaf, so a fixed span cannot land
        // anywhere but on it.
        let detector = detector_returning(json!([
            {"entity_type": "MADE_UP_BY_A_SKEWED_DETECTOR", "start": 0, "end": 16}
        ]))
        .await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "pay",
                     "input": {"card": 4_111_111_111_111_111u64}}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{returned}");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "and the number never left the process"
        );
        assert_eq!(journal(&path)[0]["error"], "tool_numeric_personal_data");
        assert!(
            !returned.contains("MADE_UP"),
            "the type is the untrusted string this refusal exists to keep out \
             of anything we write down: {returned}"
        );
    }

    #[tokio::test]
    async fn a_structured_value_under_a_name_keyword_reaches_the_detector() {
        // C2 end to end. Under `propertyNames`, `examples` states example
        // property *names* — strings — and the arm skipped the whole subtree
        // from the key alone, so an object there was never shown to the
        // detector and reached the upstream verbatim. Measured before the fix:
        // the detector saw `{"text":"hi"}` and nothing else, and the upstream
        // received `{"owner":"Martina Weber"}`.
        //
        // The tool carries no `description`, so the schema is the only document
        // and its only leaf is the one under test — the span cannot land
        // anywhere else.
        let detector = detector_finding_martina().await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "t", "input_schema": {
                    "propertyNames": {"examples": [{"owner": "Martina Weber"}]}
                }}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        let asked: Vec<Value> = detector
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).expect("a JSON body"))
            .collect();
        assert!(
            asked.iter().any(|body| body["text"] == "Martina Weber"),
            "the value was never shown to the detector: {asked:?}"
        );
        let sent = sent_to(&upstream).await;
        assert_eq!(
            sent["tools"][0]["input_schema"]["propertyNames"]["examples"][0]["owner"],
            "Martina [PERSON_1]",
            "and it must not reach the provider as the client wrote it"
        );
    }

    #[tokio::test]
    async fn a_structured_value_under_an_identifier_keyword_reaches_the_detector() {
        // The same shape as the test above, on the arm four lines up from the
        // one that test fixed. `required` holds an array of property names; an
        // object under it states no name anywhere, and the arm skipped it from
        // the key alone. Measured before the fix: the detector saw
        // `{"text":"hi"}` and nothing else, and the upstream received
        // `{"required":{"owner":"Martina Weber"}}`.
        let detector = detector_finding_martina().await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "t", "input_schema": {
                    "type": "object",
                    "required": {"owner": "Martina Weber"}
                }}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        let asked: Vec<Value> = detector
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).expect("a JSON body"))
            .collect();
        assert!(
            asked.iter().any(|body| body["text"] == "Martina Weber"),
            "the value was never shown to the detector: {asked:?}"
        );
        assert_eq!(
            sent_to(&upstream).await["tools"][0]["input_schema"]["required"]["owner"],
            "Martina [PERSON_1]",
            "and it must not reach the provider as the client wrote it"
        );
    }

    #[tokio::test]
    async fn a_dependency_array_holding_an_object_reaches_the_detector() {
        // The arm the audit found rather than the one Codex reported.
        // `dependencies`' array half is `dependentRequired` under its old
        // spelling — property names, every one of them — and the arm skipped
        // an array for being an array. Measured before the fix: the detector
        // saw `{"text":"hi"}` and nothing else.
        let detector = detector_finding_martina().await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "t", "input_schema": {
                    "type": "object",
                    "dependencies": {"card": [{"owner": "Martina Weber"}]}
                }}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        let asked: Vec<Value> = detector
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).expect("a JSON body"))
            .collect();
        assert!(
            asked.iter().any(|body| body["text"] == "Martina Weber"),
            "the value was never shown to the detector: {asked:?}"
        );
        assert_eq!(
            sent_to(&upstream).await["tools"][0]["input_schema"]["dependencies"]["card"][0]
                ["owner"],
            "Martina [PERSON_1]"
        );
    }

    #[tokio::test]
    async fn a_tool_call_id_on_a_non_tool_message_never_reaches_the_upstream() {
        // The allowlist carrying `tool_call_id` is selected by `role ==
        // "tool"`, and the denylist that runs for every message does not carry
        // it — so on a user message nothing asked, and the object travelled.
        // Measured at 8b6061d: 200, with `"tool_call_id":{"owner":"Martina
        // Weber"}` in the egress.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok", "role": "assistant"}}]}),
        )
        .await;
        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [
                {"role": "user", "content": "hi", "tool_call_id": {"owner": "Martina Weber"}}
            ]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "refused means the upstream is never called"
        );

        // And the message that defines the field still works, which is why the
        // refusal cannot live on the denylist that runs for every message.
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok", "role": "assistant"}}]}),
        )
        .await;
        let (status, returned) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [
                {"role": "tool", "content": "ok", "tool_call_id": "call_1"}
            ]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        assert_eq!(
            sent_to(&upstream).await["messages"][0]["tool_call_id"],
            "call_1"
        );
    }

    #[tokio::test]
    async fn a_response_tool_use_block_carrying_a_text_field_is_refused() {
        // The response half, and the half of the promise that is not about the
        // provider: this is a placeholder reaching a client that *acts on it*.
        // The `text` check ran before the `type` check, so a block carrying
        // both pushed the text slot and skipped the block — `input` was never
        // restored and the client opened `/home/[PERSON_1]/notes.txt`.
        // Measured at 8b6061d, in what the client received.
        //
        // **The answer this pins changed at 6f013b9+1, and deliberately.** The
        // ordering fix made this body a 200 with the path restored; the closed
        // field list makes it a 502, because `text` is not a field a `tool_use`
        // block defines and a field the type does not define is refused rather
        // than forwarded. The assertion that carries the promise is the second
        // one, and it held under both: no placeholder reaches the client.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "tool_use", "id": "t1", "name": "read_file",
                                "text": "ok",
                                "input": {"path": "/home/[PERSON_1]/notes.txt"}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert!(
            !body.contains("PERSON_1"),
            "a placeholder reached the client: {body}"
        );

        // And the shape Anthropic actually documents still works, which is what
        // makes the refusal above cost nothing: `id` and `name` are forwarded as
        // the model wrote them and `input` is restored.
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "tool_use", "id": "t1", "name": "read_file",
                                "input": {"path": "/home/[PERSON_1]/notes.txt"}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("/home/Weber/notes.txt"),
            "the arguments the client executes were not restored: {body}"
        );
    }

    #[tokio::test]
    async fn a_citation_carrying_a_placeholder_is_refused_rather_than_returned() {
        // The whole of this round, in what the client receives. A `text` block's
        // top-level `text` was restored and every other field of it was handed
        // over exactly as the provider wrote it — so a `citations[].cited_text`
        // carrying `[PERSON_1]` was the gateway returning its own placeholder as
        // quoted source material. Measured at 6f013b9: 200, with
        // `"cited_text":"[PERSON_1] lives here"` in the body.
        //
        // The cost of closing it is zero on traffic this gateway admitted: the
        // request-side allowlist refuses every block that can enable citations,
        // so a compliant provider answering an accepted request cannot produce
        // one. Where it does fire, the alternative was the placeholder.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "text", "text": "ok", "citations": [
                {"type": "char_location", "cited_text": "[PERSON_1] lives here",
                 "document_index": 0, "document_title": "d",
                 "start_char_index": 0, "end_char_index": 21}]}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "a shape this gateway cannot restore is the provider's fault: {body}"
        );
        assert!(
            !body.contains("PERSON_1"),
            "a placeholder reached the client: {body}"
        );
    }

    /// A detector that finds `Martina Weber` where it occurs and nothing
    /// anywhere else, so a text slot and a document slot in one request get
    /// different answers. `detector_finding_weber` cannot serve these: its span
    /// is five characters and these need thirteen.
    async fn detector_finding_martina_weber() -> MockServer {
        use wiremock::matchers::body_string_contains;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .and(body_string_contains("Martina"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "spans": [{"entity_type": "PERSON", "start": 0, "end": 13,
                           "confidence": 1.0, "recognizer": "ner:fake",
                           "tier": 2, "boosted": false}],
                "layers_run": ["deterministic"]})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"spans": [], "layers_run": ["deterministic"]})),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_literal_in_a_later_leaf_keeps_the_number_an_earlier_leaf_would_have_taken() {
        // Codex's P2, in what the client receives. `mask` reserves the text it
        // is handed, so leaf `a` was allocated `[PERSON_1]` before leaf `z`'s
        // literal was ever looked at, and `or_insert` could not then map that
        // literal to itself. Measured at `a1ccd3d`: the client got
        // `{"z": "Martina Weber"}` for a `z` it wrote as `[PERSON_1]` — a value
        // it never sent, in an argument its own tool is about to act on.
        let detector = detector_finding_martina_weber().await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "tool_use", "id": "t1", "name": "f",
                                "input": {"z": "[PERSON_1]"}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "f",
                 "input": {"a": "Martina Weber", "z": "[PERSON_1]"}}]}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("[PERSON_1]") && !body.contains("Martina Weber"),
            "the caller's own literal came back as somebody else's name: {body}"
        );
        // The other half of the same defect, and the earlier one: with the
        // literal unreserved both leaves went up as `[PERSON_1]`, so the model
        // was shown one token standing for two different strings.
        let sent =
            String::from_utf8(upstream.received_requests().await.unwrap()[0].body.clone()).unwrap();
        assert!(
            sent.contains(r#""a":"[PERSON_2]""#) && sent.contains(r#""z":"[PERSON_1]""#),
            "the detected value and the caller's literal were sent as one token: {sent}"
        );
    }

    #[tokio::test]
    async fn a_literal_in_a_later_slot_keeps_its_number_from_an_earlier_slots_value() {
        // Wider than the finding as Codex wrote it, which was about two leaves
        // of one document. `mask_all` loops over *slots* and every `Slot::Text`
        // masks, so an ordinary message can take the number a later document's
        // literal needed — no tool document is required on the side that
        // allocates. This is what makes the reservation a pass over the whole
        // request rather than a pass over one document's leaves.
        let detector = detector_finding_martina_weber().await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "tool_use", "id": "t1", "name": "f",
                                "input": {"z": "[PERSON_1]"}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [
                {"role": "user", "content": "Martina Weber"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f",
                     "input": {"z": "[PERSON_1]"}}]}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("[PERSON_1]") && !body.contains("Martina Weber"),
            "the caller's own literal came back as somebody else's name: {body}"
        );
        let sent =
            String::from_utf8(upstream.received_requests().await.unwrap()[0].body.clone()).unwrap();
        assert!(
            sent.contains(r#""content":"[PERSON_2]""#),
            "the message's name took the number the document's literal held: {sent}"
        );
    }

    #[tokio::test]
    async fn a_literal_in_a_plain_message_keeps_its_number_from_a_documents_value() {
        // The other direction of the same widening, and the one that decides
        // whether the reservation pass reads text slots at all. Here the
        // document allocates and the *message* holds the literal, so a pass
        // that walked only documents would leave `[PERSON_1]` unreserved and
        // hand the client back a name for the token it wrote. Prose is masked
        // by the same `mask`, so it collides by the same mechanism.
        let detector = detector_finding_martina_weber().await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "tool_use", "id": "t1", "name": "f",
                                "input": {"z": "[PERSON_1]"}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f",
                     "input": {"a": "Martina Weber"}}]},
                {"role": "user", "content": "[PERSON_1]"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("[PERSON_1]") && !body.contains("Martina Weber"),
            "the caller's own literal came back as somebody else's name: {body}"
        );
        let sent =
            String::from_utf8(upstream.received_requests().await.unwrap()[0].body.clone()).unwrap();
        assert!(
            sent.contains(r#""a":"[PERSON_2]""#),
            "the document's name took the number the message's literal held: {sent}"
        );
    }

    #[tokio::test]
    async fn a_literal_in_a_position_nothing_masks_still_keeps_its_number() {
        // Wider again, and the reason the reservation reads a document as text
        // rather than walking its leaves. `required` names properties, so
        // `json_leaves` skips it and masking must: rewriting it would leave the
        // provider a schema requiring a property that does not exist. Skipped
        // by masking is not the same as free to reissue — the identifier is
        // forwarded verbatim, so the model sees `[PERSON_1]` twice, once as the
        // caller wrote it and once standing for a name. Measured with a leaf
        // walk here: `Martina Weber` went up as `[PERSON_1]` and came back as
        // the value of a tool argument the caller had written as its own token.
        // A placeholder-shaped object *key* is the same case and the same fix.
        let detector = detector_finding_martina_weber().await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "tool_use", "id": "t1", "name": "f",
                                "input": {"z": "[PERSON_1]"}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude",
                   "messages": [{"role": "user", "content": "Martina Weber"}],
                   "tools": [{"name": "f", "description": "d", "input_schema":
                       {"type": "object", "required": ["[PERSON_1]"],
                        "properties": {}}}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("[PERSON_1]") && !body.contains("Martina Weber"),
            "the caller's own literal came back as somebody else's name: {body}"
        );
        let sent =
            String::from_utf8(upstream.received_requests().await.unwrap()[0].body.clone()).unwrap();
        assert!(
            sent.contains(r#""content":"[PERSON_2]""#)
                && sent.contains(r#""required":["[PERSON_1]"]"#),
            "the identifier was masked, or its number was reissued: {sent}"
        );
    }

    #[tokio::test]
    async fn a_tool_argument_key_naming_no_token_of_ours_is_served() {
        // Codex's P2, in what the client receives. `is_placeholder` trimmed
        // every bracket, so a legitimate `[[PERSON_1]]` — a shape a templating
        // client can plausibly produce — was judged a token this gateway had
        // issued and the whole response was refused (measured at `6a06391`:
        // 502, "upstream response uses a placeholder as a property name").
        // Nothing of ours is in it, so there is nothing to refuse.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "tool_use", "id": "t1", "name": "fill",
                                "input": {"[[ROW_1]]": "Hallo [PERSON_1]"}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("[[ROW_1]]"), "the key was not served: {body}");
        assert!(
            body.contains("Hallo Weber"),
            "the value under it was not restored: {body}"
        );
    }

    #[tokio::test]
    async fn a_tool_argument_key_carrying_our_token_is_still_refused() {
        // The trap the fix above had to avoid. The same key shape, in a session
        // that issued `[PERSON_1]` for a real name, carries that token inside
        // it — and a key is never restored, so serving it hands the client this
        // gateway's own placeholder.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "tool_use", "id": "t1", "name": "fill",
                                "input": {"[[PERSON_1]]": "x"}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert!(
            !body.contains("PERSON_1"),
            "a placeholder reached the client: {body}"
        );
    }

    #[tokio::test]
    async fn a_server_executed_tool_is_refused_before_the_upstream_is_called() {
        // The refusal direction, measured on what it costs. A bare server tool
        // — `{"name": "t", "type": "code_execution_20250522"}` — passed the
        // definition gate at `6a06391`, and Anthropic answers such a request
        // with `server_tool_use` and a result block the response path does not
        // describe. So the request was forwarded, the model ran, the tokens
        // were spent, and the caller received a 502 (measured: one upstream
        // call, then `upstream response is not in the expected anthropic
        // shape`).
        //
        // The upstream mock answers 200 with the documented server-tool shape,
        // so the assertion that matters is the one about `received_requests`:
        // the refusal fires *before* the call, not after it.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [
                {"type": "server_tool_use", "id": "srvtoolu_1", "name": "code_execution",
                 "input": {"code": "print(1)"}},
                {"type": "bash_code_execution_tool_result", "tool_use_id": "srvtoolu_1",
                 "content": {"type": "bash_code_execution_result", "stdout": "1",
                             "stderr": "", "return_code": 0}}]}),
        )
        .await;
        let state = state(&detector, &upstream);
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({"model": "claude",
                   "messages": [{"role": "user", "content": "Weber schreibt"}],
                   "tools": [{"name": "code_execution", "type": "code_execution_20250522"}]}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a tool this gateway cannot serve the answer to is the caller's to \
             drop, and 502 would blame the upstream for it: {body}"
        );
        assert!(
            upstream
                .received_requests()
                .await
                .expect("the mock records requests")
                .is_empty(),
            "the refusal cost the caller a turn: {body}"
        );
    }

    #[tokio::test]
    async fn a_client_executed_tool_still_reaches_the_upstream() {
        // The other half, and the one the ledger cares about: `bash_*` and
        // `text_editor_*` are answered with an ordinary `tool_use` the caller
        // runs, so nothing new arrives on the response path and the
        // coding-agent category is unaffected. Without this the refusal above
        // could be widened to everything and no test would notice.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "tool_use", "id": "t1", "name": "bash",
                                "input": {"command": "grep -r Weber ."}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude",
                   "messages": [{"role": "user", "content": "Weber schreibt"}],
                   "tools": [{"name": "bash", "type": "bash_20250124"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("grep -r Weber ."),
            "the arguments were not restored: {body}"
        );
    }

    #[tokio::test]
    async fn the_null_citations_every_text_block_carries_is_served() {
        // The other half of the same list, and the reason `citations` is *on*
        // it rather than left off. Anthropic documents `citations` as required
        // and nullable on a response `text` block, so `"citations": null` rides
        // on every text block of every well-formed response. A closed list that
        // omitted the key would refuse all of them — the round's fix would have
        // been a 502 on ordinary traffic, which is the opposite of the promise.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "text", "text": "Hallo [PERSON_1]", "citations": null}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("Hallo Weber"), "not restored: {body}");
    }

    #[tokio::test]
    async fn a_refusal_and_a_citation_title_reach_the_client_restored() {
        // Issue #31's two fields, asserted on what the CLIENT received.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {
                "role": "assistant",
                "content": "ok",
                "refusal": "I cannot help with [PERSON_1]",
                "annotations": [{"url_citation": {"title": "[PERSON_1] page"}}]
            }}]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !body.contains("[PERSON_1]"),
            "a placeholder reached the client: {body}"
        );
    }

    #[tokio::test]
    async fn a_colliding_key_costs_its_own_object_and_nothing_around_it() {
        // The collision fallback used to be an off switch for the whole body:
        // `restore_document` returned `None` from wherever it found the
        // collision and the `?`s carried it to the top, so `restore_sweep`
        // handed back the untouched response. An object with nothing to do
        // with `refusal` put #31's exact symptom back, on a 200.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {
                "role": "assistant",
                "content": "ok",
                "refusal": "I cannot help with [PERSON_1]",
                // `[PERSON_1]` restores to `Weber`, which is already a key
                // here. Restoring it would drop one of the two fields, so this
                // object keeps the keys it came with — and only this object.
                "meta": {"[PERSON_1]": "a", "Weber": "b"}
            }}]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let served: Value = serde_json::from_str(&body).expect("a JSON body");
        let message = &served["choices"][0]["message"];
        assert_eq!(
            message["refusal"], "I cannot help with Weber",
            "one object's collision turned the sweep off for the whole body: {body}"
        );
        assert_eq!(
            message["meta"],
            json!({"[PERSON_1]": "a", "Weber": "b"}),
            "the colliding object lost a field or was restored anyway: {body}"
        );
    }

    #[tokio::test]
    async fn a_serialized_document_with_duplicate_members_reaches_the_client_intact() {
        // The structural path parses the string it is restoring into, and a
        // parse collapses two members of the same name before anything this
        // gateway wrote gets to look at them. Re-serializing then hands the
        // client a document with one `mode` where the upstream sent two — a
        // payload forwarded byte for byte before the sweep existed, and a
        // parser differential for any client whose reader keeps the *first*
        // member rather than the last.
        //
        // `Martina "Weber"` carries a quote, so the restored value needs
        // escaping and the structural path is the one taken. With a value
        // needing none the textual path answers and the bytes survive anyway.
        let detector = detector_returning(json!([
            {"entity_type": "PERSON", "start": 0, "end": 15, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false}
        ]))
        .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {
                "role": "assistant",
                "content": "ok",
                "refusal": "{\"mode\":\"safe\",\"mode\":\"admin\",\"name\":\"[PERSON_1]\"}"
            }}]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Martina \"Weber\""}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let served: Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(
            served["choices"][0]["message"]["refusal"],
            json!("{\"mode\":\"safe\",\"mode\":\"admin\",\"name\":\"[PERSON_1]\"}"),
            "the duplicate member was collapsed on the way through: {body}"
        );
    }

    #[tokio::test]
    async fn a_token_nobody_issued_in_an_undescribed_field_is_served_not_refused() {
        // Additivity, tested rather than asserted. "The suite stayed green"
        // only covers what the suite already tests.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {
                "role": "assistant",
                "content": "ok",
                "refusal": "invented [PERSON_9]"
            }}]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let served: Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(
            served["choices"][0]["message"]["refusal"],
            "invented [PERSON_9]"
        );
    }

    #[tokio::test]
    async fn both_providers_treat_an_undescribed_response_field_the_same_way() {
        // The asymmetry is what made this bug: one provider was given a
        // treatment the other was not, and nothing compared them. Every other
        // test here names a field and a provider, so each catches its own
        // instance; this one catches the class.
        //
        // One body, read twice. It carries both envelopes — OpenAI reads
        // `choices` and Anthropic reads the top-level `content`, and neither
        // looks at the other's — so the route is the only variable between the
        // two calls, and a difference in what the client receives can only be
        // a difference in how the provider was treated.
        //
        // `diagnostics` is described by neither. That is the whole shape of the
        // hazard: not a field somebody forgot, but the field a provider adds
        // tomorrow, which is how #31 arrived.
        let detector = detector_returning(person_span()).await;
        let upstream = MockServer::start().await;
        let response = json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}],
            "content": [{"type": "text", "text": "ok", "citations": null}],
            "diagnostics": {"note": "drafted for [PERSON_1]"}
        });
        for route in ["/v1/chat/completions", "/v1/messages"] {
            Mock::given(method("POST"))
                .and(path(route))
                .respond_with(ResponseTemplate::new(200).set_body_json(response.clone()))
                .mount(&upstream)
                .await;
        }
        let state = state(&detector, &upstream);

        let (openai_status, openai_body) = call(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;
        let (anthropic_status, anthropic_body) = call(
            Arc::clone(&state),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;

        assert_eq!(openai_status, StatusCode::OK, "{openai_body}");
        assert_eq!(anthropic_status, StatusCode::OK, "{anthropic_body}");
        let openai: Value = serde_json::from_str(&openai_body).expect("a JSON body");
        let anthropic: Value = serde_json::from_str(&anthropic_body).expect("a JSON body");
        assert_eq!(
            openai["diagnostics"], anthropic["diagnostics"],
            "one provider was given a treatment the other was not: \
             {openai_body} against {anthropic_body}"
        );
        // Asserted as well as compared: two providers agreeing that a
        // placeholder goes to the client is the bug, not the fix.
        assert_eq!(
            openai["diagnostics"]["note"],
            format!("drafted for {SECRET}"),
            "the undescribed field was not restored for either provider: {openai_body}"
        );
    }

    #[tokio::test]
    async fn a_streamed_block_restores_the_fields_no_slot_addresses() {
        // The streamed path never had the gap the buffered one had, and this
        // pins the reason rather than the absence. `stream::handle` restores
        // the whole event and *then* rewrites the slots, so an undescribed
        // field is restored like any other string: the `cited_text` that
        // reaches the client here is `Weber`, not `[PERSON_1]`.
        //
        // `proxy::handle` used to start from the upstream body and write only
        // the slots, which is why the same shape reached a buffered client
        // verbatim. It now sweeps first, so the two paths agree on this case.
        // They are not the same rule: the buffered sweep restores only what
        // this request issued and the caller did not write, *except where
        // restoring it would change something else the upstream sent* — an
        // object whose restored keys would collide, or a serialized document
        // the round trip that restores it would not reproduce — which is
        // narrower than what a streamed event gets on every count. The clause
        // belongs here like it belongs everywhere else the promise is written —
        // a stream restores its event whole and has no exception, so stating
        // the buffered rule
        // without it understates the distance between the two paths this
        // comment exists to draw.
        //
        // **The reason has changed and the conclusion has not, which is why it
        // is restated rather than left.** This used to read "since neither
        // `restore_document` nor `restore_in_string` is on its path", and both
        // halves are now false: the escaping rule and the recursion moved into
        // `restore_in_string_with`, which a streamed event *does* enter, via
        // the `restore_value` that restores everything in it the slots do not
        // address. What keeps the exceptions off this path is the token
        // policy rather than the absence of the walk — `restore_value` is
        // strict, so a document it cannot re-serialize faithfully raises
        // `MappingError::Unrestorable` and the stream ends, exactly as an
        // unmappable token already ends it. No exception, for a different
        // reason than before — and the same answer however many causes the
        // round trip turns out to have, which is the point of routing them all
        // through one rule.
        //
        // So there is nothing to refuse here, which is just as well: a stream
        // cannot refuse after its first bytes have gone out — the position
        // `reject_streamed_tools` already records.
        let detector = detector_returning(person_span()).await;
        let upstream = MockServer::start().await;
        let body = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\
             \"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\",\
             \"citations\":[{\"type\":\"char_location\",\
             \"cited_text\":\"[PERSON_1] lives here\"}]}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
             \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&upstream)
            .await;
        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "stream": true,
                   "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            served.contains(&format!("{SECRET} lives here")),
            "the citation the slot path does not address was not restored: {served}"
        );
        assert!(!served.contains("PERSON_1"), "placeholder served: {served}");
    }

    /// A detector that finds `Weber` inside `Martina Weber` at the offsets it
    /// sits at when that string is a document's only leaf, and nothing in any
    /// other text. Separate from `detector_finding_weber`, whose span is fixed
    /// at 0..5 and would land on `Marti`.
    async fn detector_finding_martina() -> MockServer {
        use wiremock::matchers::body_string_contains;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .and(body_string_contains("Martina Weber"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "spans": [{"entity_type": "PERSON", "start": 8, "end": 13,
                           "confidence": 1.0, "recognizer": "ner:fake",
                           "tier": 2, "boosted": false}],
                "layers_run": ["ner"]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"spans": [], "layers_run": ["ner"]})),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_deterministic_span_refuses_a_number_an_ner_span_shares() {
        // "Any deterministic span", not "the first span". Both land on the one
        // numeric leaf and the NER label is the one the detector reports
        // first; a predicate that reads only `spans[0]` forwards a card here.
        //
        // The two spans are disjoint because `check_spans` refuses overlapping
        // ones, so they cannot both cover the whole leaf. The digits they
        // divide are still one leaf, which is what this pins.
        let detector = detector_returning(json!([
            {"entity_type": "PERSON", "start": 0, "end": 4},
            {"entity_type": "CREDIT_CARD", "start": 4, "end": 16}
        ]))
        .await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "pay",
                     "input": {"card": 4_111_111_111_111_111u64}}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{returned}");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "and the card never left the process"
        );
        assert_eq!(journal(&path)[0]["error"], "tool_numeric_personal_data");
    }

    #[tokio::test]
    async fn a_number_the_detector_passes_is_forwarded_unchanged() {
        // The other half of the branch. Refusing every numeric leaf would
        // satisfy the test above and break every schema bound in real traffic,
        // and rendering the number back as the string the detector was shown
        // would break the schema that declared it numeric.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "pay",
                     "input": {"card": 4_111_111_111_111_111u64}}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        let sent = sent_to(&upstream).await;
        let card = &sent["messages"][0]["content"][0]["input"]["card"];
        assert_eq!(
            card,
            &json!(4_111_111_111_111_111u64),
            "the number must reach the provider as the number the client wrote"
        );
        assert!(card.is_number(), "and as a number, not as its rendering");
    }

    #[tokio::test]
    async fn a_number_beside_text_does_not_move_the_texts_spans() {
        // Both leaves go into one detection call, so the text no longer starts
        // at zero. The offsets here are the detector's view of the joined text
        // — `card` sorts before `note`, so the sixteen digits come first and
        // `Weber` begins after the separator, at 18. An implementation that
        // joins the number in without extending the split correspondence
        // rebases this span against the wrong leaf, or refuses it as out of
        // range.
        let detector = detector_returning(json!([
            {"entity_type": "PERSON", "start": 18, "end": 23}
        ]))
        .await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "pay",
                     "input": {"card": 4_111_111_111_111_111u64, "note": "Weber"}}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        let sent = sent_to(&upstream).await;
        let input = &sent["messages"][0]["content"][0]["input"];
        assert_eq!(
            input["note"], "[PERSON_1]",
            "the span fell inside the text leaf and had to mask all of it: {input}"
        );
        assert_eq!(
            input["card"],
            json!(4_111_111_111_111_111u64),
            "and the number beside it is untouched: {input}"
        );
    }

    #[tokio::test]
    async fn a_tool_call_is_masked_going_up_and_restored_coming_back() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [
                {"type": "tool_use", "id": "t1", "name": "read_file",
                 "input": {"path": "[PERSON_1]"}}
            ]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let body = json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "read_file",
                 "input": {"path": "Weber"}}
            ]}]
        });
        let (status, returned) = call(state, "/v1/messages", body).await;
        assert_eq!(status, StatusCode::OK);
        let returned: Value = serde_json::from_str(&returned).expect("a JSON body");
        assert_eq!(
            returned["content"][0]["input"]["path"], "Weber",
            "the client executes this, so it has to be the real value"
        );
        assert_eq!(
            returned["content"][0]["name"], "read_file",
            "the tool name is dispatch and is never touched"
        );
        let sent = String::from_utf8(upstream.received_requests().await.unwrap()[0].body.clone())
            .expect("utf-8");
        assert!(
            !sent.contains(SECRET),
            "the original reached the upstream: {sent}"
        );
        assert!(
            sent.contains("read_file"),
            "the tool name must survive masking: {sent}"
        );
    }

    /// The upstream's view of a request, parsed. What actually left the
    /// process, which is the only thing the masking invariants are about.
    async fn sent_to(upstream: &MockServer) -> Value {
        let received = upstream.received_requests().await.expect("a request");
        serde_json::from_slice(&received[0].body).expect("a JSON body")
    }

    #[tokio::test]
    async fn a_tool_arguments_key_is_never_masked_even_when_it_looks_like_a_value() {
        // Key and value are the same string, so nothing but the rule itself
        // can tell them apart — a detector cannot, and neither can a walk that
        // yields both. The key is where the tool reads its argument from:
        // masked, the call arrives without the argument at all, and the client
        // has no way to learn why.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "read_file",
                     "input": {"Weber": "Weber"}}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let sent = sent_to(&upstream).await;
        let input = &sent["messages"][0]["content"][0]["input"];
        assert_eq!(
            input["Weber"], "[PERSON_1]",
            "the value must be masked: {input}"
        );
        assert!(
            input.get("Weber").is_some(),
            "the key must reach the upstream verbatim: {input}"
        );
    }

    #[tokio::test]
    async fn an_openai_tool_call_is_masked_going_up_and_restored_coming_back() {
        // The whole of OpenAI's difference from Anthropic in one body: the
        // arguments are a *string* holding a document, so the masker has to
        // parse it, mask its leaves and write the string back — and the
        // restoration has to undo exactly that, because the client parses the
        // string and executes what it finds.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "t1", "type": "function", "function": {
                    "name": "read_file", "arguments": "{\"path\":\"/home/[PERSON_1]\"}"}}]
            }}]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/chat/completions",
            json!({
                "model": "gpt",
                "tools": [{"type": "function", "function": {
                    "name": "read_file",
                    "description": "Weber wrote this file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
                }}],
                "messages": [
                    {"role": "assistant", "content": null, "tool_calls": [{
                        "id": "t1", "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"who\":\"Weber\"}"}
                    }]},
                    {"role": "tool", "tool_call_id": "t1", "content": "Weber"}
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");

        let sent = sent_to(&upstream).await;
        let arguments = sent["messages"][0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments reach the upstream as a string, not as a document");
        assert_eq!(
            arguments, r#"{"who":"[PERSON_1]"}"#,
            "the document inside the string is masked and the string is still a string"
        );
        assert_eq!(
            sent["tools"][0]["function"]["description"], "[PERSON_1] wrote this file",
            "a definition's prose is text like any other: {sent}"
        );
        assert_eq!(
            sent["messages"][1]["content"], "[PERSON_1]",
            "a tool message's content is a result and is masked: {sent}"
        );
        assert_eq!(
            sent["messages"][1]["tool_call_id"], "t1",
            "the id pairs the result with its call and is never masked: {sent}"
        );
        assert_eq!(
            sent["messages"][0]["tool_calls"][0]["function"]["name"], "read_file",
            "the function name is dispatch: {sent}"
        );

        let returned: Value = serde_json::from_str(&returned).expect("a JSON body");
        assert_eq!(
            returned["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"/home/Weber"}"#,
            "the client executes this, so it has to be the real value"
        );
    }

    /// A detector that finds the whole of `text` wherever it appears, so a test
    /// can choose a value whose characters matter — one carrying a `"`, which
    /// is what makes restoration into JSON *text* unsafe.
    ///
    /// Such a value is not exotic: the detector reads third-party content, and
    /// a name, an address or a quoted line in it can carry a quote. The caller
    /// never has to have written it.
    async fn detector_finding_all_of(text: &str) -> MockServer {
        use wiremock::matchers::body_string_contains;
        let server = MockServer::start().await;
        let end = text.chars().count();
        // The body carries `text` JSON-escaped, so the mock matches on a
        // fragment that survives escaping rather than on the value itself.
        let fragment: String = text.chars().take_while(|c| *c != '"').collect();
        Mock::given(method("POST"))
            .and(path("/detect"))
            .and(body_string_contains(fragment))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "spans": [{"entity_type": "PERSON", "start": 0, "end": end,
                           "confidence": 1.0, "recognizer": "ner:fake",
                           "tier": 2, "boosted": false}],
                "layers_run": ["ner"]})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"spans": [], "layers_run": ["ner"]})),
            )
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_serialized_document_inside_arguments_is_restored_without_injecting_into_it() {
        // The described path's half of the escaping rule. `arguments` is a
        // string holding a document, so the slot loop parses it and restores
        // its leaves — and a leaf that is *itself* a serialized document was
        // restored as plain text, straight into JSON the client then parses
        // and executes. A value carrying a quote closes the string it lands
        // in, and everything after the quote becomes structure.
        //
        // The sweep already got this right and the slot loop overwrote its
        // answer, which is why the bug survived the sweep landing.
        let payload = r#"x","admin":true,"pad":""#;
        let detector = detector_finding_all_of(payload).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f",
                 "arguments": r#"{"nested":"{\"name\":\"[PERSON_1]\"}"}"#}}]}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": payload}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let returned: Value = serde_json::from_str(&body).expect("a JSON body");
        let arguments = returned["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments reach the client as a string");
        let arguments: Value =
            serde_json::from_str(arguments).expect("the arguments the client executes still parse");
        let nested = arguments["nested"]
            .as_str()
            .expect("the nested document is still a string");
        let nested: Value = serde_json::from_str(nested)
            .expect("the nested document the client parses is still JSON");
        assert_eq!(
            nested["name"], payload,
            "the value belongs in the leaf it was substituted into: {nested}"
        );
        assert!(
            nested.get("admin").is_none(),
            "restoration wrote a field the upstream never sent: {nested}"
        );
    }

    #[tokio::test]
    async fn a_serialized_document_in_content_is_restored_without_injecting_into_it() {
        // The same overwrite, one slot kind over. `content` is a text slot, so
        // the loop used to restore it with plain `restore` — and under
        // `response_format: json_object` a text slot is a document the client
        // parses. The sweep had already restored it structurally; the loop
        // overwrote that with the textual answer, exactly as it did for
        // `arguments`.
        let payload = r#"x","admin":true,"pad":""#;
        let detector = detector_finding_all_of(payload).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant",
                 "content": r#"{"name":"[PERSON_1]"}"#}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": payload}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let returned: Value = serde_json::from_str(&body).expect("a JSON body");
        let content = returned["choices"][0]["message"]["content"]
            .as_str()
            .expect("content reaches the client as a string");
        let content: Value =
            serde_json::from_str(content).expect("the document the client parses is still JSON");
        assert_eq!(
            content["name"], payload,
            "the value belongs in the leaf it was substituted into: {content}"
        );
        assert!(
            content.get("admin").is_none(),
            "restoration wrote a field the upstream never sent: {content}"
        );
    }

    #[tokio::test]
    async fn a_nested_document_that_cannot_be_restored_faithfully_refuses_the_response() {
        // The other half of the described path's answer, and the only place
        // this fix can make something fail that used to succeed.
        //
        // The nested document carries two members of the same name, so the
        // parse the escaping rule needs has already lost one before this
        // gateway sees it. On the sweep that costs restoration and no bytes:
        // `a_serialized_document_with_duplicate_members_reaches_the_client_intact`
        // is that case, on `refusal`, and it still passes. Here the same
        // document sits inside `arguments`, where leaving the bytes leaves the
        // placeholder in a field the client dispatches on, and substituting
        // anyway drops a member from a document it executes. So the response is
        // refused, which is what this path already does with a token it cannot
        // map.
        //
        // `Martina "Weber"` carries the quote that makes the restored value
        // need escaping. Without it the textual path answers, the bytes survive
        // and nothing refuses — which is why this is unreachable on the
        // overwhelming majority of traffic.
        let detector = detector_returning(json!([
            {"entity_type": "PERSON", "start": 0, "end": 15, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false}
        ]))
        .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": "f",
             "arguments":
                 r#"{"nested":"{\"mode\":\"safe\",\"mode\":\"admin\",\"name\":\"[PERSON_1]\"}"}"#
            }}]}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Martina \"Weber\""}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert!(
            !body.contains("[PERSON_1]"),
            "the refusal handed back the placeholder it refused over: {body}"
        );
        assert!(
            !body.contains("Weber"),
            "the refusal named the value it was protecting: {body}"
        );
        let lines = journal(&path);
        assert_eq!(lines.len(), 2, "the request was masked, then refused");
        assert_eq!(lines[1]["result"], "refused");
        assert_eq!(
            lines[1]["error"], "mapping_lossy_document",
            "the journal has to name this failure as its own class: {:?}",
            lines[1]
        );
    }

    #[tokio::test]
    async fn an_arguments_document_the_round_trip_would_change_refuses_the_response() {
        // The path the finding was actually about, which the guards missed for
        // a round. `arguments` is read by `read_document` and written by
        // `write_document`, and that pair is a `parse -> Value -> to_string` of
        // its own, one frame above the strings `restore_value` walks. So the
        // document *nested inside* `arguments` was covered and `arguments`
        // itself was not.
        //
        // **The restored value here carries no quote, and that is the point.**
        // The nested guard only runs once something needs escaping; this round
        // trip happens whenever there is anything to restore at all, so the
        // case is wider than the one it was hiding behind.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "tool_calls": [
            {"id": "c1", "type": "function", "function": {"name": "f",
             "arguments":
                 r#"{"amount":0.12345678901234567890123456789,"name":"[PERSON_1]"}"#
            }}]}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert!(
            !body.contains("0.12345678901234568"),
            "the amount was rounded on the way to the client: {body}"
        );
        let lines = journal(&path);
        assert_eq!(lines[1]["error"], "mapping_lossy_document");
    }

    #[tokio::test]
    async fn an_arguments_document_hiding_its_placeholder_in_a_duplicate_refuses() {
        // The skip above compares two `Value`s, and the parse that produced
        // them keeps the last of two members with the same name. So the only
        // placeholder in this document was on neither side of that comparison:
        // the skip read "nothing to restore", served the upstream's own bytes,
        // and `[PERSON_1]` went to a client that executes them. Measured at
        // 200 OK before the fix, with the token in the served `arguments`.
        //
        // The sweep had already left the same string alone for the same
        // duplicate, so nothing downstream was going to catch it.
        let payload = r#"x","admin":true,"pad":""#;
        let arguments = r#"{"name":"[PERSON_1]","name":"fixed"}"#;
        let detector = detector_finding_all_of(payload).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f",
                 "arguments": arguments}}]}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": payload}]}),
        )
        .await;

        assert!(
            !body.contains("PERSON_1"),
            "a placeholder reached the client from a described field: {body}"
        );
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "a document whose placeholder the parse hid was served anyway: {body}"
        );
    }

    #[tokio::test]
    async fn an_arguments_document_with_duplicates_and_no_placeholder_keeps_its_own_bytes() {
        // The other half, and the reason the refusal above asks two questions
        // rather than one. Duplicate members are a loss this reader cannot
        // undo, but a document with nothing of ours in it has nothing to
        // restore and nothing to refuse over — it is forwarded exactly as the
        // upstream wrote it, both members intact, as it was before any of this
        // existed.
        let arguments = r#"{"mode":"safe","mode":"admin"}"#;
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f",
                 "arguments": arguments}}]}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let served: Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(
            served["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            json!(arguments),
            "a document nobody was restoring was refused or rewritten: {body}"
        );
    }

    #[tokio::test]
    async fn an_arguments_document_with_nothing_to_restore_keeps_its_own_bytes() {
        // Additivity for the guard above, and a repair to what the pair did
        // before it. `read_document` and `write_document` re-serialized
        // `arguments` on every response that carried one, whether or not there
        // was anything to put back — so a number too precise for a double was
        // rounded on the way through with no placeholder in sight, and adding a
        // guard without this would have turned that silent rounding into a 502
        // bought for nothing.
        //
        // Nothing to restore means nothing to write, and the upstream's own
        // bytes are already in the body from the sweep.
        let arguments =
            r#"{"amount":0.12345678901234567890123456789,"id":123456789012345678901234567890}"#;
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f",
                 "arguments": arguments}}]}}]}),
        )
        .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let served: Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(
            served["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            json!(arguments),
            "a document nobody was restoring came back rewritten: {body}"
        );
    }

    #[tokio::test]
    async fn an_error_body_this_reader_rejects_is_not_injected_into() {
        // P1-B at the fourth fallback. An error envelope that this reader will
        // not parse — `1e999` is valid JSON and every client reads it — took
        // the text arm, where `restore` substitutes with no escaping rule and
        // no structural retry. A mapped value carrying a `"` closed the string
        // it landed in, in a body the client parses for its retry semantics.
        let payload = r#"x","admin":true,"pad":""#;
        let detector = detector_finding_all_of(payload).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_raw(
                r#"{"limit":1e999,"error":"[PERSON_1]"}"#.as_bytes().to_vec(),
                "application/json",
            ))
            .mount(&upstream)
            .await;
        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": payload}]}),
        )
        .await;

        assert!(
            !body.contains("\"admin\""),
            "an error body was injected into: {body}"
        );
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "the upstream's status is worth keeping, but not at the price of \
             serving a body this gateway corrupted: {body}"
        );
    }

    #[tokio::test]
    async fn openai_dispatch_survives_even_when_it_looks_like_a_value() {
        // Every identifier here carries the detector's own trigger word, so a
        // slot describing any of them would visibly rewrite it. That is the
        // whole point: with ordinary ids like `t1` the detector finds nothing,
        // masking one is a no-op, and an assertion that the id survived passes
        // whether or not the id is described. This one cannot.
        //
        // What each of them is: `tool_call_id` pairs a result with the call it
        // answers, the call's `id` is what that pairs against, and both `name`s
        // say which function to run. Masked, the request still parses, the
        // provider still answers, and the client executes a call addressed to
        // nothing — a break with no error attached to it anywhere.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({
                "model": "gpt",
                "tools": [{"type": "function", "function": {
                    "name": "Weber_read_file", "parameters": {}}}],
                "messages": [
                    {"role": "assistant", "content": null, "tool_calls": [{
                        "id": "Weber-call-1", "type": "function",
                        "function": {"name": "Weber_read_file", "arguments": "{}"}
                    }]},
                    {"role": "tool", "tool_call_id": "Weber-call-1", "content": "ok"}
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let sent = sent_to(&upstream).await;
        assert_eq!(
            sent["tools"][0]["function"]["name"], "Weber_read_file",
            "a tool's name is dispatch: {sent}"
        );
        assert_eq!(
            sent["messages"][0]["tool_calls"][0]["id"], "Weber-call-1",
            "a call's id is dispatch: {sent}"
        );
        assert_eq!(
            sent["messages"][0]["tool_calls"][0]["function"]["name"], "Weber_read_file",
            "the called function's name is dispatch: {sent}"
        );
        assert_eq!(
            sent["messages"][1]["tool_call_id"], "Weber-call-1",
            "the id a result answers to is dispatch: {sent}"
        );
    }

    #[tokio::test]
    async fn an_openai_arguments_key_is_never_masked_even_when_it_looks_like_a_value() {
        // Key and value are the same string, so nothing but the keys-are-not-
        // values rule can tell them apart. Through an embedded document the
        // rule has one more chance to go wrong: the string is parsed and
        // rebuilt, so a walk that yielded keys would write them back masked.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({
                "model": "gpt",
                "messages": [{"role": "assistant", "content": null, "tool_calls": [{
                    "id": "t1", "type": "function",
                    "function": {"name": "f", "arguments": "{\"Weber\":\"Weber\"}"}
                }]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let sent = sent_to(&upstream).await;
        let arguments: Value = serde_json::from_str(
            sent["messages"][0]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .expect("a string"),
        )
        .expect("the arguments still parse");
        assert_eq!(
            arguments["Weber"], "[PERSON_1]",
            "the value must be masked: {arguments}"
        );
        assert!(
            arguments.get("Weber").is_some(),
            "the key the tool reads its argument from must survive: {arguments}"
        );
    }

    #[tokio::test]
    async fn an_openai_tool_message_counts_against_both_tool_bounds() {
        // A `role: "tool"` message is the OpenAI shape of a result, and results
        // are the largest surface this slice opens — a coding agent's file
        // reads arrive in one. Charged against neither bound they would be
        // limited only by axum's 2 MB body default.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;

        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [
                {"role": "tool", "tool_call_id": "t1",
                 "content": "x".repeat(TEST_MAX_TOOL_CHARS + 1)}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body.contains("more text than"),
            "refused by the text bound: {body}"
        );

        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let messages: Vec<Value> = (0..(TEST_MAX_TOOL_CALLS + 1))
            .map(|n| json!({"role": "tool", "tool_call_id": format!("t{n}"), "content": "ok"}))
            .collect();
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": messages}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body.contains("detector calls"),
            "refused by the call bound rather than the text bound: {body}"
        );
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "neither refusal may cost the caller an upstream call"
        );
    }

    #[tokio::test]
    async fn numeric_leaves_count_against_both_tool_bounds() {
        // A numeric leaf is joined into the document's detection call, so its
        // digits are characters the detector reads and its place in the join
        // costs a separator. Charged nothing — which is what they were before
        // they were detected at all — a document of numbers would be a call
        // and a payload that neither bound had counted.
        //
        // Digits only, so nothing here could be refused by a text leaf
        // instead. 1 200 leaves of sixteen digits is 19 200 characters plus
        // 2 398 of separator, against a test ceiling of 20 000.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let numbers: Vec<Value> = (0..1_200)
            .map(|_| json!(9_007_199_254_740_991i64))
            .collect();
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "f", "input": {"bounds": numbers}}
            ]}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body.contains("more text than"),
            "refused by the text bound: {body}"
        );

        // And the call bound, which is the count that changed shape: a
        // document of numbers and nothing else used to cost zero calls, so
        // any number of them passed. It is one call each now.
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let blocks: Vec<Value> = (0..(TEST_MAX_TOOL_CALLS + 1))
            .map(|n| {
                json!({"type": "tool_use", "id": format!("t{n}"), "name": "f",
                            "input": {"n": 1}})
            })
            .collect();
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({"model": "claude",
                   "messages": [{"role": "assistant", "content": blocks}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body.contains("detector calls"),
            "refused by the call bound rather than the text bound: {body}"
        );
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "neither refusal may cost the caller an upstream call"
        );
    }

    #[tokio::test]
    async fn ordinary_openai_prompt_text_is_not_counted_against_the_tool_bounds() {
        // The other half of the bound: it must charge tool traffic and nothing
        // else, or a bound that refused every long conversation would satisfy
        // the test above.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [
                {"role": "user", "content": "x".repeat(TEST_MAX_TOOL_CHARS + 1)}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn arguments_that_are_not_json_are_the_callers_mistake_going_up() {
        // Models emit truncated `arguments`, and a client echoes the turn back
        // verbatim on the next request — so on the way up this is the caller's
        // to fix, and 400 says so.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "t1", "type": "function",
                    "function": {"name": "f", "arguments": "{\"path\": \"/home/we"}}]}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(journal(&path)[0]["error"], "tool_arguments_malformed");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "the refusal must cost the caller nothing upstream"
        );
    }

    #[tokio::test]
    async fn arguments_that_are_not_json_are_the_upstreams_mistake_coming_back() {
        // The same failure in the other direction, and the same 400 would be a
        // lie: the caller sent nothing wrong, the model wrote arguments that do
        // not parse, and a client told to fix its request would look for a
        // defect that is not there. `read_document` takes one provider name and
        // cannot tell the directions apart, so the response loop is where the
        // blame is set.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "t1", "type": "function", "function": {
                    "name": "f", "arguments": "{\"path\": \"/home/we"}}]
            }}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "hallo"}]}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "a malformed response is not the caller's mistake: {body}"
        );
        let lines = journal(&path);
        assert_eq!(
            lines[lines.len() - 1]["error"],
            "shape_response",
            "and the journal must not record it as the caller's either: {:?}",
            lines
        );
    }

    #[tokio::test]
    async fn a_response_shape_this_gateway_cannot_read_is_not_the_callers_mistake_either() {
        // The same re-blaming, one step earlier: `response_pointers` reads the
        // upstream body with the allowlists and the content dispatch that were
        // written for the request path, and every refusal they raise is a 400
        // by `status()`'s own rule. A tool call the gateway cannot account for
        // is the upstream's doing.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "t1", "type": "function", "notes": "new field",
                                "function": {"name": "f", "arguments": "{}"}}]
            }}]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "hallo"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    }

    #[tokio::test]
    async fn a_tool_schemas_nested_prose_is_masked_and_its_property_names_are_not() {
        // The schema goes in whole, so a string the gateway was never told to
        // look for — a `default`, here — is reached anyway. Its property name
        // sits one level up in the same object and is not.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "tools": [{
                    "name": "read_file",
                    "description": "Weber wrote this file",
                    "input_schema": {"type": "object", "properties": {
                        "Weber": {"type": "string", "default": "Weber"}
                    }}
                }],
                "messages": [{"role": "user", "content": "hallo"}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let tool = &sent_to(&upstream).await["tools"][0];
        assert_eq!(
            tool["name"], "read_file",
            "the tool name is dispatch: {tool}"
        );
        assert_eq!(
            tool["description"], "[PERSON_1] wrote this file",
            "a definition's prose is masked: {tool}"
        );
        assert_eq!(
            tool["input_schema"]["properties"]["Weber"]["default"], "[PERSON_1]",
            "a string nested in the schema is reached, and its property name is not: {tool}"
        );
    }

    #[tokio::test]
    async fn a_schemas_property_names_survive_even_where_the_schema_states_them_as_values() {
        // The end-to-end half of the `Shape` distinction. `required` names a
        // property in value position, so the walk sees data; masked, the
        // provider gets a schema requiring a property that does not exist, and
        // a model obeying it emits a placeholder as a key — which
        // `restore_value` never restores, because it restores values.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "tools": [{
                    "name": "read_file",
                    "description": "Weber owns this",
                    "input_schema": {
                        "type": "object",
                        "required": ["Weber"],
                        "properties": {"Weber": {"type": "string", "default": "Weber"}}
                    }
                }],
                "messages": [{"role": "user", "content": "hallo"}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let schema = &sent_to(&upstream).await["tools"][0]["input_schema"];
        assert_eq!(
            schema["required"],
            json!(["Weber"]),
            "a required property name is dispatch, not prose: {schema}"
        );
        assert!(
            schema["properties"].get("Weber").is_some(),
            "the property name itself is a key and was already safe: {schema}"
        );
        assert_eq!(
            schema["properties"]["Weber"]["default"], "[PERSON_1]",
            "a default is a value and is still masked: {schema}"
        );
    }

    #[tokio::test]
    async fn a_structure_heavy_schema_is_charged_for_its_text_and_not_its_punctuation() {
        // The bound is denominated in characters the detector reads. A schema
        // is mostly punctuation and property names, which it never reads, so
        // charging serialized size charges for structure — and refuses payloads
        // well under the ceiling it claims to enforce. This body is that case:
        // comfortably inside the bound by text, comfortably outside it by
        // serialization.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let properties: serde_json::Map<String, Value> = (0..155)
            .map(|n| {
                (
                    format!("parameter_{n:03}"),
                    json!({"type": "string", "description": "y".repeat(100)}),
                )
            })
            .collect();
        let body = json!({
            "model": "claude",
            "tools": [{
                "name": "wide",
                "description": "d",
                "input_schema": {"type": "object", "properties": properties}
            }],
            "messages": [{"role": "user", "content": "hallo"}]
        });

        // 155 descriptions of 100 characters, plus the tool's own one-character
        // description: 15 501 characters of text against a 20 000 bound, and
        // well past it once the punctuation around them is charged too. The
        // count stays under `max_tool_calls` so that only the text/serialized
        // distinction can decide the verdict.
        let text: usize = 155 * 100 + 1;
        assert!(text < TEST_MAX_TOOL_CHARS, "{text} characters of text");
        let serialized = body["tools"][0]["input_schema"].to_string().len();
        assert!(
            serialized > TEST_MAX_TOOL_CHARS,
            "and {serialized} serialized — the two verdicts have to disagree or this \
             test proves nothing"
        );

        let (status, returned) = call(state, "/v1/messages", body).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a schema whose text fits must be served whatever its punctuation costs: {returned}"
        );
    }

    #[tokio::test]
    async fn a_tool_result_counts_against_the_text_bound() {
        // A result is a `Text` slot, and counting only documents would have
        // left the largest surface bounded by axum's 2 MB body default —
        // two hundred times this bound. A coding agent's file reads land here.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1",
                     "content": "x".repeat(TEST_MAX_TOOL_CHARS + 1)}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "the refusal must cost the caller nothing upstream"
        );
    }

    #[tokio::test]
    async fn ordinary_prompt_text_is_not_counted_against_the_tool_bound() {
        // The other side of the flag: the tool bound has no business limiting
        // how long a conversation may be, and a bound that counted every string
        // would have refused an ordinary long prompt.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "user",
                              "content": "x".repeat(TEST_MAX_TOOL_CHARS + 1)}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn a_second_turn_reuses_the_first_turns_detection_of_the_same_tools() {
        // The regression whole-request batching would have introduced, and the
        // reason a document rather than a request is the unit. The cache keys
        // on the text, so what matters is that a joined text is *stable*: the
        // same tools joined the same way on turn two are the same string, and
        // the detector is not asked again. Join the whole request instead and
        // one new `tool_result` makes that string unique every turn, so the
        // definitions — free from here on — would be re-detected forever.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let tools = json!([{
            "name": "read_file",
            "description": "Read a file from the workspace",
            "input_schema": {"type": "object", "properties": {
                "path": {"type": "string", "description": "where the file lives"},
                "limit": {"type": "integer", "description": "how much of it to read"}
            }}
        }]);

        // With a credential, because the cache deliberately refuses to serve a
        // credential-less caller at all: folding them into one bucket would
        // make a hit distinguishable from a miss by response time across
        // tenants. So the second-turn saving is real for any caller with an API
        // key — which is every caller a provider will answer — and absent for a
        // demo stand that has none.
        let key = [("x-api-key", "tenant-one")];
        let (status, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/messages",
            json!({"model": "claude", "tools": tools,
                   "messages": [{"role": "user", "content": "hallo"}]}),
            &key,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let after_first = detector.received_requests().await.unwrap().len();
        assert_eq!(
            after_first, 3,
            "one call for the description, one for the whole schema, one for the prompt"
        );

        // Turn two: the same tools, plus a turn's worth of new conversation.
        let (status, _) = call_with_headers(
            state,
            "/v1/messages",
            json!({"model": "claude", "tools": tools, "messages": [
                {"role": "user", "content": "hallo"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "read_file",
                     "input": {"path": "/tmp/notes"}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "some output"}]}
            ]}),
            &key,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let second_turn = detector.received_requests().await.unwrap().len() - after_first;
        assert_eq!(
            second_turn, 2,
            "the tool definitions and the repeated prompt cost nothing the second time — \
             only the call's arguments and the result are text the cache has not seen"
        );
    }

    #[tokio::test]
    async fn a_tool_document_of_many_tiny_strings_is_one_call_and_is_served() {
        // **Repointed, because batching removed its subject.** It used to
        // assert that hundreds of two-character strings were refused: each was
        // its own round-trip, so the cost was the count and the text bound
        // could not see it. A document is now one call however many strings it
        // holds, so the refusal it guarded no longer exists and asserting it
        // would be asserting a bug.
        //
        // Inverted, it guards the thing that replaced it: this is the exact
        // payload the old bound existed to stop, and it now goes through in a
        // single detector call.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let many: Vec<Value> = (0..(TEST_MAX_TOOL_CALLS * 10))
            .map(|n| json!(format!("v{n}")))
            .collect();
        let document = json!({ "items": many });
        assert!(
            document.to_string().len() < TEST_MAX_TOOL_CHARS,
            "serialized length is an upper bound on characters, so this proves the text \
             bound lets it through"
        );
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f", "input": document}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            detector.received_requests().await.unwrap().len(),
            1,
            "four hundred strings, one call — that is the whole of the change"
        );
    }

    #[tokio::test]
    async fn a_tool_definitions_prose_counts_against_the_text_bound() {
        // Before this slice `tools` was refused whole, so every tool
        // description is text newly sent to the detector. Left uncharged it was
        // the definition-side twin of the `tool_result` gap: the bound named
        // "every tool structure this request newly scans" would have skipped
        // the one string every tool definition is guaranteed to carry.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "tools": [{
                    "name": "read_file",
                    "description": "x".repeat(TEST_MAX_TOOL_CHARS + 1),
                    "input_schema": {}
                }],
                "messages": [{"role": "user", "content": "hallo"}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{returned}");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "the refusal must cost the caller nothing upstream"
        );
    }

    #[tokio::test]
    async fn a_tool_definitions_prose_counts_against_the_call_bound_too() {
        // One round-trip per definition, and a client may carry many. The
        // schemas here are empty, so the descriptions are the only leaves and
        // nothing else can account for the refusal.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let tools: Vec<Value> = (0..(TEST_MAX_TOOL_CALLS + 1))
            .map(|n| json!({"name": format!("t{n}"), "description": "d", "input_schema": {}}))
            .collect();
        let body = json!({
            "model": "claude",
            "tools": tools,
            "messages": [{"role": "user", "content": "hallo"}]
        });
        assert!(
            body.to_string().len() < TEST_MAX_TOOL_CHARS,
            "serialized length is an upper bound on characters, so this proves the text \
             bound lets it through"
        );
        let (status, returned) = call(state, "/v1/messages", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{returned}");
        assert!(
            returned.contains("detector calls"),
            "refused by the call bound rather than the text bound: {returned}"
        );
        assert!(
            detector.received_requests().await.unwrap().is_empty(),
            "and refused before any of those round-trips were made"
        );
    }

    #[tokio::test]
    async fn the_call_bound_refuses_before_it_spends_the_budget_it_is_refusing_over() {
        // The bound exists to cap how long one request holds its session, so
        // spending most of it and then refusing defeats the point. Resized for
        // the call bound: one document is one call now, so it takes documents
        // rather than leaves to exceed it.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        // No single document is anywhere near the bound, so only counting them
        // together can refuse this at all.
        let blocks: Vec<Value> = (0..(TEST_MAX_TOOL_CALLS + 1))
            .map(|doc| {
                let fields: serde_json::Map<String, Value> = (0..3)
                    .map(|leaf| (format!("k{leaf}"), json!(format!("v{doc}{leaf}"))))
                    .collect();
                json!({"type": "tool_use", "id": format!("t{doc}"), "name": "f",
                       "input": Value::Object(fields)})
            })
            .collect();
        let body = json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": blocks}]
        });
        assert!(
            body.to_string().len() < TEST_MAX_TOOL_CHARS,
            "serialized length is an upper bound on characters, so this proves the text \
             bound lets it through"
        );
        let (status, returned) = call(state, "/v1/messages", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{returned}");
        assert!(
            detector.received_requests().await.unwrap().is_empty(),
            "a refusal must cost the caller nothing, and the detector calls are the cost: \
             {} were made before the bound said no",
            detector.received_requests().await.unwrap().len()
        );
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "the refusal must cost the caller nothing upstream"
        );
    }

    #[tokio::test]
    async fn many_small_tool_results_are_refused_by_the_call_bound_too() {
        // A `tool_result` is one detector round-trip like a document's leaf,
        // and it costs the same second. Spending the budget only on documents
        // would have bounded the calls a schema makes and left the calls a
        // turn's results make unbounded — the same half-a-bound the byte
        // filter had before it learned to count results.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let results: Vec<Value> = (0..(TEST_MAX_TOOL_CALLS + 1))
            .map(|n| json!({"type": "tool_result", "tool_use_id": format!("t{n}"), "content": "x"}))
            .collect();
        let body = json!({
            "model": "claude",
            "messages": [{"role": "user", "content": results}]
        });
        let (status, returned) = call(state, "/v1/messages", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{returned}");
        assert!(
            returned.contains("detector calls"),
            "refused by the call bound rather than the text bound: {returned}"
        );
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "the refusal must cost the caller nothing upstream"
        );
    }

    #[tokio::test]
    async fn a_tool_document_past_the_text_bound_is_refused_before_the_upstream_call() {
        // Detection runs at roughly a thousand characters a second, so a
        // document past the bound would outlast the detector timeout and cost
        // the caller the wait before failing. Refused here instead, and
        // refused before the call rather than after it.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "write_file",
                     "input": {"text": "x".repeat(TEST_MAX_TOOL_CHARS + 1)}}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "the refusal must cost the caller nothing upstream"
        );
    }

    #[tokio::test]
    async fn a_tool_document_nested_past_the_walks_bound_is_refused() {
        // `mapping.rs`'s depth bound stops a client's document from exhausting
        // the stack in the walk. Until this task there was no request path that
        // could reach it, so it was pinned only by constructing the error; this
        // drives it through the router.
        //
        // 70 is past the walk's 64 and inside serde_json's own parser limit of
        // 128 — measured, not assumed — so the request parses and is refused by
        // the bound that exists to refuse it, rather than by the parser.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let mut nested = json!("x");
        for _ in 0..70 {
            nested = json!({ "a": nested });
        }
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "read_file", "input": nested}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body.contains("nested deeper"),
            "refused by the depth bound rather than something else: {body}"
        );
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "the refusal must cost the caller nothing upstream"
        );
    }

    #[tokio::test]
    async fn a_tool_document_past_the_walks_node_bound_is_refused() {
        // The node bound was described in this file as unreachable through the
        // router — "`max_tool_chars` has already refused it" — and that stopped
        // being true. `handle` calls `json_leaves` to count the document before
        // it compares anything against either bound, so `TooLarge` propagates
        // first whatever the characters say. And a document can reach the node
        // bound while charging *nothing*: 10 001 empty arrays are 10 001 nodes,
        // no leaves, no characters and no detector call, so no character bound
        // at any setting would have stopped it. Shallow on purpose — this must
        // fail on nodes, not on the depth bound above it.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let wide = Value::Array(vec![json!([]); mapping::MAX_JSON_NODES]);
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "read_file", "input": wide}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body.contains("more values than this gateway will walk"),
            "refused by the node bound rather than by depth or by size: {body}"
        );
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "the refusal must cost the caller nothing upstream"
        );
    }

    #[tokio::test]
    async fn a_tool_document_within_the_text_bound_is_served() {
        // The other side of the same bound: a refusal that fired on everything
        // would satisfy the test above and serve nobody.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "write_file",
                     "input": {"text": "x".repeat(TEST_MAX_TOOL_CHARS - 100)}}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn the_client_gets_the_original_back() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "Hallo [PERSON_1]"}}]}),
        )
        .await;

        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Hallo Weber"), "not restored: {body}");
        assert!(
            !body.contains("PERSON_1"),
            "a placeholder reached the client: {body}"
        );
    }

    #[tokio::test]
    async fn a_detector_failure_refuses_the_request() {
        let detector = failing_detector().await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "fail closed means the upstream is never called"
        );
    }

    #[tokio::test]
    async fn an_unparsable_body_refuses_the_request() {
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt"}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_lost_mapping_refuses_the_response() {
        // The upstream invents a placeholder nobody issued. Handing it to the
        // client would put "[PERSON_9]" where a name belongs.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "Hallo [PERSON_9]"}}]}),
        )
        .await;

        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(!body.contains("PERSON_9") || body.contains("no mapping"));
    }

    #[tokio::test]
    async fn errors_never_carry_the_original_text() {
        let detector = failing_detector().await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;

        let (_, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user",
                   "content": "Weber, IBAN CH9300762011623852957"}]}),
        )
        .await;

        assert!(
            !body.contains(SECRET),
            "the error body echoed the text: {body}"
        );
        assert!(!body.contains("CH9300762011623852957"));
    }

    #[tokio::test]
    async fn an_upstream_error_keeps_its_status_and_body() {
        // A 429 turned into a generic 502 loses the retry semantics the client
        // needs to behave well.
        let detector = detector_returning(json!([])).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_json(
                json!({"error": {"type": "rate_limit_error", "message": "slow down"}}),
            ))
            .mount(&upstream)
            .await;

        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(
            body.contains("rate_limit_error"),
            "the envelope was lost: {body}"
        );
    }

    #[tokio::test]
    async fn an_upstream_error_still_gets_its_placeholders_restored() {
        // Providers quote the offending request back; that quote is masked.
        let detector = detector_returning(person_span()).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"error": {"message": "bad request near [PERSON_1]"}})),
            )
            .mount(&upstream)
            .await;

        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("near Weber"), "not restored: {body}");
    }

    #[tokio::test]
    async fn identifier_fields_outside_content_are_masked() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "user": "Weber",
                   "messages": [{"role": "user", "name": "Weber", "content": "Weber fragt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let sent =
            String::from_utf8(upstream.received_requests().await.unwrap()[0].body.clone()).unwrap();
        assert!(!sent.contains(SECRET), "an identifier field leaked: {sent}");
    }

    #[tokio::test]
    async fn a_malformed_span_refuses_the_request() {
        // A detector contract bug must not become raw egress.
        let detector = detector_returning(
            json!([{"entity_type": "PERSON", "start": 0, "end": 999, "confidence": 1.0,
                    "recognizer": "ner:fake", "tier": 2, "boosted": false}]),
        )
        .await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn provider_credentials_reach_the_upstream() {
        // Without these the proxy is not a drop-in: every authenticated request
        // fails before it reaches a model.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

        let (status, _) = call_with_headers(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Hallo"}]}),
            &[
                ("authorization", "Bearer sk-test"),
                ("openai-organization", "org-1"),
            ],
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let received = &upstream.received_requests().await.unwrap()[0];
        assert_eq!(received.headers["authorization"], "Bearer sk-test");
        assert_eq!(received.headers["openai-organization"], "org-1");
    }

    #[tokio::test]
    async fn anthropic_credentials_and_version_reach_the_upstream() {
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "text", "text": "ok"}]}),
        )
        .await;

        let (status, _) = call_with_headers(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "Hallo"}]}),
            &[
                ("x-api-key", "sk-ant-test"),
                ("anthropic-version", "2023-06-01"),
            ],
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let received = &upstream.received_requests().await.unwrap()[0];
        assert_eq!(received.headers["x-api-key"], "sk-ant-test");
        assert_eq!(received.headers["anthropic-version"], "2023-06-01");
    }

    #[tokio::test]
    async fn a_second_credential_is_asked_again_through_the_proxy() {
        // `detector.rs`'s own tests prove `DetectorClient::detect` separates
        // tenants; this proves the wiring between `handle` and `detect` does
        // not drop that separation on the way down. Two different
        // credentials are two different cache buckets, so both requests miss
        // and the detector must be asked exactly twice — pinned directly
        // rather than left to whatever the cache happens to do.
        let detector = detector_returning_expecting(json!([]), Some(2)).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);
        let body = json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber"}]});

        let (status_a, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            body.clone(),
            &[("authorization", "Bearer a")],
        )
        .await;
        let (status_b, _) = call_with_headers(
            state,
            "/v1/chat/completions",
            body,
            &[("authorization", "Bearer b")],
        )
        .await;

        assert_eq!(status_a, StatusCode::OK);
        assert_eq!(status_b, StatusCode::OK);
        // `expect(2)` is asserted when `detector` drops.
    }

    #[tokio::test]
    async fn the_journal_says_the_same_for_a_cached_detection() {
        // The evidence layer must not get weaker because an answer came from
        // memory. Two identical requests, the second served from the cache:
        // both masked lines must carry the same counts.
        //
        // `Some(1)` and a real credential, not `detector_returning` and
        // `call`'s headerless default: a credential-less request is never
        // cached at all (detection_cache.rs's
        // `two_anonymous_callers_never_share_a_cached_hit`), so without
        // both this test would see two misses — which would still pass,
        // since two identical misses report identical counts too, but
        // would no longer be testing what its name says it tests.
        let detector = detector_returning_expecting(person_span(), Some(1)).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let body = json!({"messages": [{"role": "user", "content": "Weber schreibt"}]});
        let headers = [("authorization", "Bearer k1")];

        let (first, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            body.clone(),
            &headers,
        )
        .await;
        let (second, _) =
            call_with_headers(Arc::clone(&state), "/v1/chat/completions", body, &headers).await;
        assert_eq!(first, StatusCode::OK);
        assert_eq!(second, StatusCode::OK);

        let lines = journal(&path);
        let masked: Vec<&Value> = lines
            .iter()
            .filter(|line| line["event"] == "masked")
            .collect();
        assert_eq!(masked.len(), 2, "two requests, two masked lines");
        assert_eq!(masked[0]["types"], masked[1]["types"]);
        assert_eq!(masked[0]["spans"], masked[1]["spans"]);
    }

    #[tokio::test]
    async fn a_cache_hit_forwards_the_same_body_as_the_miss() {
        // Counts survive a cache hit that applies the wrong offsets just as
        // well as a correct one: shifting every span by one character still
        // masks one PERSON out of one span found, so
        // `the_journal_says_the_same_for_a_cached_detection` would not
        // notice. The body sent upstream is the assertion that would — a
        // wrong offset masks a different slice of the same-length text, and
        // the two requests stop being byte-identical.
        //
        // `Some(1)` is the other half of that: two identical bodies would
        // agree whether the second came from the cache or from a second,
        // equally correct miss, so nothing above pins that a hit actually
        // happened. Without it, turning the cache off everywhere in this
        // suite (`detection_cache_entries = 0`) leaves every test here
        // green, including this one — the offset mutation only bites
        // because a hit happens to occur, not because this test requires
        // one.
        //
        // A real credential, not `call`'s headerless default: a
        // credential-less request is never cached at all (see
        // detection_cache.rs's `two_anonymous_callers_never_share_a_cached_hit`),
        // so without one this test would see two misses and `Some(1)`
        // above would fail for an unrelated reason.
        let detector = detector_returning_expecting(person_span(), Some(1)).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);
        let body = json!({"messages": [{"role": "user", "content": "Weber schreibt"}]});
        let headers = [("authorization", "Bearer k1")];

        call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            body.clone(),
            &headers,
        )
        .await;
        call_with_headers(Arc::clone(&state), "/v1/chat/completions", body, &headers).await;

        let received = upstream.received_requests().await.unwrap();
        assert_eq!(received.len(), 2, "two requests, two upstream calls");
        assert_eq!(
            received[0].body, received[1].body,
            "a cache hit forwarded a body different from the miss that computed it"
        );
        // `Some(1)` above is asserted when `detector` drops: two requests,
        // one detector call, is what this test's name already claims.
    }

    #[tokio::test]
    async fn headers_outside_the_allowlist_are_not_forwarded() {
        // A client's cookies are not the model provider's business.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

        call_with_headers(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Hallo"}]}),
            &[
                ("cookie", "session=secret"),
                ("authorization", "Bearer sk-test"),
            ],
        )
        .await;

        let received = &upstream.received_requests().await.unwrap()[0];
        assert!(
            received.headers.get("cookie").is_none(),
            "a cookie was forwarded"
        );
    }

    /// An SSE upstream that sends `body` and then closes.
    ///
    /// The content-type has to travel through `set_body_raw`'s mime
    /// parameter, not a separately inserted header: `ResponseTemplate`
    /// stores a body call's mime apart from its headers and applies it last,
    /// so a header inserted earlier is silently overwritten by whichever body
    /// call runs after it.
    async fn upstream_streaming(route: &str, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher(route))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;
        server
    }

    const STREAM_BODY: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hallo [PER\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"SON_1]!\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    /// An upstream that promises more body than it sends and then drops the
    /// connection. wiremock cannot sever a stream mid-body, and the claim under
    /// test is exactly what happens when one is severed.
    async fn truncating_upstream(body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut scratch = vec![0u8; 8192];
            let _ = socket.read(&mut scratch).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                        content-length: 100000\r\n\r\n";
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            // Dropped here, far short of the promised length.
        });
        base
    }

    fn state_for(detector: &MockServer, upstream_base: String) -> Arc<AppState> {
        // The `TempDir` is dropped here rather than threaded through: nothing
        // in the streaming tests reads the journal back, and the open file
        // handle keeps working after the directory entry is unlinked.
        let dir = tempfile::tempdir().expect("a temp dir");
        let audit =
            Arc::new(crate::audit::Audit::open(&dir.path().join("audit.jsonl")).expect("opens"));
        Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5), 16, UNCAPPED),
            upstream: reqwest::Client::new(),
            openai_base: upstream_base.clone(),
            anthropic_base: upstream_base,
            sessions: SessionStore::new(test_limits()),
            audit,
            max_tool_chars: TEST_MAX_TOOL_CHARS,
            max_tool_calls: TEST_MAX_TOOL_CALLS,
        })
    }

    #[tokio::test]
    async fn a_severed_stream_still_serves_what_was_already_restored() {
        // The waiting event is restored and safe; the connection dying does not
        // make it unsafe, and dropping it would lose text the client paid for.
        let detector = detector_returning(person_span()).await;
        let base = truncating_upstream(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hallo [PERSON_1]\"}}]}\n\n",
        )
        .await;

        let (status, served) = call(
            state_for(&detector, base),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            served.contains(SECRET),
            "restored text was dropped: {served}"
        );
        assert!(
            served.contains("tessera_restoration_failed"),
            "the break was not reported: {served}"
        );
    }

    #[tokio::test]
    async fn an_extended_thinking_request_never_reaches_the_upstream() {
        // Refusing at the first streamed block would already have cost the
        // caller the call and its tokens.
        let detector = detector_returning(json!([])).await;
        let upstream = MockServer::start().await;
        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "stream": true,
                   "thinking": {"type": "enabled", "budget_tokens": 1024},
                   "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_streaming_response_reaches_the_client_restored() {
        // The placeholder is split across two events; the client must see the
        // value once, whole, and never the token.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_streaming("/v1/chat/completions", STREAM_BODY).await;

        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(served.contains(SECRET), "not restored: {served}");
        assert!(!served.contains("PERSON_1"), "placeholder served: {served}");
        assert!(served.ends_with("data: [DONE]\n\n"), "truncated: {served}");
    }

    #[tokio::test]
    async fn a_stream_holds_no_session_lock() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_streaming("/v1/chat/completions", STREAM_BODY).await;
        let state = state(&detector, &upstream);

        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", "Bearer k1")
            .header(SESSION_HEADER, "conv-1")
            .body(Body::from(
                json!({"model": "gpt", "stream": true,
                       "messages": [{"role": "user", "content": "Weber schreibt"}]})
                .to_string(),
            ))
            .unwrap();
        let response = router(Arc::clone(&state)).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // The stream has not been read yet. If the masking guard were still
        // held, this would fail — and a stream that hung would block its
        // conversation for as long as it hung.
        // `acquire` itself claims the mapping's lock synchronously whenever it
        // is free, so a successful claim here — without a second, separate
        // `try_lock` that would deadlock against `claimed`'s own guard — is
        // exactly the proof that nothing else (in particular, no still-live
        // stream) is holding it.
        let claimed = state
            .sessions
            .acquire(&test_key("conv-1", "Bearer k1"))
            .unwrap();
        assert!(
            claimed.guard.is_some(),
            "the stream is holding its session lock"
        );

        // Draining afterwards proves the stream still restores from the
        // snapshot it was handed.
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let served = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            served.contains(SECRET),
            "the stream did not restore: {served}"
        );
    }

    #[tokio::test]
    async fn the_anthropic_stream_shape_is_restored_too() {
        // Anthropic carries text under a different pointer and separates blocks
        // with events that carry none.
        let detector = detector_returning(person_span()).await;
        let upstream = MockServer::start().await;
        let body = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\
             \"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
             \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hallo [PER\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\
             \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"SON_1]\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(body.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&upstream)
            .await;

        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "stream": true,
                   "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(served.contains(SECRET), "not restored: {served}");
        assert!(!served.contains("PERSON_1"), "placeholder served: {served}");
        assert!(
            served.contains("event: message_stop"),
            "truncated: {served}"
        );
    }

    #[tokio::test]
    async fn an_oddly_cased_media_type_is_still_a_stream() {
        // `Text/Event-Stream; charset=utf-8` is the same media type, and missing
        // it would buffer a live response and fail to parse it as JSON.
        let detector = detector_returning(person_span()).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                STREAM_BODY.as_bytes().to_vec(),
                "Text/Event-Stream; charset=utf-8",
            ))
            .mount(&upstream)
            .await;

        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(served.contains(SECRET), "not streamed: {served}");
        assert!(!served.contains("PERSON_1"), "placeholder served: {served}");
    }

    #[tokio::test]
    async fn a_streaming_response_is_served_as_a_stream() {
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_streaming("/v1/chat/completions", "data: [DONE]\n\n").await;
        let response = router(state(&detector, &upstream))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": "gpt", "stream": true,
                               "messages": [{"role": "user", "content": "Hallo"}]})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
    }

    #[tokio::test]
    async fn the_upstream_is_still_told_to_stream() {
        // Dropping the flag would make the provider answer with a whole body the
        // client is not waiting for.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_streaming("/v1/chat/completions", "data: [DONE]\n\n").await;
        call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;
        let received = &upstream.received_requests().await.unwrap()[0];
        let sent: Value = serde_json::from_slice(&received.body).unwrap();
        assert_eq!(sent["stream"], json!(true));
    }

    #[tokio::test]
    async fn a_stream_carrying_an_unknown_placeholder_ends_with_an_error() {
        // Bytes have already gone out, so the request cannot be refused. It ends
        // instead — the client never receives a token in place of a name.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hallo [PERSON_9]\"}}]}\n\n",
                "data: [DONE]\n\n",
            ),
        )
        .await;

        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(served.contains("tessera_restoration_failed"), "{served}");
        assert!(
            !served.contains("data: [DONE]"),
            "served as complete: {served}"
        );
        // Nor does the error event hand over the token. A client is never
        // supposed to see a placeholder — restoration exists so they do not —
        // and a failure is not a reason to show them one.
        assert!(
            !served.contains("PERSON_9"),
            "the error event names the placeholder: {served}"
        );
    }

    #[tokio::test]
    async fn a_buffered_unknown_placeholder_is_refused_without_naming_it() {
        // The same rule on the buffered path, where the refusal body is the
        // whole response rather than a trailing event.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "text", "text": "Hallo [PERSON_9]"}]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{returned}");
        assert!(
            !returned.contains("PERSON_9"),
            "the refusal body names the placeholder: {returned}"
        );
    }

    #[tokio::test]
    async fn a_broken_event_does_not_take_the_good_ones_with_it() {
        // The stream ends on the malformed event, but the delta before it was
        // already restored and correct, and the client gets it.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hallo [PERSON_1]\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"trunc\n\n",
            ),
        )
        .await;

        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": SECRET}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            served.contains(SECRET),
            "restored text was dropped: {served}"
        );
        assert!(
            served.contains("tessera_restoration_failed"),
            "the failure was not reported: {served}"
        );
        assert!(!served.contains("PERSON_1"), "placeholder served: {served}");
    }

    #[tokio::test]
    async fn a_streaming_error_response_keeps_the_buffered_path() {
        // A 429 is not a stream, whatever it says it is.
        let detector = detector_returning(json!([])).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "3")
                    // `set_body_raw`'s mime, not a separate header: a header
                    // inserted alongside `set_body_json` would be overwritten
                    // by that call's own `application/json` mime, and this
                    // test's whole premise is a response that claims to be a
                    // stream while carrying a non-success status.
                    .set_body_raw(
                        json!({"error": {"message": "slow down"}}).to_string(),
                        "text/event-stream",
                    ),
            )
            .mount(&upstream)
            .await;

        let (status, served) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "stream": true,
                   "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(served.contains("slow down"), "{served}");
    }

    #[tokio::test]
    async fn a_non_json_upstream_error_keeps_its_status_and_body() {
        let detector = detector_returning(json!([])).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"))
            .mount(&upstream)
            .await;

        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Hallo"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body.contains("bad gateway"), "the body was lost: {body}");
    }

    #[tokio::test]
    async fn retry_after_survives_an_upstream_rate_limit() {
        let detector = detector_returning(json!([])).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "30")
                    .set_body_json(json!({"error": {"message": "slow down"}})),
            )
            .mount(&upstream)
            .await;

        let response = router(state(&detector, &upstream))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model": "gpt",
                               "messages": [{"role": "user", "content": "Hallo"}]})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["retry-after"], "30");
    }

    #[tokio::test]
    async fn one_providers_key_never_reaches_the_other() {
        // A caller holding both sets of credentials must not have the Anthropic
        // key posted to OpenAI.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

        call_with_headers(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Hallo"}]}),
            &[
                ("authorization", "Bearer sk-openai"),
                ("x-api-key", "sk-ant-secret"),
            ],
        )
        .await;

        let received = &upstream.received_requests().await.unwrap()[0];
        assert_eq!(received.headers["authorization"], "Bearer sk-openai");
        assert!(
            received.headers.get("x-api-key").is_none(),
            "the Anthropic key crossed to OpenAI"
        );
    }

    #[tokio::test]
    async fn a_tool_bearing_request_is_refused() {
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({"choices": []})).await;

        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "tools": [{"type": "function"}],
                   "messages": [{"role": "user", "content": "Weber"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_anthropic_shape_is_masked_too() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/messages",
            json!({"content": [{"type": "text", "text": "Hallo [PERSON_1]"}]}),
        )
        .await;

        let (status, body) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({
                "model": "claude",
                "system": "Weber ist der Mandant",
                "messages": [{"role": "user", "content": [{"type": "text", "text": "Weber fragt"}]}]
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let sent =
            String::from_utf8(upstream.received_requests().await.unwrap()[0].body.clone()).unwrap();
        assert!(!sent.contains(SECRET), "the system field leaked: {sent}");
        assert!(body.contains("Hallo Weber"));
    }

    #[tokio::test]
    async fn one_session_keeps_one_value_on_one_placeholder() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);
        let headers = session_headers("Bearer k1", "conv-1");

        for _ in 0..2 {
            call_with_headers(
                Arc::clone(&state),
                "/v1/chat/completions",
                json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
                &headers,
            )
            .await;
        }

        let received = upstream.received_requests().await.unwrap();
        let first = String::from_utf8(received[0].body.clone()).unwrap();
        let second = String::from_utf8(received[1].body.clone()).unwrap();
        assert!(first.contains("[PERSON_1]"));
        assert!(
            second.contains("[PERSON_1]"),
            "the second turn renamed the same person: {second}"
        );
    }

    #[tokio::test]
    async fn a_literal_a_stored_value_carries_survives_a_later_turn_reusing_its_number() {
        // Why the sweep runs on the upstream body and not on the slot loop's
        // result. Turn one's caller writes `[PERSON_3]` themselves inside a
        // message the detector spans whole, so the session stores a *value*
        // carrying that literal. `reserve_literals` maps it to itself for that
        // one request and `absorb` does not carry the self-map, while `next`
        // only ever climbs — so by turn three `[PERSON_3]` is free, and turn
        // three issues it for `Braun`.
        //
        // Restoring `content` strictly puts turn one's stored value back,
        // literal and all. A sweep over *that* would read the literal as a
        // token this request issued and hand the client `Braun Weber`.
        // Sweeping the untouched upstream body cannot: what the response
        // carries is `[PERSON_1]`, which this request did not issue.
        use wiremock::matchers::body_string_contains;
        let detector = MockServer::start().await;
        let span = |end: usize| {
            json!({"spans": [{"entity_type": "PERSON", "start": 0, "end": end,
                              "confidence": 1.0, "recognizer": "ner:fake",
                              "tier": 2, "boosted": false}],
                   "layers_run": ["deterministic"]})
        };
        for (text, end) in [("[PERSON_3] Weber", 16), ("Meier", 5), ("Braun", 5)] {
            Mock::given(method("POST"))
                .and(path("/detect"))
                .and(body_string_contains(text))
                .respond_with(ResponseTemplate::new(200).set_body_json(span(end)))
                .mount(&detector)
                .await;
        }
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"spans": [], "layers_run": ["deterministic"]})),
            )
            .mount(&detector)
            .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "[PERSON_1]"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);
        let headers = session_headers("Bearer k1", "conv-1");

        let mut served = String::new();
        for text in ["[PERSON_3] Weber", "Meier", "Braun"] {
            let (status, body) = call_with_headers(
                Arc::clone(&state),
                "/v1/chat/completions",
                json!({"model": "gpt", "messages": [{"role": "user", "content": text}]}),
                &headers,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            served = body;
        }

        assert!(
            served.contains("[PERSON_3] Weber"),
            "the caller's own literal was overwritten: {served}"
        );
        assert!(
            !served.contains("Braun"),
            "a value this turn issued was restored into an earlier turn's text: {served}"
        );
    }

    #[tokio::test]
    async fn a_caller_writing_a_session_owned_literal_gets_its_own_text_back() {
        // Turn one issues `[PERSON_1]` for `Weber` and the session keeps it.
        // Turn two's caller writes that literal itself and the provider echoes
        // it into two fields: `content`, which the slot loop describes, and
        // `refusal`, which it does not.
        //
        // The two answers differ, and the difference is the point. `refusal` is
        // reached only by the sweep, which asks `Provenance::restorable` —
        // turn two issued nothing, so the token is not this request's to claim
        // and the caller gets back the bytes it sent. `content` is reached by
        // the slot loop, which restores strictly out of the session table and
        // does not consult provenance at all, so it hands back turn one's
        // value. #32 is what makes the two agree; until then this is the
        // behaviour, asserted here rather than left to be discovered.
        //
        // **This is not the ordering pin**, though an earlier draft of the plan
        // said it was: measured, it passes with the sweep on either side of the
        // slot loop. A token inside a value masked from *this* request is
        // necessarily also in this request's `written` set, so `restorable`
        // excludes it whichever order runs. The hazard needs a value that
        // entered the table in an earlier turn, and
        // `a_literal_a_stored_value_carries_survives_a_later_turn_reusing_its_number`
        // above is the test that fails when the sweep moves.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {
                "role": "assistant",
                "content": "[PERSON_1]",
                "refusal": "I cannot help with [PERSON_1]"
            }}]}),
        )
        .await;
        let state = state(&detector, &upstream);
        let headers = session_headers("Bearer k1", "conv-1");

        let (status, first) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &headers,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");

        // Turn two names the placeholder itself and says nothing the detector
        // finds, so this request issues nothing at all.
        let (status, second) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "[PERSON_1] wer?"}]}),
            &headers,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");

        let served: Value = serde_json::from_str(&second).expect("a JSON body");
        let message = &served["choices"][0]["message"];
        assert_eq!(
            message["refusal"], "I cannot help with [PERSON_1]",
            "the sweep claimed a token this request never issued: {second}"
        );
        assert_eq!(
            message["content"], SECRET,
            "the described field stopped restoring strictly out of the table: {second}"
        );
    }

    #[tokio::test]
    async fn a_guessed_session_id_returns_no_other_callers_value() {
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "[PERSON_1]"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);

        // The first caller puts Weber into the session called "shared".
        call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "shared"),
        )
        .await;

        // A second caller guesses the id but holds a different key, and asks
        // the model to echo the placeholder back.
        let (status, body) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "[PERSON_1] wer?"}]}),
            &session_headers("Bearer k2", "shared"),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            !body.contains(SECRET),
            "another caller's value came back: {body}"
        );
        assert!(body.contains("[PERSON_1]"));
    }

    #[tokio::test]
    async fn a_refused_request_leaves_the_session_untouched() {
        // The first message masks; the second has no "Weber" and the detector
        // refuses it, so the request dies after masking has already happened.
        let detector = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .and(wiremock::matchers::body_string_contains(SECRET))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"spans": person_span(), "layers_run": ["deterministic"]}),
                ),
            )
            .mount(&detector)
            .await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&detector)
            .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);

        let (status, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [
                {"role": "user", "content": "Weber schreibt"},
                {"role": "user", "content": "und dann?"}
            ]}),
            &session_headers("Bearer k1", "conv-1"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        let session = state
            .sessions
            .acquire(&test_key("conv-1", "Bearer k1"))
            .unwrap()
            .session;
        assert!(
            session.mapping.lock().await.is_empty(),
            "a refused request left values in the session"
        );
    }

    #[tokio::test]
    async fn a_request_refused_by_the_journal_leaves_the_session_untouched() {
        // The other refusal class: masking succeeded, so the values exist and
        // are ready to commit, and the journal is what refuses. Nothing left
        // the perimeter, so nothing of the caller's may stay behind — neither
        // in the session's table nor against its value budget.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let state = Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5), 16, UNCAPPED),
            upstream: reqwest::Client::new(),
            openai_base: upstream.uri(),
            anthropic_base: upstream.uri(),
            sessions: SessionStore::new(test_limits()),
            audit: Arc::new(crate::audit::failing_audit_for_tests()),
            max_tool_chars: TEST_MAX_TOOL_CHARS,
            max_tool_calls: TEST_MAX_TOOL_CALLS,
        });

        let (status, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "conv-1"),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let session = state
            .sessions
            .acquire(&test_key("conv-1", "Bearer k1"))
            .unwrap()
            .session;
        assert!(
            session.mapping.lock().await.is_empty(),
            "a request the journal refused left values in the session"
        );
    }

    #[tokio::test]
    async fn a_failing_request_does_not_evict_a_live_third_party_session() {
        // Session "a" commits a real value through an ordinary successful
        // request. Session "b" then takes the store's second slot with a
        // request that fails during masking, leaving its entry created but
        // empty — `acquire` runs, and creates the entry, before `mask_all`
        // can fail. A third session's request also fails during masking,
        // and needs a slot in a now-full store: it must evict "b"
        // (reclaimable), never "a" (live) — even though "a" is the older
        // of the two by last_seen.
        let detector = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .and(wiremock::matchers::body_string_contains(SECRET))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"spans": person_span(), "layers_run": ["deterministic"]}),
                ),
            )
            .mount(&detector)
            .await;
        Mock::given(method("POST"))
            .and(path("/detect"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&detector)
            .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state_with(
            &detector,
            &upstream,
            Limits {
                idle: Duration::from_secs(1800),
                max_sessions: 2,
                max_values: 8,
            },
        )
        .0;

        // "a" gets a real, committed value.
        call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "a"),
        )
        .await;

        // "b" has no "Weber" in it, so the detector 503s and the request
        // fails — but its session entry was already created, and stays
        // empty and reclaimable.
        let (status_b, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "und dann?"}]}),
            &session_headers("Bearer k1", "b"),
        )
        .await;
        assert_eq!(status_b, StatusCode::BAD_GATEWAY);

        // "c" also fails during masking, and needs a slot in a store
        // already holding {a, b} at its cap of 2.
        let (status_c, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "und dann?"}]}),
            &session_headers("Bearer k1", "c"),
        )
        .await;
        assert_eq!(status_c, StatusCode::BAD_GATEWAY);

        // Session "a" must still hold Weber's placeholder.
        let session_a = state
            .sessions
            .acquire(&test_key("a", "Bearer k1"))
            .unwrap()
            .session;
        assert_eq!(
            session_a
                .mapping
                .lock()
                .await
                .restore("[PERSON_1]")
                .unwrap(),
            "Weber",
            "a failing request for a different session evicted a's live value"
        );
    }

    #[tokio::test]
    async fn a_value_past_the_cap_is_still_masked_and_still_restored() {
        let two_spans = json!([
            {"entity_type": "PERSON", "start": 0, "end": 5, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
            {"entity_type": "PERSON", "start": 10, "end": 15, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false}
        ]);
        let detector = detector_returning(two_spans).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant",
                   "content": "[PERSON_1] und [PERSON_2]"}}]}),
        )
        .await;
        let state = state_with(
            &detector,
            &upstream,
            Limits {
                idle: Duration::from_secs(1800),
                max_sessions: 8,
                max_values: 1,
            },
        )
        .0;

        let (status, body) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber und Meier"}]}),
            &session_headers("Bearer k1", "conv-1"),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let sent =
            String::from_utf8(upstream.received_requests().await.unwrap()[0].body.clone()).unwrap();
        assert!(!sent.contains(SECRET), "the value past the cap went up raw");
        assert!(
            !sent.contains("Meier"),
            "the value past the cap went up raw"
        );
        assert!(body.contains(SECRET) && body.contains("Meier"));

        let session = state
            .sessions
            .acquire(&test_key("conv-1", "Bearer k1"))
            .unwrap()
            .session;
        assert_eq!(session.mapping.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn no_header_creates_no_session() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state(&detector, &upstream);

        let (status, _) = call(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state.sessions.live(),
            0,
            "a request that asked for no session got one anyway"
        );
    }

    #[tokio::test]
    async fn a_session_header_against_a_disabled_gateway_is_refused() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state_with(
            &detector,
            &upstream,
            Limits {
                idle: Duration::ZERO,
                max_sessions: 0,
                max_values: 0,
            },
        )
        .0;

        let (status, body) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "conv-1"),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("disabled"), "{body}");
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            0,
            "a refused request still reached the provider"
        );
    }

    #[tokio::test]
    async fn a_malformed_session_id_is_refused() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;

        let (status, _) = call_with_headers(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "conv 1"),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            detector.received_requests().await.unwrap().len(),
            0,
            "a malformed header cost a detection pass"
        );
    }

    #[tokio::test]
    async fn a_saturated_store_refuses_before_the_detector_runs() {
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let state = state_with(
            &detector,
            &upstream,
            Limits {
                idle: Duration::from_secs(1800),
                max_sessions: 1,
                max_values: 8,
            },
        )
        .0;

        // The only slot belongs to a session another request is inside right
        // now, exactly as it would be mid-`mask_all`.
        let mut held = state
            .sessions
            .acquire(&test_key("conv-1", "Bearer k1"))
            .unwrap();
        let _guard = held
            .guard
            .take()
            .expect("a fresh session is always claimable");

        let (status, body) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": "Weber schreibt"}]}),
            &session_headers("Bearer k1", "conv-2"),
        )
        .await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("in flight"), "{body}");
        assert_eq!(
            detector.received_requests().await.unwrap().len(),
            0,
            "a refused request cost a detection pass"
        );
        assert_eq!(
            upstream.received_requests().await.unwrap().len(),
            0,
            "a refused request still reached the provider"
        );
    }

    #[tokio::test]
    async fn a_served_request_leaves_two_records() {
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, _) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let lines = journal(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"], "masked");
        assert_eq!(lines[0]["provider"], "openai");
        assert_eq!(lines[0]["types"]["PERSON"], 1);
        assert_eq!(lines[0]["spans"], 1);
        assert_eq!(lines[1]["result"], "completed");
        assert_eq!(lines[1]["status"], 200);
        assert_eq!(lines[0]["request"], lines[1]["request"]);
    }

    #[tokio::test]
    async fn a_detector_failure_leaves_one_record_and_calls_nobody() {
        let detector = failing_detector().await;
        let upstream = upstream_returning("/v1/chat/completions", json!({})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, _) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        let lines = journal(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["event"], "outcome");
        assert_eq!(lines[0]["upstream"], false);
        assert_eq!(lines[0]["result"], "refused");
        assert_eq!(lines[0]["error"], "detector_status");
        assert!(
            upstream
                .received_requests()
                .await
                .expect("recorded")
                .is_empty(),
            "nothing may reach the provider when the request is refused"
        );
    }

    #[tokio::test]
    async fn each_refusal_records_its_own_class() {
        // The invariant a guard that inferred its outcome would violate
        // silently, so it is exercised per variant rather than once.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({})).await;

        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, _) = call_with_headers(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "hello"}]}),
            &[(SESSION_HEADER, "not a valid id!")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(journal(&path)[0]["error"], "session_bad_id");

        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, _) = call(state, "/v1/chat/completions", json!({"messages": "wrong"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(journal(&path)[0]["error"], "shape_request");
    }

    #[tokio::test]
    async fn a_journal_that_cannot_be_written_refuses_before_the_provider() {
        // Fail-closed end to end: no evidence, no request.
        let detector = detector_finding_weber().await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let state = Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5), 16, UNCAPPED),
            upstream: reqwest::Client::new(),
            openai_base: upstream.uri(),
            anthropic_base: upstream.uri(),
            sessions: SessionStore::new(test_limits()),
            audit: Arc::new(crate::audit::failing_audit_for_tests()),
            max_tool_chars: TEST_MAX_TOOL_CHARS,
            max_tool_calls: TEST_MAX_TOOL_CALLS,
        });

        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("audit unavailable"));
        assert!(
            !body.contains('/'),
            "no filesystem detail reaches the client"
        );
        assert!(
            upstream
                .received_requests()
                .await
                .expect("recorded")
                .is_empty(),
            "an unrecorded request must not reach the provider"
        );
    }

    #[tokio::test]
    async fn the_masked_record_precedes_the_provider_call() {
        // Asserted structurally: the upstream mock reads the journal as it
        // answers, so the ordering is observed rather than timed.
        let detector = detector_finding_weber().await;
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        let audit = Arc::new(crate::audit::Audit::open(&path).expect("opens"));

        let seen = Arc::new(std::sync::Mutex::new(0usize));
        let upstream = MockServer::start().await;
        let counter = Arc::clone(&seen);
        let watched = path.clone();
        Mock::given(method("POST"))
            .and(path_matcher("/v1/chat/completions"))
            .respond_with(move |_: &wiremock::Request| {
                *counter.lock().expect("lock") = std::fs::read_to_string(&watched)
                    .map(|text| text.lines().count())
                    .unwrap_or(0);
                ResponseTemplate::new(200)
                    .set_body_json(json!({"choices": [{"message": {"content": "ok"}}]}))
            })
            .mount(&upstream)
            .await;

        let state = Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5), 16, UNCAPPED),
            upstream: reqwest::Client::new(),
            openai_base: upstream.uri(),
            anthropic_base: upstream.uri(),
            sessions: SessionStore::new(test_limits()),
            audit,
            max_tool_chars: TEST_MAX_TOOL_CHARS,
            max_tool_calls: TEST_MAX_TOOL_CALLS,
        });
        call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;

        assert_eq!(
            *seen.lock().expect("lock"),
            1,
            "the masked record must be on disk before the provider is called"
        );
    }

    /// A port bound and released: nothing listens there, so a connection to it
    /// is refused before a byte of the request is written.
    fn a_dead_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        listener.local_addr().expect("an address").port()
    }

    fn state_against(
        detector: &MockServer,
        base: String,
    ) -> (Arc<AppState>, tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        let audit = Arc::new(crate::audit::Audit::open(&path).expect("opens"));
        let state = Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5), 16, UNCAPPED),
            upstream: reqwest::Client::new(),
            openai_base: base.clone(),
            anthropic_base: base,
            sessions: SessionStore::new(test_limits()),
            audit,
            max_tool_chars: TEST_MAX_TOOL_CHARS,
            max_tool_calls: TEST_MAX_TOOL_CALLS,
        });
        (state, dir, path)
    }

    #[tokio::test]
    async fn a_provider_that_was_never_reached_records_that_nothing_left() {
        // `masked` claims the bytes left before they do, because a request
        // that dies mid-flight did send them and a journal that said otherwise
        // would under-report the one thing it exists to report. A refused
        // connection is the single failure that is knowably the other way:
        // `upstream: true` here would tell an auditor a request reached a
        // provider that never accepted one.
        let detector = detector_finding_weber().await;
        let (state, _dir, path) =
            state_against(&detector, format!("http://127.0.0.1:{}", a_dead_port()));

        let (status, _) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        let lines = journal(&path);
        assert_eq!(lines.len(), 2, "the request was masked, then refused");
        assert_eq!(lines[0]["event"], "masked");
        assert_eq!(lines[1]["result"], "refused");
        assert_eq!(lines[1]["error"], "upstream_failed");
        assert_eq!(
            lines[1]["upstream"], false,
            "the connection was never established, so nothing left the perimeter"
        );
    }

    #[tokio::test]
    async fn a_provider_that_accepted_and_vanished_still_records_bytes_leaving() {
        // The other side of the correction above, and what keeps it narrow: a
        // provider that accepts the connection and then disappears has already
        // read whatever the socket carried. `send` fails here too, and this is
        // the case where the conservative claim must stand — under-reporting a
        // request that did leave is the dangerous direction for a privacy
        // journal, and a correction that fired on every `send` error would do
        // exactly that.
        let detector = detector_finding_weber().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binds");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                // Accepted, then closed with no response at all.
                drop(stream);
            }
        });
        let (state, _dir, path) = state_against(&detector, base);

        let (status, _) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);

        let lines = journal(&path);
        assert_eq!(lines[1]["error"], "upstream_failed");
        assert_eq!(
            lines[1]["upstream"], true,
            "the connection was established, so the conservative claim stands"
        );
    }

    #[tokio::test]
    async fn a_session_turn_counts_its_own_request_not_the_table() {
        // The detector reports a PERSON at a fixed span regardless of the
        // text, so each turn masks exactly one value of its own — but a
        // different one each time. The session's mapping therefore
        // accumulates to two entries while each request's own count stays at
        // one; a version that read the count off the mapping instead of the
        // request would report two on the second turn.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let bodies = [
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
            json!({"messages": [{"role": "user", "content": "Meier called"}]}),
        ];
        for body in bodies {
            call_with_headers(
                Arc::clone(&state),
                "/v1/chat/completions",
                body,
                &session_headers("sk-tenant", "chat-1"),
            )
            .await;
        }

        let lines = journal(&path);
        let masked: Vec<&Value> = lines
            .iter()
            .filter(|line| line["event"] == "masked")
            .collect();
        assert_eq!(masked.len(), 2);
        assert_eq!(
            masked[1]["types"]["PERSON"], 1,
            "the second turn describes the request, not the session's running total"
        );
        assert_eq!(
            masked[0]["tenant"], masked[1]["tenant"],
            "one credential is one tenant"
        );
        assert_eq!(masked[0]["session"], masked[1]["session"]);
        assert_eq!(masked[0]["tenant"].as_str().expect("a digest").len(), 32);
    }

    #[tokio::test]
    async fn a_repeated_value_carries_its_type_past_the_mapping_unvalidated() {
        // The path that makes the audit module's own type check load-bearing
        // rather than defensive. `Mapping::placeholder_for` returns the cached
        // placeholder on a `by_value` hit *before* it validates the type, so
        // the second span over a value already mapped — here in one text, but
        // equally any value seeded from an earlier turn of a session — reaches
        // `mask_all` with its `entity_type` unexamined. This is an ordinary
        // 200-OK request, and without the check in `Record::detected` the
        // detector's string would be a key in the evidence file.
        let detector = detector_returning(json!([
            {"entity_type": "PERSON", "start": 0, "end": 5, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
            {"entity_type": "Weber, Hauptstrasse 4", "start": 10, "end": 15, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
        ]))
        .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());

        let (status, _) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber und Weber"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the mapping accepted the request");

        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(
            !text.contains(SECRET),
            "a detector's type name reached the journal on the ordinary path: {text}"
        );
        let lines = journal(&path);
        assert_eq!(lines[0]["types"]["PERSON"], 1);
        assert_eq!(
            lines[0]["types"]["unvalidated"], 1,
            "the type that skipped the mapping's check is counted, not quoted"
        );
    }

    #[tokio::test]
    async fn a_request_without_a_session_still_has_a_tenant() {
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        call_with_headers(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "hello"}]}),
            &[("authorization", "sk-tenant")],
        )
        .await;

        let lines = journal(&path);
        assert!(lines[0]["tenant"].is_string());
        assert!(lines[0]["session"].is_null());
        assert_eq!(
            lines[0]["tenant"], lines[1]["tenant"],
            "both lines of one request name the same tenant"
        );
    }

    #[tokio::test]
    async fn a_request_refused_before_masking_still_says_whose_it_was() {
        // The claim the attribution block makes by sitting above the shape
        // check. A refusal this early leaves one line and no `masked` line to
        // join to, so if that line has no `tenant` the request is attributable
        // to nobody — which is the first thing anyone reading a run of
        // refusals wants to know.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;

        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, _) = call_with_headers(
            Arc::clone(&state),
            "/v1/chat/completions",
            json!({"messages": "wrong"}),
            &[("authorization", "sk-tenant")],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // The same credential again, refused one step later for its session id
        // — still before anything was masked.
        let (status, _) = call_with_headers(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "hello"}]}),
            &session_headers("sk-tenant", "not a valid id!"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let lines = journal(&path);
        assert_eq!(
            lines.len(),
            2,
            "neither request was masked, so one line each"
        );
        assert_eq!(lines[0]["error"], "shape_request");
        assert_eq!(lines[1]["error"], "session_bad_id");
        let tenant = lines[0]["tenant"]
            .as_str()
            .expect("a refusal before masking still names its tenant");
        assert_eq!(tenant.len(), 32);
        assert_eq!(
            lines[0]["tenant"], lines[1]["tenant"],
            "one credential is one tenant, however the request was refused"
        );
        assert!(
            lines[1]["session"].is_null(),
            "an id the gateway rejected is not an identity it records"
        );
    }

    #[tokio::test]
    async fn the_journal_never_carries_the_submitted_value() {
        let detector = detector_finding_weber().await;
        // The provider echoes the placeholder back inside an error body, which
        // is restored to the real value on the way out — the one path where a
        // value exists on the response side too.
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_json(json!({"error": {"message": "[PERSON_1] is rate limited"}})),
            )
            .mount(&upstream)
            .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(
            body.contains(SECRET),
            "the client does get the restored value"
        );

        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(!text.contains(SECRET), "the journal does not");
        assert!(!text.contains("PERSON_1"), "nor a placeholder name");
    }

    #[tokio::test]
    async fn the_journal_counts_spans_found_in_tool_traffic() {
        // The evidence layer must not get weaker because the personal data
        // arrived in an argument rather than in a message. The counts come
        // from `count_distinct`, and the `Json` arm has to call it for the
        // same reason the `Text` arm does — a slot kind is not a reason for a
        // request to go unrecorded.
        let detector = detector_returning(person_span()).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let body = json!({
            "model": "claude",
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "read_file",
                 "input": {"path": SECRET}}
            ]}]
        });
        let (status, _) = call(state, "/v1/messages", body).await;
        assert_eq!(status, StatusCode::OK);

        let lines = journal(&path);
        let masked = lines
            .iter()
            .find(|line| line["event"] == "masked")
            .expect("a masked record");
        assert_eq!(masked["types"]["PERSON"], 1);
        assert_eq!(
            masked["spans"], 1,
            "one occurrence, counted where the argument was: {masked}"
        );
        let text = serde_json::to_string(masked).unwrap();
        assert!(
            !text.contains(SECRET),
            "the journal never carries a value: {text}"
        );
        assert!(
            !text.contains("path"),
            "the journal never carries a key name either — an argument's key is \
             the client's own vocabulary and can name the person in it: {text}"
        );
    }

    #[tokio::test]
    async fn a_refused_tool_request_leaves_one_line_that_quotes_none_of_it() {
        // The other half of the same promise. A request refused before the
        // provider is called leaves a single outcome line, and it answers both
        // questions on its own: whether bytes left, and what class of failure
        // it was. `error` is drawn from a fixed vocabulary rather than
        // formatted, so the key name the caller chose and the digits it held
        // cannot reach the file through it — which is the whole reason the
        // vocabulary is fixed.
        let detector = detector_returning(json!([
            {"entity_type": "CREDIT_CARD", "start": 0, "end": 16}
        ]))
        .await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, _) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "settle",
                     "input": {"cardholder_pan": 4_111_111_111_111_111u64}}
                ]}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "nothing left, so there is nothing to record having left"
        );

        let lines = journal(&path);
        assert_eq!(lines.len(), 1, "a refusal has no masked line to join to");
        assert_eq!(lines[0]["event"], "outcome");
        assert_eq!(lines[0]["result"], "refused");
        assert_eq!(lines[0]["upstream"], false);
        assert_eq!(lines[0]["status"], 400);
        assert_eq!(
            lines[0]["error"], "tool_numeric_personal_data",
            "and it names the class, so a refusal is legible without the body"
        );

        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(
            !text.contains("4111"),
            "the value the request was refused over is the last thing that \
             belongs in the record of refusing it: {text}"
        );
        assert!(
            !text.contains("cardholder_pan"),
            "nor the key naming it: {text}"
        );
    }

    #[tokio::test]
    async fn a_whole_stream_records_completed() {
        let detector = detector_finding_weber().await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, _) = call(
            state,
            "/v1/chat/completions",
            json!({"stream": true, "messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let lines = journal(&path);
        assert_eq!(
            lines[0]["stream"], true,
            "the masked record knows it is a stream"
        );
        assert_eq!(lines[1]["result"], "completed");
        assert_eq!(lines[1]["status"], 200);
        assert_eq!(
            lines[1]["upstream"], true,
            "bytes did leave before the stream finished"
        );
    }

    #[tokio::test]
    async fn an_unrestorable_token_records_stream_failed() {
        // The provider invents a placeholder no mapping knows. Bytes have
        // already gone out, so the stream ends rather than the request being
        // refused — and the record says so with the status the client got.
        let detector = detector_finding_weber().await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"[PERSON_9]\"}}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, body) = call(
            state,
            "/v1/chat/completions",
            json!({"stream": true, "messages": [{"role": "user", "content": "Weber called"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the head was already sent");
        assert!(
            body.contains("error"),
            "the client is told the stream failed"
        );

        let lines = journal(&path);
        assert_eq!(lines[1]["result"], "stream_failed");
        assert_eq!(lines[1]["status"], 200);
        assert_eq!(lines[1]["error"], "stream_unrestorable");
        assert_eq!(lines[1]["upstream"], true);
    }

    /// Build a streaming request and hand back the unread `Response` — the
    /// body's generator has not been polled once, so nothing in it has run.
    async fn streamed_response(state: Arc<AppState>) -> Response {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"stream": true, "messages": [{"role": "user", "content": "hello"}]})
                    .to_string(),
            ))
            .unwrap();
        router(state).oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn the_stream_finishes_the_record_not_the_wrapper() {
        // If the wrapper finalized a streamed request, the outcome line would
        // already exist the instant `handle` returns — before the body is
        // drained, let alone a single event restored.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let response = streamed_response(state).await;
        assert_eq!(response.status(), StatusCode::OK);

        assert_eq!(
            journal(&path).len(),
            1,
            "only the masked line exists before the body is ever read"
        );

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("[DONE]"));

        let lines = journal(&path);
        assert_eq!(lines.len(), 2, "exactly one outcome, written once");
        assert_eq!(lines[1]["event"], "outcome");
        assert_eq!(lines[1]["result"], "completed");
    }

    #[tokio::test]
    async fn a_dropped_stream_is_recorded_as_aborted() {
        // The client vanished before the stream ever ran: no upstream break,
        // no restoration failure, no success either. None of
        // `restore_stream`'s three signalling exits fires, so the record must
        // not assume success just because nothing else was said.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let response = streamed_response(state).await;
        assert_eq!(response.status(), StatusCode::OK);

        // Neither `serve` nor the generator ever calls `completed`: the
        // wrapper's own handle is already gone (`serve` returned), and the
        // generator inside this unread body has not run a single statement.
        drop(response);

        let lines = journal(&path);
        assert_eq!(lines.len(), 2, "the dropped handle still writes its line");
        assert_eq!(lines[1]["result"], "aborted");
    }

    #[tokio::test]
    async fn a_stream_dropped_after_its_first_yield_still_records_the_failure() {
        // `restorer.push` fails on this body's very first event, so the error
        // arm's `record.stream_failed(...)` is the first thing the generator
        // ever does — and the `error_event` bytes it renders afterwards are
        // its first `yield`. A single poll drives the generator exactly to
        // that `yield` and no further: dropping the stream there proves the
        // signal ran before it, not after. With the signal placed after the
        // `yield` instead, a generator parked there and dropped never reaches
        // it, and the outcome falls back to `aborted` — the exact bug this
        // test exists to catch.
        use futures_util::StreamExt;

        let detector = detector_finding_weber().await;
        let upstream = upstream_streaming(
            "/v1/chat/completions",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"[PERSON_9]\"}}]}\n\n\
             data: [DONE]\n\n",
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let response = streamed_response(state).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the head was already sent"
        );

        let mut body = response.into_body().into_data_stream();
        let first = body.next().await;
        assert!(first.is_some(), "the error event is the first thing sent");
        drop(body);

        let lines = journal(&path);
        assert_eq!(lines.len(), 2, "the dropped handle still writes its line");
        assert_eq!(lines[1]["result"], "stream_failed");
    }

    #[tokio::test]
    async fn a_stream_dropped_after_the_upstream_breaks_still_records_the_failure() {
        // The same shape as the restoration-failure case above, on the other
        // exit that reorders a signal ahead of its `yield`: the connection
        // breaks before a single body byte arrives, so the break is caught on
        // the first read and `record.stream_failed("stream_broken")` is the
        // first thing the generator does. Its `error_event` yield is the
        // first `yield` at all, so one poll parks the generator exactly
        // there.
        use futures_util::StreamExt;

        let detector = detector_returning(json!([])).await;
        let base = truncating_upstream("").await;
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("audit.jsonl");
        let audit = Arc::new(crate::audit::Audit::open(&path).expect("opens"));
        let state = Arc::new(AppState {
            detector: DetectorClient::new(detector.uri(), Duration::from_secs(5), 16, UNCAPPED),
            upstream: reqwest::Client::new(),
            openai_base: base.clone(),
            anthropic_base: base,
            sessions: SessionStore::new(test_limits()),
            audit,
            max_tool_chars: TEST_MAX_TOOL_CHARS,
            max_tool_calls: TEST_MAX_TOOL_CALLS,
        });

        let response = streamed_response(state).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the head was already sent"
        );

        let mut body = response.into_body().into_data_stream();
        let first = body.next().await;
        assert!(first.is_some(), "the error event is the first thing sent");
        drop(body);

        let lines = journal(&path);
        assert_eq!(lines.len(), 2, "the dropped handle still writes its line");
        assert_eq!(lines[1]["result"], "stream_failed");
        assert_eq!(lines[1]["error"], "stream_broken");
    }

    #[tokio::test]
    async fn health_answers_without_a_credential() {
        // An orchestrator has no API key and must still be able to ask.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("routed");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_does_not_drive_the_detector() {
        // An unauthenticated endpoint that reached the detector on request
        // would be a way to run detection without a credential — and a
        // detector outage is a per-request refusal by design, not a reason to
        // call this gateway unhealthy.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());

        router(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("routed");

        assert!(
            detector
                .received_requests()
                .await
                .expect("recorded")
                .is_empty(),
            "health must not call the detector"
        );
    }

    #[tokio::test]
    async fn health_writes_no_audit_record() {
        // A liveness probe runs every few seconds forever. Journaling it would
        // bury the evidence under lines about nothing.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/chat/completions", json!({})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());

        router(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("routed");

        let journal = std::fs::read_to_string(&path).expect("readable");
        assert!(journal.is_empty(), "health wrote to the journal: {journal}");
    }

    #[tokio::test]
    async fn a_structured_dispatch_value_never_reaches_the_upstream() {
        // D1 end to end. `Admits::Dispatch` shared `known_value`'s first arm
        // with `Described`, `Elsewhere` and `Unscanned`, and that arm is
        // `Ok(())`, so a structured value under a dispatch field was admitted,
        // scanned by nothing and forwarded verbatim. Measured before the fix:
        //
        //   status=200 upstream saw: {"messages":[{"content":"hi","role":"user"}],
        //   "model":"claude","tools":[{"input_schema":{"type":"object"},
        //   "name":{"owner":"Martina Weber"}}]}
        //
        // The argument for leaving dispatch alone was about the character set.
        // An object has no characters for it to be about.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": {"owner": "Martina Weber"},
                           "input_schema": {"type": "object"}}]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{returned}");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "and the name never left the process"
        );
        assert!(
            !returned.contains("Weber"),
            "the refusal must not repeat what it refused: {returned}"
        );
    }

    #[tokio::test]
    async fn a_tool_choice_field_no_slot_addresses_never_reaches_the_upstream() {
        // D2 end to end. `tool_choice` is admitted by *absence from a denylist*
        // rather than by presence on an allowlist, which is why the sweep over
        // the allowlists could not see it. Measured before the fix:
        //
        //   status=200 upstream saw: {..., "tool_choice":{"note":"Martina Weber",
        //   "type":"auto"}, "tools":[{"input_schema":{"type":"object"},"name":"t"}]}
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "t", "input_schema": {"type": "object"}}],
                "tool_choice": {"type": "auto", "note": "Martina Weber"}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{returned}");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "and the note never left the process"
        );
        assert!(
            !returned.contains("Weber"),
            "the refusal must not repeat what it refused: {returned}"
        );
    }

    #[tokio::test]
    async fn an_openai_tool_choice_field_no_slot_addresses_never_reaches_the_upstream() {
        // The same hole on the other provider, and the shape OpenAI's
        // `tool_choice` has that Anthropic's does not — the value is either a
        // bare mode string or an object — is why this check lives at the use
        // site rather than in `Admits`. Measured before the fix:
        //
        //   status=200 upstream saw: {..., "tool_choice":{"function":{"name":"t"},
        //   "note":"Martina Weber","type":"function"}, ...}
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/chat/completions",
            json!({
                "model": "gpt",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"type": "function", "function": {"name": "t", "parameters": {}}}],
                "tool_choice": {"type": "function", "note": "Martina Weber",
                                "function": {"name": "t"}}
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{returned}");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "and the note never left the process"
        );
        assert!(
            !returned.contains("Weber"),
            "the refusal must not repeat what it refused: {returned}"
        );
    }

    #[tokio::test]
    async fn a_bare_mode_string_still_forces_a_tool_on_openai() {
        // The other direction, and the one a new check breaks silently:
        // `"tool_choice": "required"` is a legal value OpenAI publishes and
        // Anthropic does not. A shape check written for the object form alone
        // would refuse a working request, which is the failure mode the whole
        // branch has been avoiding on the other side.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}),
        )
        .await;
        let (state, _dir, _path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/chat/completions",
            json!({
                "model": "gpt",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"type": "function", "function": {"name": "t", "parameters": {}}}],
                "tool_choice": "required"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        assert_eq!(
            sent_to(&upstream).await["tool_choice"],
            json!("required"),
            "and it reached the upstream unchanged"
        );
    }

    #[tokio::test]
    async fn an_anthropic_tool_block_in_an_openai_message_never_reaches_the_upstream() {
        // The end-to-end half of the provider-level test, at the only boundary
        // that settles it: what left the process. Measured at 6f22649, this
        // body was a 200 and `Martina Weber` was in the egress as a `tool_use`
        // block's `name` — a dispatch field, so nothing scanned it and the
        // detector was never shown it.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok", "role": "assistant"}}]}),
        )
        .await;
        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/chat/completions",
            json!({"model": "gpt", "messages": [{"role": "user", "content": [
                {"type": "tool_use", "id": "x", "name": "Martina Weber", "input": {}}
            ]}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "refused means the upstream is never called"
        );

        // And the same block on the provider that defines it is served, which
        // is what makes this scoping rather than a new refusal. A tool call's
        // arguments are the traffic this slice exists to mask.
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (status, returned) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": [
                {"type": "tool_use", "id": "x", "name": "read_file", "input": {"path": "Martina Weber"}}
            ]}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        assert_eq!(
            sent_to(&upstream).await["messages"][0]["content"][0]["input"]["path"],
            "Martina Weber",
            "scanned, and this detector found nothing in it"
        );
    }

    #[tokio::test]
    async fn a_tool_block_in_an_anthropic_system_prompt_never_reaches_the_upstream() {
        // The context half. `system` takes text blocks and nothing else, and
        // the shared walk admitted tool blocks there: measured at 6f22649 as a
        // 200 with the name in the egress.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (status, _) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude",
                   "system": [{"type": "tool_use", "id": "x", "name": "Martina Weber", "input": {}}],
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "refused means the upstream is never called"
        );

        // A system prompt written the way Anthropic documents it still works.
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (status, returned) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude",
                   "system": [{"type": "text", "text": "you are a bot"}],
                   "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        assert_eq!(
            sent_to(&upstream).await["system"][0]["text"],
            "you are a bot"
        );
    }

    #[tokio::test]
    async fn a_malformed_applicator_is_shown_to_the_detector_and_masked() {
        // The other P1, at the same boundary. `{"allOf": {"type": "Martina
        // Weber"}}` is malformed — `allOf` is an array — and the arm carried
        // the schema shape into it anyway, so `type`'s value was skipped one
        // level down as a type name. Measured at 6f22649: 200, and the
        // detector's only call was for `"hi"`.
        let detector = detector_finding_martina().await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (status, returned) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "hi"}],
                   "tools": [{"name": "t", "input_schema": {"allOf": {"type": "Martina Weber"}}}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        assert_eq!(
            sent_to(&upstream).await["tools"][0]["input_schema"]["allOf"]["type"],
            "Martina [PERSON_1]",
            "the value was never shown to the detector"
        );

        // **The half this round corrected.** It read `{"allOf": [{"type":
        // "Martina Weber"}]}` and asserted the name reached the provider
        // verbatim, under the comment "a real applicator's identifiers are
        // still identifiers". The applicator is real and the identifier is
        // not: JSON Schema permits seven values for `type` and this is none of
        // them, so the document states no type and the container it sits in
        // cannot make it one. Codex found the leak by reading the test that
        // asserted it.
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (status, returned) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "hi"}],
                   "tools": [{"name": "t", "input_schema": {"allOf": [{"type": "Martina Weber"}]}}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        assert_eq!(
            sent_to(&upstream).await["tools"][0]["input_schema"]["allOf"][0]["type"],
            "Martina [PERSON_1]",
            "a well-formed container does not make its contents an identifier"
        );

        // And the twin that is still worth pinning, which is what the sentence
        // above was reaching for: a real applicator holding a real identifier.
        // `required` holds property names, the caller chooses those freely, and
        // one that reads like a person's name is dispatch and is forwarded —
        // this branch's recorded position, unchanged by this round.
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (status, returned) = call(
            state(&detector, &upstream),
            "/v1/messages",
            json!({"model": "claude", "messages": [{"role": "user", "content": "hi"}],
                   "tools": [{"name": "t", "input_schema":
                              {"allOf": [{"required": ["Martina Weber"], "type": "object"}]}}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{returned}");
        assert_eq!(
            sent_to(&upstream).await["tools"][0]["input_schema"]["allOf"][0]["required"][0],
            "Martina Weber",
            "a real applicator's identifiers are still identifiers"
        );
    }

    #[tokio::test]
    async fn a_partially_checked_identifier_reaches_the_detector_at_the_boundary() {
        // **Round 9, and all three are inside round 8's own fix.** That round
        // asked "does the document state the thing the rule exempts" of the
        // container and then of the contents, and checked the contents
        // *partially*: a prefix (`contentMediaType`'s essence, with the
        // parameters named as unread in its own report), a union too wide
        // (`$dynamicAnchor` given 2019-09's grammar, from a draft that does not
        // define the keyword), and an argument asserted rather than measured
        // (`pattern`, on "every string is a valid regex" — `Martina Weber(` is
        // an unterminated group and ECMA-262 rejects it).
        //
        // Each pair below is the leak and the control it must not break, and
        // the controls are the point: a check stricter than the draft masks a
        // working schema, visibly, at the caller.
        let detector = detector_finding_weber().await;
        for (leaked, control, note) in [
            (
                json!({"pattern": "Weber("}),
                json!({"pattern": "^Weber$"}),
                "a regular expression has to close its groups, and one that spells a \
                 person out is still the caller's own expression",
            ),
            (
                json!({"contentMediaType": "text/plain; Weber"}),
                json!({"contentMediaType": "text/plain; charset=Weber"}),
                "a media type's parameters are `token=value`, and a bare word is neither \
                 half of one",
            ),
            (
                json!({"$dynamicAnchor": "Weber:node"}),
                json!({"$anchor": "Weber:node"}),
                "2019-09 permits `:` in an anchor and 2020-12 does not, and only 2020-12 \
                 defines `$dynamicAnchor` at all",
            ),
        ] {
            let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
            let (status, returned) = call(
                state(&detector, &upstream),
                "/v1/messages",
                json!({"model": "claude", "messages": [{"role": "user", "content": "hi"}],
                       "tools": [{"name": "t", "input_schema": leaked}]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{returned}");
            let sent = sent_to(&upstream).await["tools"][0]["input_schema"].clone();
            assert!(
                sent.to_string().contains("[PERSON_1]"),
                "{note}: the value was never shown to the detector, {sent}"
            );

            // The other direction, and the one that costs a working client if
            // it is got wrong.
            let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
            let (status, returned) = call(
                state(&detector, &upstream),
                "/v1/messages",
                json!({"model": "claude", "messages": [{"role": "user", "content": "hi"}],
                       "tools": [{"name": "t", "input_schema": control.clone()}]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{returned}");
            assert_eq!(
                sent_to(&upstream).await["tools"][0]["input_schema"],
                control,
                "{note}: a valid schema was masked, which breaks a client that was working"
            );
        }
    }

    #[tokio::test]
    async fn a_keyword_no_draft_constrains_reaches_the_provider_unmasked() {
        // **Round 10, and the probe runs the other way round for the first
        // time in ten.** Every earlier boundary test here asserts that a value
        // reached the *detector*; these assert that it reached the *provider*
        // untouched, because the failure being reproduced is over-masking. Nine
        // rounds of closing leaks under time pressure produced two checks
        // stricter than the drafts they cite, and a check stricter than the
        // draft breaks a schema that was correct — visibly, at the caller, in
        // the request they just made.
        //
        // Each pair is the over-mask and the near-miss it must not take down
        // with it. The near-misses are the point: loosening a check is only
        // safe if the part of it that was load-bearing survives.
        let detector = detector_finding_weber().await;
        for (forwarded, still_scanned, note) in [
            (
                // 2019-09 §8.2.4.2.1: "MUST be a string which is a
                // URI-reference". It was on no identifier list at all, so this
                // went upstream as `{"$recursiveRef": "[PERSON_1].json"}` — a
                // rewritten reference target, which is a broken schema.
                json!({"$recursiveRef": "Weber.json"}),
                // And the grammar it is now held to still holds: a
                // URI-reference has no space in it, so this states no
                // reference and is the caller's data.
                json!({"$recursiveRef": "Weber Martina"}),
                "2019-09 defines `$recursiveRef` as a URI-reference and the value `\"#\"` \
                 is a rule about evaluation, not about syntax",
            ),
            (
                // 2020-12 §7.1: "It MUST be a string", and §7.2.3 lets an
                // implementation recognise any custom attribute it likes. The
                // one-token rule was inferred from what the registered names
                // look like and went upstream as `{"format": "[PERSON_1]
                // Martina"}`.
                json!({"format": "Weber Martina"}),
                // The sibling that keeps the token rule, and the reason this
                // round did not loosen both: `contentEncoding`'s drafts name
                // RFC 2045 §6.1 and RFC 4648 as where its values come from,
                // and both of those extend through tokens.
                json!({"contentEncoding": "Weber Martina"}),
                "no draft publishes a grammar for a format attribute, and the keyword \
                 whose draft cites one keeps its check",
            ),
        ] {
            let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
            let (status, returned) = call(
                state(&detector, &upstream),
                "/v1/messages",
                json!({"model": "claude", "messages": [{"role": "user", "content": "hi"}],
                       "tools": [{"name": "t", "input_schema": forwarded.clone()}]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{returned}");
            assert_eq!(
                sent_to(&upstream).await["tools"][0]["input_schema"],
                forwarded,
                "{note}: a valid schema was masked, which breaks a client that was working"
            );

            let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
            let (status, returned) = call(
                state(&detector, &upstream),
                "/v1/messages",
                json!({"model": "claude", "messages": [{"role": "user", "content": "hi"}],
                       "tools": [{"name": "t", "input_schema": still_scanned}]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{returned}");
            let sent = sent_to(&upstream).await["tools"][0]["input_schema"].clone();
            assert!(
                sent.to_string().contains("[PERSON_1]"),
                "{note}: loosening the check took its load-bearing half with it, {sent}"
            );
        }
    }

    #[tokio::test]
    async fn a_value_dressed_as_a_type_never_reaches_a_journal_line() {
        // C1, driven the way it is actually reachable: an ordinary 200, no
        // session, the client's own response correct and the provider given
        // `[REDACTED_1]`. The string exists in exactly one place — the evidence
        // file — which is why eleven rounds of probes on response bodies could
        // not see it.
        //
        // The grammar the check used to be is a check on the shouting, not on
        // the value: uppercasing and underscore-joining costs a detector
        // nothing, `MAX_ENTITY_TYPE` is forty characters, and every span of the
        // request is a separate key.
        let detector = detector_returning(json!([
            {"entity_type": "MARTINA_WEBER_HAUPTSTRASSE_VIER", "start": 0, "end": 13,
             "confidence": 1.0, "recognizer": "ner:fake", "tier": 2, "boosted": false}
        ]))
        .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Martina Weber schreibt"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "an ordinary request: {returned}");
        let sent = sent_to(&upstream).await.to_string();
        assert!(
            sent.contains("[REDACTED_1]"),
            "the mapping declined it: {sent}"
        );
        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(
            !text.contains("MARTINA") && !text.contains(SECRET.to_uppercase().as_str()),
            "the detector wrote a submitted value into the evidence: {text}"
        );
        assert_eq!(
            journal(&path)[0]["types"]["unvalidated"],
            1,
            "and the operator is told there was one, without being told what"
        );
    }

    #[tokio::test]
    async fn two_turns_of_one_session_carrying_the_same_traffic_write_the_same_line() {
        // I1. Both turns send `[REDACTED_1] called` to the provider. The
        // journal said `redacted: 1` and then `redacted: 0`, because the count
        // was read off the mapping, which counts *allocations*: turn two's span
        // hit `by_value` and returned before the type was looked at, so nothing
        // was allocated and nothing was counted.
        //
        // A field that describes the request must not depend on what an earlier
        // request happened to have put in the session's table.
        let detector = detector_returning(json!([
            {"entity_type": "WEBER", "start": 0, "end": 5, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
        ]))
        .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        for _ in 0..2 {
            call_with_headers(
                Arc::clone(&state),
                "/v1/chat/completions",
                json!({"messages": [{"role": "user", "content": "Weber called"}]}),
                &session_headers("sk-tenant", "chat-1"),
            )
            .await;
        }

        let sent: Vec<String> = upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| String::from_utf8(request.body.clone()).unwrap())
            .collect();
        assert_eq!(sent[0], sent[1], "the premise: identical traffic");
        let lines = journal(&path);
        let masked: Vec<&Value> = lines
            .iter()
            .filter(|line| line["event"] == "masked")
            .collect();
        // Every field but the two that are supposed to differ. `README.md`
        // says two turns carrying identical traffic write identical lines, and
        // a comparison of one field would have missed the next count to be
        // read off the mapping instead of off the request.
        let strip = |line: &Value| {
            let mut line = line.clone();
            let object = line.as_object_mut().expect("a JSON object");
            object.remove("ts");
            object.remove("request");
            line
        };
        assert_eq!(
            strip(masked[0]),
            strip(masked[1]),
            "identical traffic, two different evidence lines: {} / {}",
            masked[0],
            masked[1]
        );
        assert_eq!(
            masked[1]["redacted"], 1,
            "and the number is the true one: the provider received no WEBER placeholder"
        );
    }

    #[tokio::test]
    async fn a_value_named_twice_says_the_second_name_did_not_reach_the_provider() {
        // I1 without a session at all, and the case that shows what `redacted`
        // has to mean. One value, two spans, two types: only one placeholder
        // can be issued for a value, so `[PERSON_1]` went up twice and no
        // `WEBER` placeholder was ever issued. Reading `types` alone the line
        // claims two types were found and both were honoured.
        let detector = detector_returning(json!([
            {"entity_type": "PERSON", "start": 0, "end": 5, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
            {"entity_type": "ORG", "start": 10, "end": 15, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
        ]))
        .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber und Weber"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{returned}");
        let sent = sent_to(&upstream).await.to_string();
        assert!(
            sent.contains("[PERSON_1] und [PERSON_1]") && !sent.contains("[ORG_"),
            "the premise: one placeholder for one value: {sent}"
        );
        let line = &journal(&path)[0];
        assert_eq!(line["types"]["PERSON"], 1);
        assert_eq!(line["types"]["ORG"], 1, "both types are declared and named");
        assert_eq!(
            line["redacted"], 1,
            "one of the two names did not reach the provider and the line must say so: {line}"
        );
    }

    #[tokio::test]
    async fn a_number_the_exemption_forwards_is_journalled_as_forwarded() {
        // I2. The one place this gateway deliberately does not mask a detected
        // span was the one place the journal had no field for: the line read
        // `spans: 1`, `types: {"PERSON": 1}`, `redacted: 0` — which is what a
        // *masked* value looks like — while the digits went to the provider
        // unchanged. The forwarding is a ruling of this branch; the evidence
        // recording a pseudonymization that did not happen was not.
        let detector = detector_returning(json!([
            {"entity_type": "PERSON", "start": 0, "end": 16, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
        ]))
        .await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "pay",
                     "input": {"card": 4_111_111_111_111_111u64}}
                ]}]
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{returned}");
        assert_eq!(
            sent_to(&upstream).await["messages"][0]["content"][0]["input"]["card"],
            json!(4_111_111_111_111_111u64),
            "the premise: the number reaches the provider as the client wrote it"
        );
        let line = &journal(&path)[0];
        assert_eq!(line["types"]["PERSON"], 1, "the finding is still reported");
        assert_eq!(
            line["forwarded"], 1,
            "the line claims a masking that did not happen: {line}"
        );
        assert_eq!(
            line["redacted"], 0,
            "forwarded and redacted are different fates and each occurrence has one: {line}"
        );
        // The message above said "each *finding* has one" when this test was
        // written, and round 2 of this review is the counterexample: a value
        // that is a number in one leaf and a string in another has both fates
        // at once. Corrected here rather than deleted — the request this test
        // drives has one occurrence, so its numbers are the same under either
        // reading, and what was wrong was the sentence.
    }

    #[tokio::test]
    async fn a_value_forwarded_in_one_leaf_and_masked_in_another_is_counted_as_both() {
        // The partition used to be over findings, and a finding does not have
        // one fate. `9007199254740991` is `Number.MAX_SAFE_INTEGER`, and this
        // repo's own tool payload writes it twice, as the `maximum` of `limit`
        // and of `offset`; a `description` that states the bound holds the same
        // characters as prose. So one value, three occurrences: two numeric
        // leaves the exemption forwards, one string leaf masked as
        // `[PERSON_1]`. Measured before the fix, at 200:
        //
        //   {"documents":1,"forwarded":1,"redacted":0,"spans":3,"texts":1,
        //    "types":{"PERSON":1}}
        //
        // Read against the partition that line documented — one finding, in
        // exactly one of three states, and `forwarded` names it — it says the
        // provider received the digits and no `[PERSON_1]`. It received both,
        // and its arithmetic left two occurrences claiming to be placeholders
        // when one was.
        let detector = detector_finding_the_max_safe_integer(json!([
            {"entity_type": "PERSON", "start": 0, "end": 16, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
            {"entity_type": "PERSON", "start": 18, "end": 34, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
            {"entity_type": "PERSON", "start": 36, "end": 52, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
        ]))
        .await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "user", "content": "hallo"}],
                "tools": [{"name": "page", "input_schema": {
                    "type": "object",
                    "description": "9007199254740991",
                    "properties": {
                        "limit": {"type": "integer", "maximum": 9_007_199_254_740_991u64},
                        "offset": {"type": "integer", "maximum": 9_007_199_254_740_991u64},
                    }
                }}]
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{returned}");
        let schema = sent_to(&upstream).await["tools"][0]["input_schema"].clone();
        assert_eq!(
            schema["description"], "[PERSON_1]",
            "the premise: the string leaf went up under its own name: {schema}"
        );
        assert_eq!(
            schema["properties"]["offset"]["maximum"],
            json!(9_007_199_254_740_991u64),
            "the premise: the numeric leaves went up verbatim: {schema}"
        );
        let line = &journal(&path)[0];
        assert_eq!(line["spans"], 3, "three occurrences of one value: {line}");
        assert_eq!(line["types"]["PERSON"], 1, "one finding, under one name");
        assert_eq!(
            line["forwarded"], 2,
            "both numeric occurrences went verbatim and the line counts \
             occurrences, not values: {line}"
        );
        assert_eq!(
            line["redacted"], 0,
            "no occurrence went up under a name that was not its own: {line}"
        );
        assert_eq!(
            residual(line),
            1,
            "exactly one occurrence reached the provider as [PERSON_1], and a \
             line claiming two would be claiming a masking of the digits: {line}"
        );
    }

    #[tokio::test]
    async fn a_value_forwarded_in_one_leaf_can_be_redacted_in_another() {
        // Why a single "mixed" counter would not have been the fix. A finding's
        // fate is not one of {forwarded, named} plus a "both" — it is any
        // non-empty *subset* of three states, and this is the third pairing:
        // `ORG` takes the value's placeholder first, so the `PERSON` span on
        // the string leaf is masked under a name that is not `PERSON`, while
        // the `PERSON` span on the numeric leaf is forwarded. One counter
        // called "mixed" would read the same here as in the test above, and an
        // auditor could not tell the line where `[PERSON_1]` reached the
        // provider from the line where `[ORG_1]` and bare digits did.
        //
        // Measured before the fix, at 200: {"forwarded":1,"redacted":0,
        // "spans":3,"types":{"ORG":1,"PERSON":1}} — the redacted occurrence
        // missing from the evidence entirely.
        let detector = detector_finding_the_max_safe_integer(json!([
            {"entity_type": "ORG", "start": 0, "end": 16, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
            {"entity_type": "PERSON", "start": 18, "end": 34, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
            {"entity_type": "PERSON", "start": 36, "end": 52, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
        ]))
        .await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "user", "content": "hallo"}],
                "tools": [{"name": "page", "input_schema": {
                    "type": "object",
                    "description": "9007199254740991",
                    "properties": {"limit": {"type": "integer",
                                             "description": "9007199254740991",
                                             "maximum": 9_007_199_254_740_991u64}}
                }}]
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{returned}");
        let schema = sent_to(&upstream).await["tools"][0]["input_schema"].clone();
        assert_eq!(
            schema["properties"]["limit"]["description"], "[ORG_1]",
            "the premise: the PERSON span was masked under ORG's placeholder: {schema}"
        );
        let line = &journal(&path)[0];
        assert_eq!(line["spans"], 3, "three occurrences: {line}");
        assert_eq!(
            line["forwarded"], 1,
            "the numeric occurrence went verbatim: {line}"
        );
        assert_eq!(
            line["redacted"], 1,
            "a forwarded value can also be masked under another name, and the \
             line has to say so: {line}"
        );
        assert_eq!(
            residual(line),
            1,
            "one occurrence, the first, went up under the name it was found \
             under: {line}"
        );
    }

    #[tokio::test]
    async fn two_texts_the_cache_answers_once_are_two_detections_asked_for() {
        // The sweep's own finding. `texts` and `documents` were documented in
        // three places as counts of *detector calls*, beside a README sentence
        // that had been corrected to "detections asked for" in the same commit
        // because the cache falsifies the stronger claim. This is the request
        // that falsifies it: one credential, two identical messages, two texts
        // in the evidence and one request at the detector's door.
        let detector = detector_returning_expecting(json!([]), Some(1)).await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call_with_headers(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "wer"},
                                {"role": "user", "content": "wer"}]}),
            &session_headers("sk-tenant", "chat-1"),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{returned}");
        let line = &journal(&path)[0];
        assert_eq!(
            line["texts"], 2,
            "the request asked for two detections, whoever served them: {line}"
        );
        assert_eq!(
            detector.received_requests().await.expect("recorded").len(),
            1,
            "the premise: the cache answered the second one"
        );
    }

    #[tokio::test]
    async fn a_line_accounts_for_every_span_it_reports() {
        // `redacted` counting findings and `spans` counting occurrences left
        // the two incommensurable: a value masked three times as `[REDACTED_1]`
        // reported `spans: 3, redacted: 1`, and the other two occurrences were
        // in no state at all. They are three now, and the line's own arithmetic
        // closes.
        let detector = detector_returning(json!([
            {"entity_type": "WEBER", "start": 0, "end": 5, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
            {"entity_type": "WEBER", "start": 10, "end": 15, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
            {"entity_type": "WEBER", "start": 20, "end": 25, "confidence": 1.0,
             "recognizer": "ner:fake", "tier": 2, "boosted": false},
        ]))
        .await;
        let upstream = upstream_returning(
            "/v1/chat/completions",
            json!({"choices": [{"message": {"content": "ok"}}]}),
        )
        .await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/chat/completions",
            json!({"messages": [{"role": "user", "content": "Weber und Weber und Weber"}]}),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{returned}");
        let sent = sent_to(&upstream).await.to_string();
        assert!(
            sent.contains("[REDACTED_1] und [REDACTED_1] und [REDACTED_1]"),
            "the premise: three occurrences, none under the detector's own name: {sent}"
        );
        let line = &journal(&path)[0];
        assert_eq!(line["spans"], 3);
        assert_eq!(line["types"]["unvalidated"], 1, "one finding: {line}");
        assert_eq!(
            line["redacted"], 3,
            "three occurrences reached the provider under a name that was not \
             the detector's: {line}"
        );
        assert_eq!(
            residual(line),
            0,
            "nothing went up under the name it was found under: {line}"
        );
    }

    #[tokio::test]
    async fn a_document_the_detector_was_never_shown_is_not_counted_as_a_text() {
        // M1. `texts` was `slots.len()`, and since tool traffic a slot may be a
        // document: at most one detector call however many leaves it holds, and
        // **zero** calls when it holds none. Two empty schemas were two texts in
        // the evidence and nothing at all in the detector's log.
        //
        // What that costs is not tidiness. Round 1's H2 on this branch was "the
        // schema document produced no leaves at all, so Weber was never scanned
        // by anything", and the journal is the artifact that should expose a
        // recurrence — while it was recording the un-scanned document as a
        // scanned text.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let (status, returned) = call(
            state,
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "user", "content": "hallo"}],
                "tools": [
                    {"name": "a", "description": "da", "input_schema": {}},
                    {"name": "b", "description": "db", "input_schema": {}},
                ]
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{returned}");
        // Every text here is distinct, so nothing is served from the detection
        // cache and the mock's own count is the number of detections asked for.
        let calls = detector.received_requests().await.unwrap().len();
        let line = &journal(&path)[0];
        assert_eq!(
            line["texts"].as_u64().expect("a count") + line["documents"].as_u64().expect("a count"),
            calls as u64,
            "the journal counts what the detector was shown, and it was shown {calls}: {line}"
        );
        assert_eq!(
            line["documents"], 0,
            "neither schema was scanned, so neither is evidence of a scan: {line}"
        );
        assert_eq!(line["texts"], 3, "one message and two descriptions: {line}");
    }

    #[tokio::test]
    async fn a_schema_of_identifiers_alone_is_a_document_nobody_scanned() {
        // M1's realistic shape, and the reason it is graded above a naming
        // quibble: a schema made only of identifier-valued keywords —
        // `$schema`, `$id`, `$ref`, `type`, `required`, which the last three
        // rounds made a *common* shape rather than a contrived one — has no
        // leaves at all. `json_leaves` yields nothing, no detector call is
        // made, and the schema is forwarded verbatim.
        //
        // Measured with the old count in place: `texts: 2` against **one**
        // detector call. The evidence disagreed with the detector's own
        // traffic, which is the same class of defect as a forwarded numeric
        // leaf journalled as a masking.
        //
        // Driven through the session branch deliberately. Both branches of
        // `handle` build their line from the same `Detected`, and this is the
        // one that used to be able to differ.
        let detector = detector_returning(json!([])).await;
        let upstream = upstream_returning("/v1/messages", json!({"content": []})).await;
        let (state, _dir, path) = state_with(&detector, &upstream, test_limits());
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.test/lookup.json",
            "$ref": "#/$defs/Query",
            "type": "object",
            "required": ["query"]
        });
        let (status, returned) = call_with_headers(
            Arc::clone(&state),
            "/v1/messages",
            json!({
                "model": "claude",
                "messages": [{"role": "user", "content": "hallo"}],
                "tools": [{"name": "lookup", "input_schema": schema.clone()}]
            }),
            &[("x-api-key", "sk-tenant"), (SESSION_HEADER, "chat-1")],
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{returned}");
        assert_eq!(
            sent_to(&upstream).await["tools"][0]["input_schema"],
            schema,
            "the premise: nothing in this schema is data, so nothing is masked"
        );
        let calls = detector.received_requests().await.unwrap().len();
        assert_eq!(calls, 1, "the message, and nothing for the schema");
        let line = &journal(&path)[0];
        assert_eq!(
            line["documents"], 0,
            "a document that made no detector call is not evidence of a scan: {line}"
        );
        assert_eq!(
            line["texts"].as_u64().expect("a count") + line["documents"].as_u64().expect("a count"),
            calls as u64,
            "the journal and the detector's own traffic disagree: {line}"
        );
    }

    #[test]
    fn a_shape_failure_of_the_upstream_is_not_one_of_our_own() {
        // I3. `Shape(_)` collapsed `Response` and `Pointer` into one class, so
        // an auditor reading a run of 502s could not separate "the upstream
        // sent a body we could not read" from "a pointer this gateway produced
        // did not resolve" — the second is a defect here, and it is the one
        // worth paging somebody about.
        let response = ProxyError::Shape(ShapeError::Response("choices"));
        let pointer = ProxyError::Shape(ShapeError::Pointer("/messages/0".to_owned()));
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(pointer.status(), StatusCode::BAD_GATEWAY);
        assert_ne!(
            response.audit_class(),
            pointer.audit_class(),
            "one status, two causes, and the journal is the only place they differ"
        );
    }

    /// Every error this gateway can write a class for. One list, used by the
    /// two tests below: a variant added to any of the wrapped enums already
    /// fails to compile in `audit_class`, and this is what makes it also fail
    /// to be *documented* and to be distinct.
    fn every_audit_class() -> Vec<&'static str> {
        vec![
            ProxyError::Shape(ShapeError::Request("messages")).audit_class(),
            ProxyError::Shape(ShapeError::Response("choices")).audit_class(),
            ProxyError::Shape(ShapeError::Pointer("/a".to_owned())).audit_class(),
            ProxyError::Shape(ShapeError::Unsupported("openai", "logprobs")).audit_class(),
            ProxyError::Shape(ShapeError::MalformedDocument("openai", "/a".to_owned()))
                .audit_class(),
            ProxyError::Detector(DetectorError::Transport("refused".to_owned())).audit_class(),
            ProxyError::Detector(DetectorError::Status(503)).audit_class(),
            ProxyError::Mapping(MappingError::Unknown("[PERSON_1]".to_owned())).audit_class(),
            ProxyError::Mapping(MappingError::BadSpan("overlapping")).audit_class(),
            ProxyError::Mapping(MappingError::TooDeep).audit_class(),
            ProxyError::Mapping(MappingError::TooLarge).audit_class(),
            ProxyError::Mapping(MappingError::MaskCountMismatch("walks")).audit_class(),
            ProxyError::Mapping(MappingError::PlaceholderKey("[PERSON_1]".to_owned()))
                .audit_class(),
            ProxyError::Mapping(MappingError::Unrestorable("two members of the same name"))
                .audit_class(),
            ProxyError::Upstream("reset".to_owned()).audit_class(),
            ProxyError::Session(SessionError::BadId).audit_class(),
            ProxyError::Session(SessionError::Disabled).audit_class(),
            ProxyError::Session(SessionError::NoCredential("authorization")).audit_class(),
            ProxyError::Session(SessionError::Saturated).audit_class(),
            ProxyError::Audit(crate::audit::AuditError::Unavailable).audit_class(),
            ProxyError::ToolTooLarge.audit_class(),
            ProxyError::TooManyToolCalls.audit_class(),
            ProxyError::NumericPersonalData.audit_class(),
        ]
    }

    #[test]
    fn every_error_this_gateway_records_has_a_class_of_its_own() {
        // The other half of I3, and what makes the wildcard's absence visible
        // rather than merely absent: a variant added to any of the four wrapped
        // enums now fails to compile in `audit_class` as it already did in
        // `status()`. This asserts the consequence — no two errors that mean
        // different things share a word — because a compile error cannot be
        // asserted from inside the crate that must compile.
        let classes = every_audit_class();
        let mut seen = classes.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            classes.len(),
            "two errors an auditor must tell apart are one word: {classes:?}"
        );
    }

    #[test]
    fn every_class_the_journal_can_write_is_documented_as_whose_fault_it_is() {
        // `shape_pointer` is a defect in this gateway and `shape_response` is
        // the provider's, and the prefix they share says neither — which is how
        // the wildcard managed to blame the provider for our own pointer for as
        // long as it did. `README.md` names the four groups, and a paragraph
        // nobody checks is how this branch has been wrong seven times, so this
        // is the check.
        let readme = include_str!("../../README.md");
        let (_, audit_section) = readme.split_once("### Audit").expect("the audit section");
        let audit_section = audit_section
            .split_once("\n## ")
            .map_or(audit_section, |(section, _)| section);
        for class in every_audit_class() {
            assert!(
                audit_section.contains(&format!("`{class}`")),
                "the journal can write `{class}` and the README does not say whose fault it is"
            );
        }
    }
}
