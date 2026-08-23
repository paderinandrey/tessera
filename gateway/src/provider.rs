use serde_json::Value;

use crate::mapping::Shape;

#[derive(Debug, thiserror::Error)]
pub enum ShapeError {
    #[error("request body is not in the expected {0} shape")]
    Request(&'static str),
    #[error("upstream response is not in the expected {0} shape")]
    Response(&'static str),
    #[error("no value at {0}")]
    Pointer(String),
    #[error("{0} request uses {1}, which this gateway does not mask yet; it is refused rather than forwarded")]
    Unsupported(&'static str, &'static str),
    /// An embedded document that is not the JSON it claims to be. Its own
    /// variant because it is the *caller's* mistake and nothing else here is:
    /// `Pointer` is a 502 that blames the upstream, and a model emitting
    /// truncated `arguments` — which they do — is echoed back by the client on
    /// the next turn as ordinary input. Blaming the provider for that would
    /// send the caller looking in the wrong place forever.
    #[error("{0} tool arguments at {1} are not the JSON they are declared to be")]
    MalformedDocument(&'static str, String),
}

/// Where the text lives. Providers describe locations; masking and restoration
/// are written once against them, so a new shape adds no new rewriting code.
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn upstream_path(&self) -> &'static str;
    fn request_pointers(&self, body: &Value) -> Result<Vec<Slot>, ShapeError>;
    fn response_pointers(&self, body: &Value) -> Result<Vec<Slot>, ShapeError>;
    /// Where the text lives inside one streamed event. An event type we do not
    /// know carries no slots and is forwarded as it came: both protocols add
    /// event types over time, and `ping` must not break a stream.
    fn stream_slots(&self, event: &Value) -> Result<Vec<TextSlot>, ShapeError>;
    /// Which runs of text this event ends. A keepalive ends none: draining a
    /// buffer on one would release half a placeholder as ordinary text, and the
    /// client would reassemble the token we were hiding.
    fn stream_terminates(&self, event: &Value) -> Terminates;
    /// The header this provider authenticates with. A session is namespaced by
    /// its value: the mapping table is a restoration oracle, and an id alone
    /// would let a guessed id read another caller's values out of it one
    /// placeholder at a time.
    fn credential_header(&self) -> &'static str;
}

/// Where text sits in one streamed event, and which run of text it belongs to.
///
/// The two differ, and the difference is load-bearing. OpenAI streams one choice
/// per chunk at array position 0 whatever its logical `index`, so the pointer
/// alone would give two completions the same hold-back buffer and splice their
/// fragments together. The pointer addresses this event; the key identifies the
/// run across events.
#[derive(Debug, Clone, PartialEq)]
pub struct TextSlot {
    pub pointer: String,
    pub key: String,
}

/// What an event without text of its own does to the runs in progress.
#[derive(Debug, Clone, PartialEq)]
pub enum Terminates {
    /// A keepalive, a comment, an event type added after this was written.
    Nothing,
    Runs(Vec<String>),
    All,
}

/// Where a maskable value lives, and what kind of value it is. `Text` is a
/// string masked as it stands. `Json` is a document whose string leaves are
/// masked and whose keys are not — `embedded` distinguishes a document from a
/// string holding one, which is the only difference between Anthropic's
/// `input` object and OpenAI's `arguments`, and `shape` says whether the
/// document is a schema, whose strings are not all data.
#[derive(Debug, Clone, PartialEq)]
pub enum Slot {
    Text {
        pointer: String,
        /// Whether this string is tool traffic, and so counts against
        /// `max_tool_bytes`. A `tool_result` is a `Text` slot like any prompt
        /// text — it is a bare string — but it is the largest surface tool
        /// support opens, and a bound that skipped it would be a bound on the
        /// smaller half. False for ordinary prompt and message text, which the
        /// tool bound has no business limiting.
        tool: bool,
    },
    Json {
        pointer: String,
        embedded: bool,
        /// Whether the document is a JSON Schema. Every `Json` slot is tool
        /// traffic, so there is no `tool` flag here — there would be nothing
        /// to distinguish.
        shape: Shape,
    },
}

impl Slot {
    /// A plain string that is not tool traffic — prompt text, an identifier
    /// field, a message part. The common case, named so the call sites do not
    /// each repeat `tool: false`.
    fn text(pointer: String) -> Self {
        Slot::Text {
            pointer,
            tool: false,
        }
    }

    /// Test-only, and marked so rather than suppressed. Every production site
    /// has to know *which* kind it holds — masking reads a string or a
    /// document, the size bound counts documents alone — so each destructures
    /// its own arm and none of them wants an accessor that erases the kind.
    /// The tests want exactly that, to assert a list of locations without
    /// restating how each one is read.
    #[cfg(test)]
    pub fn pointer(&self) -> &str {
        match self {
            Slot::Text { pointer, .. } => pointer,
            Slot::Json { pointer, .. } => pointer,
        }
    }
}

pub struct OpenAi;
pub struct Anthropic;

/// Content parts we deliberately do not scan. Anything else without a `text`
/// string is refused rather than forwarded: a shape we do not understand may
/// carry personal data we would pass through untouched. Tool blocks are not on
/// this list and are not refused either — `content_pointers` describes them,
/// because their arguments and results are exactly the text this gateway
/// exists to mask.
const UNSCANNED_PART_TYPES: [&str; 4] = ["image_url", "image", "input_audio", "audio"];

/// Tool fields a provider still has no slots for. A request that uses one is
/// refused rather than forwarded, because forwarding it would send arbitrary
/// strings past the masker — the same silence-is-a-leak shape as an
/// unrecognized content part.
///
/// Per provider, because the two are relaxed one slice at a time. OpenAI's is
/// still the whole set until its own slice describes them; sharing one list
/// would have relaxed OpenAI's refusal here, where nothing yet produces a slot
/// to replace it.
///
/// Anthropic's tool traffic is described now, so what is left on its list is
/// the three fields Anthropic's API does not define at all. They are still
/// refused rather than ignored: a field no slot addresses is forwarded exactly
/// as it came, so `tool_calls` smuggled into an Anthropic body would carry its
/// arguments past the masker. `tool_choice` is deliberately absent — Anthropic
/// does define it, and it holds a tool's name, which is dispatch and is
/// therefore left alone rather than masked or refused.
const OPENAI_TOOL_FIELDS: [&str; 5] = [
    "tools",
    "tool_choice",
    "functions",
    "function_call",
    "tool_calls",
];
const ANTHROPIC_TOOL_FIELDS: [&str; 3] = ["functions", "function_call", "tool_calls"];

/// Every message must be an object carrying `content`. A bare string entry, or
/// an object without content, produced no pointer and was forwarded untouched —
/// the same silence-is-a-leak shape as the others.
fn require_scannable_message(message: &Value, provider: &'static str) -> Result<(), ShapeError> {
    if !message.is_object() || message.get("content").is_none() {
        return Err(ShapeError::Request(provider));
    }
    Ok(())
}

fn reject_tool_fields(
    body: &Value,
    fields: &[&'static str],
    provider: &'static str,
) -> Result<(), ShapeError> {
    for field in fields {
        if body.get(field).is_some_and(|value| !value.is_null()) {
            return Err(ShapeError::Unsupported(provider, field));
        }
    }
    Ok(())
}

/// `proxy.rs` calls `request_pointers` before it looks at `stream`, so relaxing
/// the tool refusal admits streamed tool requests as readily as buffered ones —
/// and `stream_slots`, which this slice does not touch, would then reject the
/// tool events *after* the upstream call, spending the caller's tokens to
/// return a broken stream. The streaming slice deletes this function; nothing
/// else should.
fn reject_streamed_tools(
    body: &Value,
    carries_tools: bool,
    provider: &'static str,
) -> Result<(), ShapeError> {
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if streaming && carries_tools {
        return Err(ShapeError::Unsupported(provider, "streamed tool traffic"));
    }
    Ok(())
}

/// Fields a tool definition may carry that this gateway can account for.
/// `name` is dispatch and `type` names a server tool; `cache_control` is
/// `{"type": "ephemeral"}` and carries nothing of the caller's. `description`
/// and the schema are described as slots.
///
/// Anything else is refused, because a tool definition is an object the caller
/// fills in and a field no slot addresses is forwarded exactly as it came.
/// Anthropic's own server tools are why this is not theoretical:
/// `web_search_20250305` carries `user_location: {city, region, country,
/// timezone}`, and `city` is a LOCATION in this gateway's own vocabulary.
const ANTHROPIC_TOOL_DEFINITION_FIELDS: [&str; 5] = [
    "name",
    "description",
    "input_schema",
    "type",
    "cache_control",
];

/// A tool definition's prose and its schema. The schema goes in whole rather
/// than by naming the keywords that carry text: `description` is not the only
/// one — `default`, `const`, `enum`, `examples`, `title` and `$comment` are all
/// client-controlled strings — and a list of what to scan is wrong the day
/// someone adds to it. Property names are keys, which the walk never yields in
/// key position; `Shape::Schema` is what handles the positions where a schema
/// names one as a value.
fn tool_definition_slots(
    prefix: &str,
    definition: &Value,
    schema_field: &str,
    allowed: &[&'static str],
    provider: &'static str,
    out: &mut Vec<Slot>,
) -> Result<(), ShapeError> {
    // A definition that is not an object has no fields to describe and would
    // travel whole: `"tools": ["Martina Weber"]` produced no slot and no
    // refusal.
    let fields = definition
        .as_object()
        .ok_or(ShapeError::Request(provider))?;
    for key in fields.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ShapeError::Unsupported(provider, "tool definition field"));
        }
    }
    // Present but not a string cannot be masked, so it is refused rather than
    // forwarded as it is — the rule `identifier_pointer` already applies to
    // `/user` and `/name`, which this once did not.
    match definition.get("description") {
        None | Some(Value::Null) => {}
        Some(Value::String(_)) => out.push(Slot::text(format!("{prefix}/description"))),
        Some(_) => return Err(ShapeError::Request(provider)),
    }
    if definition
        .get(schema_field)
        .is_some_and(|value| !value.is_null())
    {
        out.push(Slot::Json {
            pointer: format!("{prefix}/{schema_field}"),
            embedded: false,
            shape: Shape::Schema,
        });
    }
    Ok(())
}

/// A field that should hold a maskable string. Absent is fine; present but not
/// a string cannot be masked, so it is refused rather than forwarded as it is.
fn identifier_pointer(
    body: &Value,
    lookup: &str,
    output: String,
    provider: &'static str,
    out: &mut Vec<Slot>,
) -> Result<(), ShapeError> {
    match body.pointer(lookup) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(_)) => {
            out.push(Slot::text(output));
            Ok(())
        }
        Some(_) => Err(ShapeError::Request(provider)),
    }
}

/// Whether a streamed `logprobs` field is one that can be forwarded untouched.
///
/// Only shapes that demonstrably carry no text qualify: null, or an object whose
/// token lists are absent, null or empty. Everything else is refused — not only
/// a populated list. A field the slot path does not rewrite is forwarded exactly
/// as it came, so `"logprobs": "[PERSON_1]"` or a key we have never seen would
/// carry the placeholder straight to the client.
fn logprobs_carry_nothing(logprobs: &Value) -> bool {
    let Some(fields) = logprobs.as_object() else {
        return logprobs.is_null();
    };
    fields.iter().all(|(key, value)| match key.as_str() {
        "content" | "refusal" => match value {
            Value::Null => true,
            Value::Array(items) => items.is_empty(),
            _ => false,
        },
        _ => false,
    })
}

fn content_pointers(
    prefix: &str,
    content: &Value,
    provider: &'static str,
    out: &mut Vec<Slot>,
) -> Result<(), ShapeError> {
    match content {
        Value::String(_) => out.push(Slot::text(prefix.to_owned())),
        Value::Array(parts) => {
            for (index, part) in parts.iter().enumerate() {
                if part.get("text").and_then(Value::as_str).is_some() {
                    out.push(Slot::text(format!("{prefix}/{index}/text")));
                    continue;
                }
                match part.get("type").and_then(Value::as_str).unwrap_or("") {
                    // `id` and `name` are the client's dispatch and are never
                    // described; only the arguments are.
                    "tool_use" => {
                        if part.get("input").is_some() {
                            out.push(Slot::Json {
                                pointer: format!("{prefix}/{index}/input"),
                                embedded: false,
                                shape: Shape::Instance,
                            });
                        }
                        continue;
                    }
                    // A result recurses through this same function, which is
                    // what makes a string result and a list of blocks one case
                    // rather than two — and what makes an image inside a result
                    // inherit the policy images already have. `tool_use_id`
                    // is dispatch and is left alone.
                    "tool_result" => {
                        if let Some(content) = part.get("content") {
                            let from = out.len();
                            content_pointers(
                                &format!("{prefix}/{index}/content"),
                                content,
                                provider,
                                out,
                            )?;
                            // Whatever the recursion just produced is tool
                            // traffic, however deep it went. Marked here rather
                            // than inside the recursion because this is the
                            // only frame that knows it is inside a result.
                            for slot in &mut out[from..] {
                                if let Slot::Text { tool, .. } = slot {
                                    *tool = true;
                                }
                            }
                        }
                        continue;
                    }
                    kind if UNSCANNED_PART_TYPES.contains(&kind) => continue,
                    _ => return Err(ShapeError::Request(provider)),
                }
            }
        }
        // Neither a string nor a list of parts: we cannot say where the text
        // is, so the request does not go anywhere.
        _ => return Err(ShapeError::Request(provider)),
    }
    Ok(())
}

impl Provider for OpenAi {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn upstream_path(&self) -> &'static str {
        "/v1/chat/completions"
    }

    fn request_pointers(&self, body: &Value) -> Result<Vec<Slot>, ShapeError> {
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .ok_or(ShapeError::Request("openai"))?;
        reject_tool_fields(body, &OPENAI_TOOL_FIELDS, "openai")?;
        // With logprobs on, every choice carries the model's output again as
        // token strings under `logprobs`. Those are the masked tokens: joined
        // back together they spell the placeholder, and their probabilities
        // describe text the client will never see. Restoring them is not
        // meaningful — token boundaries do not follow placeholder boundaries —
        // so the request is refused before the call.
        // Explicitly disabled is not asking for them: an SDK that serializes
        // its default must not be turned away.
        match body.get("logprobs") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => {}
            _ => return Err(ShapeError::Unsupported("openai", "logprobs")),
        }
        // Audio output streams a transcript in fragments beside audio bytes we
        // cannot mask at all. Restoring the transcript would leave it saying a
        // name the recording does not say; leaving it alone would hand the
        // placeholder to the client a fragment at a time. Neither is a service,
        // so the request is refused before the call.
        if body.get("audio").is_some_and(|value| !value.is_null()) {
            return Err(ShapeError::Unsupported("openai", "audio"));
        }
        if body
            .get("modalities")
            .and_then(Value::as_array)
            .is_some_and(|modalities| {
                modalities
                    .iter()
                    .any(|modality| modality.as_str() == Some("audio"))
            })
        {
            return Err(ShapeError::Unsupported("openai", "audio"));
        }
        match body.get("top_logprobs") {
            None | Some(Value::Null) => {}
            Some(Value::Number(count)) if count.as_u64() == Some(0) => {}
            _ => return Err(ShapeError::Unsupported("openai", "top_logprobs")),
        }
        let mut pointers = Vec::new();
        // Personal data does not only live in `content`. OpenAI's optional
        // per-message `name` and top-level `user` are identifiers, and
        // forwarding them verbatim would leak exactly what the proxy exists to
        // stop.
        identifier_pointer(body, "/user", "/user".to_owned(), "openai", &mut pointers)?;
        for (index, message) in messages.iter().enumerate() {
            require_scannable_message(message, "openai")?;
            reject_tool_fields(message, &OPENAI_TOOL_FIELDS, "openai")?;
            if message.get("tool_call_id").is_some() {
                return Err(ShapeError::Unsupported("openai", "tool_call_id"));
            }
            identifier_pointer(
                message,
                "/name",
                format!("/messages/{index}/name"),
                "openai",
                &mut pointers,
            )?;
            if let Some(content) = message.get("content") {
                content_pointers(
                    &format!("/messages/{index}/content"),
                    content,
                    "openai",
                    &mut pointers,
                )?;
            }
        }
        Ok(pointers)
    }

    fn response_pointers(&self, body: &Value) -> Result<Vec<Slot>, ShapeError> {
        let choices = body
            .get("choices")
            .and_then(Value::as_array)
            .ok_or(ShapeError::Response("openai"))?;
        let mut pointers = Vec::new();
        for (index, choice) in choices.iter().enumerate() {
            if let Some(content) = choice.pointer("/message/content") {
                content_pointers(
                    &format!("/choices/{index}/message/content"),
                    content,
                    "openai",
                    &mut pointers,
                )?;
            }
        }
        Ok(pointers)
    }

    fn stream_slots(&self, event: &Value) -> Result<Vec<TextSlot>, ShapeError> {
        let Some(choices) = event.get("choices").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        let mut slots = Vec::new();
        for (position, choice) in choices.iter().enumerate() {
            // Checked before anything may skip this choice: `logprobs` sits
            // beside `delta`, not inside it, so a choice carrying no delta at
            // all can still carry token strings — and with another choice in the
            // same event producing a slot, the envelope is not restored whole.
            match choice.get("logprobs") {
                None => {}
                Some(logprobs) if logprobs_carry_nothing(logprobs) => {}
                Some(_) => return Err(ShapeError::Unsupported("openai", "logprobs")),
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            // A field whose text arrives in fragments must be a declared run:
            // restoring the envelope is event-local, so `[PER` and `SON_1]` in
            // successive events would each pass it untouched and join at the
            // client. Audio transcripts are such a field, and they are refused
            // rather than restored, for the reason given on the request side.
            if delta.get("audio").is_some_and(|value| !value.is_null()) {
                return Err(ShapeError::Unsupported("openai", "audio"));
            }
            // Tool arguments stream as their own field, past the masker.
            // Masking them is a later slice; until then they are refused.
            for field in ["tool_calls", "function_call"] {
                if delta.get(field).is_some_and(|value| !value.is_null()) {
                    return Err(ShapeError::Unsupported("openai", "tool_calls"));
                }
            }
            // `index` says which completion this chunk belongs to; the array
            // position only says where it sits in this chunk. With `n > 1` they
            // are not the same number.
            let index = choice
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(position as u64);
            // `refusal` is model text like `content` and streams beside it. Left
            // out, it would pass through unrestored — a placeholder handed to
            // the client in the one case where nobody is looking.
            for field in ["content", "refusal"] {
                match delta.get(field) {
                    None | Some(Value::Null) => {}
                    Some(Value::String(_)) => slots.push(TextSlot {
                        pointer: format!("/choices/{position}/delta/{field}"),
                        key: format!("choice/{index}/{field}"),
                    }),
                    // Recognized, unreadable: refused rather than forwarded.
                    Some(_) => return Err(ShapeError::Response("openai")),
                }
            }
        }
        Ok(slots)
    }

    fn stream_terminates(&self, event: &Value) -> Terminates {
        let Some(choices) = event.get("choices").and_then(Value::as_array) else {
            return Terminates::Nothing;
        };
        let mut keys = Vec::new();
        for (position, choice) in choices.iter().enumerate() {
            if choice
                .get("finish_reason")
                .is_some_and(|reason| !reason.is_null())
            {
                let index = choice
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(position as u64);
                keys.push(format!("choice/{index}/content"));
                keys.push(format!("choice/{index}/refusal"));
            }
        }
        if keys.is_empty() {
            Terminates::Nothing
        } else {
            Terminates::Runs(keys)
        }
    }

    fn credential_header(&self) -> &'static str {
        "authorization"
    }
}

impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn upstream_path(&self) -> &'static str {
        "/v1/messages"
    }

    fn request_pointers(&self, body: &Value) -> Result<Vec<Slot>, ShapeError> {
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .ok_or(ShapeError::Request("anthropic"))?;
        reject_tool_fields(body, &ANTHROPIC_TOOL_FIELDS, "anthropic")?;
        let mut pointers = Vec::new();
        let carries_tools = body.get("tools").is_some_and(|value| !value.is_null());
        reject_streamed_tools(body, carries_tools, "anthropic")?;
        // Explicitly null is an SDK serializing its default, not a request to
        // use tools — the same reading `thinking` and `logprobs` already get.
        if let Some(tools) = body.get("tools").filter(|value| !value.is_null()) {
            let tools = tools.as_array().ok_or(ShapeError::Request("anthropic"))?;
            for (index, definition) in tools.iter().enumerate() {
                tool_definition_slots(
                    &format!("/tools/{index}"),
                    definition,
                    "input_schema",
                    &ANTHROPIC_TOOL_DEFINITION_FIELDS,
                    "anthropic",
                    &mut pointers,
                )?;
            }
        }
        // Extended thinking opens the stream with a `thinking` block whose text
        // this gateway neither masks nor restores, and whose signature is
        // computed over the text the provider saw — restoring it would break
        // verification on the caller's next turn. Refused here rather than at
        // the first streamed block, so the refusal does not cost the caller an
        // upstream call and the tokens with it.
        match body.get("thinking") {
            None | Some(Value::Null) => {}
            // Explicitly disabled generates no thinking blocks and asks for
            // nothing, exactly as `logprobs: false` does.
            Some(config) => match config.get("type").and_then(Value::as_str) {
                Some("disabled") => {}
                _ => return Err(ShapeError::Unsupported("anthropic", "thinking")),
            },
        }
        // Anthropic carries a caller-supplied identifier here.
        identifier_pointer(
            body,
            "/metadata/user_id",
            "/metadata/user_id".to_owned(),
            "anthropic",
            &mut pointers,
        )?;
        if let Some(system) = body.get("system") {
            content_pointers("/system", system, "anthropic", &mut pointers)?;
        }
        for (index, message) in messages.iter().enumerate() {
            require_scannable_message(message, "anthropic")?;
            reject_tool_fields(message, &ANTHROPIC_TOOL_FIELDS, "anthropic")?;
            if let Some(content) = message.get("content") {
                content_pointers(
                    &format!("/messages/{index}/content"),
                    content,
                    "anthropic",
                    &mut pointers,
                )?;
            }
        }
        Ok(pointers)
    }

    fn response_pointers(&self, body: &Value) -> Result<Vec<Slot>, ShapeError> {
        let blocks = body
            .get("content")
            .and_then(Value::as_array)
            .ok_or(ShapeError::Response("anthropic"))?;
        let mut pointers = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            if block.get("text").and_then(Value::as_str).is_some() {
                pointers.push(Slot::text(format!("/content/{index}/text")));
                continue;
            }
            // A tool_use block's arguments carry the placeholders we issued,
            // and the client executes them: unrestored, it would open
            // `/home/[PERSON_1]/notes.txt`. Its `id` and `name` are dispatch
            // and are left exactly as the model wrote them.
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                if block.get("input").is_some() {
                    pointers.push(Slot::Json {
                        pointer: format!("/content/{index}/input"),
                        embedded: false,
                        shape: Shape::Instance,
                    });
                }
                continue;
            }
            // Any other block is one we cannot read. Handing a placeholder to
            // the client is the failure restoration exists to prevent, so an
            // unreadable block refuses the response.
            return Err(ShapeError::Response("anthropic"));
        }
        Ok(pointers)
    }

    fn stream_slots(&self, event: &Value) -> Result<Vec<TextSlot>, ShapeError> {
        // One content block is one run of text: its opening event and every
        // delta after it share a key, so a token split between them joins.
        let key = format!(
            "block/{}",
            event.get("index").and_then(Value::as_u64).unwrap_or(0)
        );
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_delta") => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => match delta.get("text") {
                        Some(Value::String(_)) => Ok(vec![TextSlot {
                            pointer: "/delta/text".to_owned(),
                            key,
                        }]),
                        _ => Err(ShapeError::Response("anthropic")),
                    },
                    // `input_json_delta` streams tool arguments, which this
                    // gateway does not mask yet.
                    Some("input_json_delta") => {
                        Err(ShapeError::Unsupported("anthropic", "tool_use"))
                    }
                    _ => Err(ShapeError::Response("anthropic")),
                }
            }
            Some("content_block_start") => {
                let block = event.get("content_block").unwrap_or(&Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => match block.get("text") {
                        Some(Value::String(_)) => Ok(vec![TextSlot {
                            pointer: "/content_block/text".to_owned(),
                            key,
                        }]),
                        _ => Err(ShapeError::Response("anthropic")),
                    },
                    Some("tool_use") => Err(ShapeError::Unsupported("anthropic", "tool_use")),
                    _ => Err(ShapeError::Response("anthropic")),
                }
            }
            _ => Ok(Vec::new()),
        }
    }

    fn stream_terminates(&self, event: &Value) -> Terminates {
        match event.get("type").and_then(Value::as_str) {
            Some("content_block_stop") => Terminates::Runs(vec![format!(
                "block/{}",
                event.get("index").and_then(Value::as_u64).unwrap_or(0)
            )]),
            Some("message_stop") | Some("error") => Terminates::All,
            // `ping` lands here, and so does every event type these protocols
            // grow later.
            _ => Terminates::Nothing,
        }
    }

    fn credential_header(&self) -> &'static str {
        "x-api-key"
    }
}

pub fn read_pointer(body: &Value, pointer: &str) -> Result<String, ShapeError> {
    body.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ShapeError::Pointer(pointer.to_owned()))
}

pub fn write_pointer(body: &mut Value, pointer: &str, text: &str) -> Result<(), ShapeError> {
    let slot = body
        .pointer_mut(pointer)
        .ok_or_else(|| ShapeError::Pointer(pointer.to_owned()))?;
    *slot = Value::String(text.to_owned());
    Ok(())
}

/// A `Json` slot's document. `embedded` means the pointer addresses a string
/// holding a document rather than the document itself — OpenAI's `arguments`
/// against Anthropic's `input`.
pub fn read_document(
    body: &Value,
    pointer: &str,
    embedded: bool,
    provider: &'static str,
) -> Result<Value, ShapeError> {
    let at = body
        .pointer(pointer)
        .ok_or_else(|| ShapeError::Pointer(pointer.to_owned()))?;
    if !embedded {
        return Ok(at.clone());
    }
    // Both failures below are the caller's: a slot said this holds a document,
    // and it does not. Neither is the upstream's fault, and a 502 would say it
    // was.
    let text = at
        .as_str()
        .ok_or_else(|| ShapeError::MalformedDocument(provider, pointer.to_owned()))?;
    serde_json::from_str(text)
        .map_err(|_| ShapeError::MalformedDocument(provider, pointer.to_owned()))
}

pub fn write_document(
    body: &mut Value,
    pointer: &str,
    document: &Value,
    embedded: bool,
) -> Result<(), ShapeError> {
    let slot = body
        .pointer_mut(pointer)
        .ok_or_else(|| ShapeError::Pointer(pointer.to_owned()))?;
    *slot = if embedded {
        Value::String(serde_json::to_string(document).map_err(|_| ShapeError::Response("json"))?)
    } else {
        document.clone()
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Unwraps `stream_slots`' `TextSlot` results, for the streamed-event tests.
    fn pointers(slots: Result<Vec<TextSlot>, ShapeError>) -> Vec<String> {
        slots
            .unwrap()
            .into_iter()
            .map(|slot| slot.pointer)
            .collect()
    }

    // Unwraps `request_pointers`/`response_pointers`' `Slot` results — a
    // different type from `TextSlot` above, so a second helper rather than a
    // shared one; Rust does not overload free functions by parameter type.
    // Named constructors so a full-`Slot` expectation stays as readable as a
    // list of pointers was. The kind is the point: `slot_pointers` below cannot
    // see it, and the first slot whose kind mattered got through a mutation
    // because of that.
    fn text(pointer: &str) -> Slot {
        Slot::text(pointer.to_owned())
    }
    fn tool_text(pointer: &str) -> Slot {
        Slot::Text {
            pointer: pointer.to_owned(),
            tool: true,
        }
    }
    fn instance(pointer: &str) -> Slot {
        Slot::Json {
            pointer: pointer.to_owned(),
            embedded: false,
            shape: Shape::Instance,
        }
    }
    fn schema(pointer: &str) -> Slot {
        Slot::Json {
            pointer: pointer.to_owned(),
            embedded: false,
            shape: Shape::Schema,
        }
    }

    /// Pointers alone, for the one kind of assertion that is a predicate over
    /// pointer strings rather than a description of what was found. Everything
    /// that says "these are the slots" asserts the slots.
    fn slot_pointers(slots: Result<Vec<Slot>, ShapeError>) -> Vec<String> {
        slots
            .unwrap()
            .into_iter()
            .map(|slot| slot.pointer().to_owned())
            .collect()
    }

    #[test]
    fn a_text_slot_names_the_pointer_it_wraps() {
        let slot = Slot::text("/messages/0/content".to_owned());
        assert_eq!(slot.pointer(), "/messages/0/content");
        assert!(
            matches!(slot, Slot::Text { tool: false, .. }),
            "prompt text is not tool traffic and the tool bound must not count it"
        );
    }

    #[test]
    fn a_json_slot_remembers_whether_the_document_is_embedded() {
        let plain = Slot::Json {
            pointer: "/messages/0/content/0/input".to_owned(),
            embedded: false,
            shape: Shape::Instance,
        };
        let embedded = Slot::Json {
            pointer: "/messages/0/tool_calls/0/function/arguments".to_owned(),
            embedded: true,
            shape: Shape::Instance,
        };
        assert_eq!(plain.pointer(), "/messages/0/content/0/input");
        assert!(!matches!(plain, Slot::Json { embedded: true, .. }));
        assert!(matches!(embedded, Slot::Json { embedded: true, .. }));
    }

    fn keys(slots: Result<Vec<TextSlot>, ShapeError>) -> Vec<String> {
        slots.unwrap().into_iter().map(|slot| slot.key).collect()
    }

    #[test]
    fn interleaved_choices_get_distinct_keys_at_the_same_position() {
        // With `n > 1` each chunk carries one choice at array position 0, and
        // only `index` says which completion it is. Keying on the pointer would
        // splice two completions into one hold-back buffer.
        let first = json!({"choices": [{"index": 0, "delta": {"content": "a"}}]});
        let second = json!({"choices": [{"index": 1, "delta": {"content": "b"}}]});
        assert_eq!(
            pointers(OpenAi.stream_slots(&first)),
            pointers(OpenAi.stream_slots(&second))
        );
        assert_ne!(
            keys(OpenAi.stream_slots(&first)),
            keys(OpenAi.stream_slots(&second))
        );
    }

    #[test]
    fn an_anthropic_block_keeps_one_key_from_start_to_delta() {
        // A token split between the opening event and the first delta must join.
        let start = json!({"type": "content_block_start", "index": 2,
                           "content_block": {"type": "text", "text": ""}});
        let delta = json!({"type": "content_block_delta", "index": 2,
                           "delta": {"type": "text_delta", "text": "x"}});
        assert_eq!(
            keys(Anthropic.stream_slots(&start)),
            keys(Anthropic.stream_slots(&delta))
        );
    }

    #[test]
    fn separate_anthropic_blocks_get_separate_keys() {
        let zero = json!({"type": "content_block_delta", "index": 0,
                          "delta": {"type": "text_delta", "text": "x"}});
        let one = json!({"type": "content_block_delta", "index": 1,
                         "delta": {"type": "text_delta", "text": "y"}});
        assert_ne!(
            keys(Anthropic.stream_slots(&zero)),
            keys(Anthropic.stream_slots(&one))
        );
    }

    #[test]
    fn openai_finds_string_content() {
        let body = json!({"messages": [{"role": "user", "content": "Weber"}]});
        assert_eq!(
            OpenAi.request_pointers(&body).unwrap(),
            vec![text("/messages/0/content")]
        );
    }

    #[test]
    fn openai_finds_text_parts_and_skips_the_rest() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "Weber"},
            {"type": "image_url", "image_url": {"url": "http://x"}}
        ]}]});
        assert_eq!(
            OpenAi.request_pointers(&body).unwrap(),
            vec![text("/messages/0/content/0/text")]
        );
    }

    #[test]
    fn openai_masks_identifier_fields_outside_content() {
        let body = json!({
            "user": "weber@example.ch",
            "messages": [{"role": "user", "name": "Weber", "content": "Hallo"}]
        });
        assert_eq!(
            OpenAi.request_pointers(&body).unwrap(),
            vec![
                text("/user"),
                text("/messages/0/name"),
                text("/messages/0/content")
            ]
        );
    }

    #[test]
    fn anthropic_masks_the_metadata_user_id() {
        let body = json!({
            "metadata": {"user_id": "weber-1"},
            "messages": [{"role": "user", "content": "Hallo"}]
        });
        assert_eq!(
            Anthropic.request_pointers(&body).unwrap(),
            vec![text("/metadata/user_id"), text("/messages/0/content")]
        );
    }

    #[test]
    fn openai_reads_the_response_content() {
        let body = json!({"choices": [{"message": {"role": "assistant", "content": "Hallo"}}]});
        assert_eq!(
            OpenAi.response_pointers(&body).unwrap(),
            vec![text("/choices/0/message/content")]
        );
    }

    #[test]
    fn anthropic_finds_the_system_field_and_the_messages() {
        let body = json!({
            "system": "Du bist hilfreich",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Weber"}]}]
        });
        assert_eq!(
            Anthropic.request_pointers(&body).unwrap(),
            vec![text("/system"), text("/messages/0/content/0/text")]
        );
    }

    #[test]
    fn anthropic_reads_the_response_blocks() {
        let body = json!({"content": [{"type": "text", "text": "Hallo"}]});
        assert_eq!(
            Anthropic.response_pointers(&body).unwrap(),
            vec![text("/content/0/text")]
        );
    }

    #[test]
    fn anthropic_describes_tool_definitions_and_calls_and_results() {
        let body = json!({
            "model": "claude",
            "tools": [{
                "name": "read_file",
                "description": "Read a file for Dr. Weber",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
            }],
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "read_file",
                     "input": {"path": "/home/weber/notes.txt"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "Martina Weber"}
                ]}
            ]
        });
        assert_eq!(
            Anthropic.request_pointers(&body).unwrap(),
            vec![
                text("/tools/0/description"),
                schema("/tools/0/input_schema"),
                instance("/messages/0/content/0/input"),
                tool_text("/messages/1/content/0/content"),
            ]
        );
    }

    #[test]
    fn anthropic_never_describes_a_tool_name_or_a_result_id() {
        let body = json!({
            "model": "claude",
            "tools": [{"name": "read_file", "description": "d", "input_schema": {}}],
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "x"}
            ]}]
        });
        let described = slot_pointers(Anthropic.request_pointers(&body));
        assert!(
            !described
                .iter()
                .any(|p| p.ends_with("/name") || p.ends_with("/tool_use_id")),
            "a tool name and a result id are the client's dispatch: {described:?}"
        );
    }

    #[test]
    fn a_tool_definition_this_gateway_cannot_account_for_is_refused() {
        // A tool definition is an object the caller fills in, and a field no
        // slot addresses travels exactly as it came. Anthropic's own server
        // tools are the live case: `web_search_20250305` carries
        // `user_location`, whose `city` is a LOCATION in this gateway's
        // vocabulary.
        let server_tool = json!({
            "model": "claude",
            "tools": [{
                "type": "web_search_20250305",
                "name": "web_search",
                "user_location": {"city": "Zurich", "country": "CH"}
            }],
            "messages": [{"role": "user", "content": "Hi"}]
        });
        assert!(matches!(
            Anthropic.request_pointers(&server_tool),
            Err(ShapeError::Unsupported(
                "anthropic",
                "tool definition field"
            ))
        ));
    }

    #[test]
    fn a_tool_definition_that_cannot_be_read_is_refused_rather_than_forwarded() {
        // Each of these produced no slot and no refusal, which is the shape
        // that forwards a value untouched. `identifier_pointer` has refused
        // exactly this for `/user` and `/name` all along.
        let not_an_object = json!({
            "model": "claude",
            "tools": ["Martina Weber"],
            "messages": [{"role": "user", "content": "Hi"}]
        });
        assert!(Anthropic.request_pointers(&not_an_object).is_err());

        let description_not_a_string = json!({
            "model": "claude",
            "tools": [{"name": "f", "description": {"long": "Martina Weber"},
                       "input_schema": {}}],
            "messages": [{"role": "user", "content": "Hi"}]
        });
        assert!(Anthropic
            .request_pointers(&description_not_a_string)
            .is_err());

        let tools_not_an_array = json!({
            "model": "claude",
            "tools": {"name": "f"},
            "messages": [{"role": "user", "content": "Hi"}]
        });
        assert!(Anthropic.request_pointers(&tools_not_an_array).is_err());
    }

    #[test]
    fn a_tool_definitions_schema_is_described_as_a_schema_not_an_instance() {
        // The kind is load-bearing and a pointer list cannot see it: a schema
        // read as an instance masks the property names it states as values.
        let body = json!({
            "model": "claude",
            "tools": [{"name": "read_file", "description": "d", "input_schema": {}}],
            "messages": [{"role": "user", "content": [
                {"type": "tool_use", "id": "t1", "name": "read_file", "input": {}}
            ]}]
        });
        assert_eq!(
            Anthropic.request_pointers(&body).unwrap(),
            vec![
                Slot::text("/tools/0/description".to_owned()),
                Slot::Json {
                    pointer: "/tools/0/input_schema".to_owned(),
                    embedded: false,
                    shape: Shape::Schema,
                },
                Slot::Json {
                    pointer: "/messages/0/content/0/input".to_owned(),
                    embedded: false,
                    shape: Shape::Instance,
                },
            ]
        );
    }

    #[test]
    fn a_tool_results_text_is_marked_as_tool_traffic_and_a_prompts_is_not() {
        // The byte bound reads this flag. A result is a bare string like any
        // prompt text, so nothing but the flag distinguishes the largest
        // surface tool support opens from ordinary conversation.
        let body = json!({
            "model": "claude",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "prompt"},
                {"type": "tool_result", "tool_use_id": "t1", "content": "result"},
                {"type": "tool_result", "tool_use_id": "t2", "content": [
                    {"type": "text", "text": "nested result"}
                ]}
            ]}]
        });
        assert_eq!(
            Anthropic.request_pointers(&body).unwrap(),
            vec![
                Slot::Text {
                    pointer: "/messages/0/content/0/text".to_owned(),
                    tool: false,
                },
                Slot::Text {
                    pointer: "/messages/0/content/1/content".to_owned(),
                    tool: true,
                },
                Slot::Text {
                    pointer: "/messages/0/content/2/content/0/text".to_owned(),
                    tool: true,
                },
            ],
            "a result nested inside blocks is still a result"
        );
    }

    #[test]
    fn anthropic_still_refuses_the_openai_shaped_tool_fields() {
        // Anthropic's API defines none of these, so nothing here produces a
        // slot for them and nothing would mask them. A field no slot addresses
        // is forwarded exactly as it came, which is how `tool_calls` smuggled
        // into an Anthropic body would carry its arguments past the masker.
        for field in ["functions", "function_call", "tool_calls"] {
            let at_body = json!({
                "model": "claude",
                field: [{"name": "read_file", "arguments": "{\"n\":\"Weber\"}"}],
                "messages": [{"role": "user", "content": "Hi"}]
            });
            assert!(
                Anthropic.request_pointers(&at_body).is_err(),
                "{field} was allowed at the top level"
            );
            let at_message = json!({
                "model": "claude",
                "messages": [{"role": "assistant", "content": "Hi",
                              field: [{"function": {"arguments": "{\"n\":\"Weber\"}"}}]}]
            });
            assert!(
                Anthropic.request_pointers(&at_message).is_err(),
                "{field} was allowed on a message"
            );
        }
    }

    #[test]
    fn anthropic_tool_choice_is_neither_masked_nor_refused() {
        // It selects a tool by name. The name is the client's dispatch: masking
        // it would break the call it dispatches, and refusing it would close
        // forced tool use for no reason.
        let body = json!({
            "model": "claude",
            "tools": [{"name": "read_file", "input_schema": {}}],
            "tool_choice": {"type": "tool", "name": "read_file"},
            "messages": [{"role": "user", "content": "Hi"}]
        });
        assert_eq!(
            Anthropic.request_pointers(&body).unwrap(),
            vec![schema("/tools/0/input_schema"), text("/messages/0/content")],
            "tool_choice must not appear here"
        );
    }

    #[test]
    fn anthropic_refuses_tool_traffic_on_a_streamed_request() {
        let body = json!({
            "model": "claude",
            "stream": true,
            "tools": [{"name": "read_file", "description": "d", "input_schema": {}}],
            "messages": [{"role": "user", "content": "hello"}]
        });
        assert!(matches!(
            Anthropic.request_pointers(&body),
            Err(ShapeError::Unsupported(
                "anthropic",
                "streamed tool traffic"
            ))
        ));
    }

    #[test]
    fn a_body_without_messages_is_a_shape_error() {
        // Fail closed: an unparsed body must not be forwarded unmasked.
        assert!(OpenAi.request_pointers(&json!({"model": "gpt"})).is_err());
        assert!(Anthropic
            .request_pointers(&json!({"model": "claude"}))
            .is_err());
    }

    #[test]
    fn an_unrecognized_content_shape_is_refused() {
        // Silently finding no pointers would forward the body untouched.
        let body = json!({"messages": [{"role": "user", "content": {"text": "Weber"}}]});
        assert!(OpenAi.request_pointers(&body).is_err());
    }

    #[test]
    fn an_unrecognized_content_part_is_refused() {
        // A part type nobody has taught this gateway to read. `tool_result`
        // used to stand here and no longer can: `content_pointers` describes
        // it now, for both providers, since a result's text is text wherever
        // it turns up.
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "video_url", "video_url": {"url": "http://x"}}
        ]}]});
        assert!(OpenAi.request_pointers(&body).is_err());
        assert!(Anthropic.request_pointers(&body).is_err());
    }

    #[test]
    fn a_tool_result_part_is_understood_rather_than_refused() {
        // The positive half of the test above: `tool_result` moved from the
        // refused side to the described side, and both halves say so together.
        // `content_pointers` is shared, so it moved for both providers at once.
        //
        // The result recurses through `content_pointers` itself, so a bare
        // string and a list of blocks are one case, and an image inside a
        // result inherits the policy images already have — asserted here rather
        // than argued, since it is the reason the recursion exists.
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "Martina Weber"},
            {"type": "tool_result", "tool_use_id": "t2", "content": [
                {"type": "text", "text": "Weber"},
                {"type": "image", "source": {"type": "base64", "data": "..."}}
            ]}
        ]}]});
        for described in [
            OpenAi.request_pointers(&body).unwrap(),
            Anthropic.request_pointers(&body).unwrap(),
        ] {
            assert_eq!(
                described,
                vec![
                    tool_text("/messages/0/content/0/content"),
                    tool_text("/messages/0/content/1/content/0/text"),
                ],
                "the image is unscanned and the tool_use_id is dispatch"
            );
        }
    }

    #[test]
    fn media_parts_are_allowed_through_unscanned() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "Weber"},
            {"type": "image_url", "image_url": {"url": "http://x"}}
        ]}]});
        assert_eq!(
            OpenAi.request_pointers(&body).unwrap(),
            vec![text("/messages/0/content/0/text")]
        );
    }

    #[test]
    fn openai_tool_bearing_requests_are_refused() {
        // Masking OpenAI's tool arguments is a later slice, and nothing here
        // produces a slot for them yet; until then a request that uses them is
        // refused rather than forwarded past the masker. Anthropic is
        // deliberately absent — its tool traffic is described now, and the
        // tests above assert where.
        let with_tools = json!({"messages": [{"role": "user", "content": "Hi"}],
                                "tools": [{"type": "function"}]});
        assert!(OpenAi.request_pointers(&with_tools).is_err());

        let with_calls = json!({"messages": [
            {"role": "assistant", "tool_calls": [{"function": {"arguments": "{\"n\":\"Weber\"}"}}]}
        ]});
        assert!(OpenAi.request_pointers(&with_calls).is_err());
    }

    #[test]
    fn a_message_that_cannot_be_scanned_is_refused() {
        // A bare string entry, or an object with no content, used to produce no
        // pointer and travel upstream untouched.
        assert!(OpenAi
            .request_pointers(&json!({"messages": ["Weber"]}))
            .is_err());
        assert!(OpenAi
            .request_pointers(&json!({"messages": [{"role": "assistant"}]}))
            .is_err());
        assert!(Anthropic
            .request_pointers(&json!({"messages": ["Weber"]}))
            .is_err());
    }

    #[test]
    fn an_identifier_that_is_not_a_string_is_refused() {
        // as_str() returning None used to mean "nothing to mask", which left
        // the value in the body on its way upstream.
        assert!(OpenAi
            .request_pointers(&json!({"user": 42, "messages": []}))
            .is_err());
        assert!(OpenAi
            .request_pointers(&json!({"messages": [{"name": ["Weber"], "content": "Hi"}]}))
            .is_err());
        assert!(Anthropic
            .request_pointers(&json!({"metadata": {"user_id": 7}, "messages": []}))
            .is_err());
    }

    #[test]
    fn an_unreadable_anthropic_response_block_is_refused() {
        // A block that can carry placeholders we issued and that nothing here
        // knows how to read. Handing them to the client unrestored is the
        // failure restoration exists to prevent. `tool_use` used to stand
        // here; it is read now, and the test below says so.
        let body = json!({"content": [{"type": "thinking", "thinking": "[PERSON_1] again"}]});
        assert!(Anthropic.response_pointers(&body).is_err());
    }

    #[test]
    fn anthropic_reads_a_tool_use_block_out_of_the_response() {
        // The client executes these arguments. A placeholder left in one is a
        // call against a path that does not exist.
        let body = json!({"content": [
            {"type": "text", "text": "Reading it now"},
            {"type": "tool_use", "id": "t1", "name": "read_file",
             "input": {"path": "/home/[PERSON_1]/notes.txt"}}
        ]});
        let slots = Anthropic.response_pointers(&body).unwrap();
        assert_eq!(
            slots,
            vec![
                Slot::text("/content/0/text".to_owned()),
                Slot::Json {
                    pointer: "/content/1/input".to_owned(),
                    embedded: false,
                    shape: Shape::Instance,
                },
            ],
            "the id and the name are dispatch and are not described"
        );
    }

    #[test]
    fn an_embedded_document_round_trips_through_read_and_write() {
        // The `embedded: true` branch has no production caller yet — OpenAI's
        // `arguments` is its first, in the next slice — but both functions are
        // pure, so nothing about testing them needs to wait for one.
        let mut body = json!({"call": {"arguments": "{\"b\":\"Weber\",\"a\":1}"}});
        let document = read_document(&body, "/call/arguments", true, "openai").unwrap();
        assert_eq!(document, json!({"a": 1, "b": "Weber"}));

        write_document(&mut body, "/call/arguments", &document, true).unwrap();
        // Alphabetical, and deliberately asserted rather than discovered: this
        // crate's `serde_json::Map` is a `BTreeMap` — no `preserve_order`
        // feature — so re-serializing an embedded document sorts its keys.
        // Harmless (JSON objects are unordered and the client parses rather
        // than compares) but somebody debugging changed argument bytes should
        // find it written down.
        assert_eq!(body["call"]["arguments"], r#"{"a":1,"b":"Weber"}"#);
    }

    #[test]
    fn an_embedded_document_that_is_not_json_is_the_callers_mistake() {
        // Models emit truncated `arguments`, and clients echo the turn back
        // verbatim, so this is routine input rather than an exotic failure.
        // `Pointer` would have made it a 502 blaming the upstream and sent the
        // caller looking in the wrong place.
        let truncated = json!({"call": {"arguments": "{\"path\": \"/home/we"}});
        assert!(matches!(
            read_document(&truncated, "/call/arguments", true, "openai"),
            Err(ShapeError::MalformedDocument("openai", _))
        ));

        let not_a_string = json!({"call": {"arguments": {"path": "x"}}});
        assert!(matches!(
            read_document(&not_a_string, "/call/arguments", true, "openai"),
            Err(ShapeError::MalformedDocument("openai", _))
        ));

        // A pointer addressing nothing is still a `Pointer`: that is this
        // gateway describing a location that does not exist, which is ours.
        assert!(matches!(
            read_document(&truncated, "/call/missing", true, "openai"),
            Err(ShapeError::Pointer(_))
        ));
    }

    #[test]
    fn a_plain_document_is_read_and_written_as_itself() {
        let mut body = json!({"input": {"path": "Weber"}});
        let document = read_document(&body, "/input", false, "anthropic").unwrap();
        assert_eq!(document, json!({"path": "Weber"}));
        write_document(&mut body, "/input", &json!({"path": "[PERSON_1]"}), false).unwrap();
        assert_eq!(body["input"], json!({"path": "[PERSON_1]"}));
    }

    #[test]
    fn pointers_round_trip_through_read_and_write() {
        let mut body = json!({"messages": [{"content": "Weber"}]});
        assert_eq!(read_pointer(&body, "/messages/0/content").unwrap(), "Weber");
        write_pointer(&mut body, "/messages/0/content", "[PERSON_1]").unwrap();
        assert_eq!(body["messages"][0]["content"], "[PERSON_1]");
    }

    #[test]
    fn openai_finds_the_delta_content() {
        let event = json!({"choices": [{"index": 0, "delta": {"content": "hi"}}]});
        assert_eq!(
            pointers(OpenAi.stream_slots(&event)),
            ["/choices/0/delta/content"]
        );
    }

    #[test]
    fn openai_finds_every_choice_in_a_chunk() {
        let event = json!({"choices": [
            {"delta": {"content": "a"}},
            {"delta": {"content": "b"}}
        ]});
        assert_eq!(
            pointers(OpenAi.stream_slots(&event)),
            ["/choices/0/delta/content", "/choices/1/delta/content"]
        );
    }

    #[test]
    fn openai_finds_a_streamed_refusal() {
        // Refusal text is model output like content; unrestored it would carry
        // a placeholder to the client.
        let event = json!({"choices": [{"delta": {"refusal": "I cannot help [PERSON_1]"}}]});
        assert_eq!(
            pointers(OpenAi.stream_slots(&event)),
            ["/choices/0/delta/refusal"]
        );
    }

    #[test]
    fn openai_yields_nothing_for_a_finish_chunk() {
        let event = json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
        assert!(OpenAi.stream_slots(&event).unwrap().is_empty());
    }

    #[test]
    fn openai_refuses_a_non_string_delta_content() {
        // A shape we recognize but cannot read is refused, never forwarded.
        let event = json!({"choices": [{"delta": {"content": {"parts": []}}}]});
        assert!(OpenAi.stream_slots(&event).is_err());
    }

    #[test]
    fn openai_refuses_a_streamed_tool_call() {
        let event = json!({"choices": [{"delta": {"tool_calls": [{"index": 0}]}}]});
        assert!(OpenAi.stream_slots(&event).is_err());
    }

    #[test]
    fn anthropic_finds_the_text_delta() {
        let event = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hi"}
        });
        assert_eq!(pointers(Anthropic.stream_slots(&event)), ["/delta/text"]);
    }

    #[test]
    fn anthropic_finds_the_text_of_an_opening_block() {
        let event = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        });
        assert_eq!(
            pointers(Anthropic.stream_slots(&event)),
            ["/content_block/text"]
        );
    }

    #[test]
    fn anthropic_refuses_a_streamed_tool_block() {
        let event = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "input": {}}
        });
        assert!(Anthropic.stream_slots(&event).is_err());
    }

    #[test]
    fn anthropic_refuses_an_input_json_delta() {
        let event = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{"}
        });
        assert!(Anthropic.stream_slots(&event).is_err());
    }

    #[test]
    fn unknown_event_types_carry_no_text() {
        // `ping` and event types added later must not break a stream.
        assert!(Anthropic
            .stream_slots(&json!({"type": "ping"}))
            .unwrap()
            .is_empty());
        assert!(OpenAi
            .stream_slots(&json!({"object": "x"}))
            .unwrap()
            .is_empty());
    }

    fn thinking_request(config: Value) -> Value {
        json!({
            "model": "claude",
            "thinking": config,
            "messages": [{"role": "user", "content": "Hallo"}]
        })
    }

    #[test]
    fn an_extended_thinking_request_is_refused_before_the_call() {
        let body = thinking_request(json!({"type": "enabled", "budget_tokens": 1024}));
        assert!(Anthropic.request_pointers(&body).is_err());
    }

    #[test]
    fn explicitly_disabled_thinking_is_allowed() {
        assert!(Anthropic
            .request_pointers(&thinking_request(json!({"type": "disabled"})))
            .is_ok());
        assert!(Anthropic
            .request_pointers(&thinking_request(json!(null)))
            .is_ok());
    }

    #[test]
    fn a_streamed_thinking_block_is_refused_too() {
        // Defence in depth: a provider that streams one anyway does not get to
        // pass its text through unrestored.
        let start = json!({"type": "content_block_start", "index": 0,
                           "content_block": {"type": "thinking", "thinking": ""}});
        assert!(Anthropic.stream_slots(&start).is_err());
        let delta = json!({"type": "content_block_delta", "index": 0,
                           "delta": {"type": "thinking_delta", "thinking": "hm"}});
        assert!(Anthropic.stream_slots(&delta).is_err());
    }

    fn with_option(field: &str, value: Value) -> Value {
        json!({
            "model": "gpt-4o",
            field: value,
            "messages": [{"role": "user", "content": "Hallo"}]
        })
    }

    #[test]
    fn a_logprobs_request_is_refused_before_the_call() {
        // The token strings under `logprobs` are the masked output again.
        assert!(OpenAi
            .request_pointers(&with_option("logprobs", json!(true)))
            .is_err());
        assert!(OpenAi
            .request_pointers(&with_option("top_logprobs", json!(5)))
            .is_err());
    }

    #[test]
    fn explicitly_disabled_logprobs_are_allowed() {
        // An SDK serializing its default asks for nothing and is not turned away.
        assert!(OpenAi
            .request_pointers(&with_option("logprobs", json!(false)))
            .is_ok());
        assert!(OpenAi
            .request_pointers(&with_option("logprobs", json!(null)))
            .is_ok());
        assert!(OpenAi
            .request_pointers(&with_option("top_logprobs", json!(0)))
            .is_ok());
    }

    fn streamed_logprobs(value: Value) -> Value {
        json!({"choices": [{"index": 0, "delta": {"content": "a"}, "logprobs": value}]})
    }

    #[test]
    fn an_empty_streamed_logprobs_field_is_not_a_refusal() {
        // Providers send `logprobs: null` on every chunk when none were asked
        // for; refusing on that would break every stream.
        for empty in [
            json!(null),
            json!({}),
            json!({"content": []}),
            json!({"content": null}),
        ] {
            assert!(
                OpenAi
                    .stream_slots(&streamed_logprobs(empty.clone()))
                    .is_ok(),
                "{empty} refused"
            );
        }
    }

    #[test]
    fn an_audio_output_request_is_refused_before_the_call() {
        // The transcript streams in fragments beside audio nothing can mask.
        assert!(OpenAi
            .request_pointers(&with_option(
                "audio",
                json!({"voice": "alloy", "format": "pcm16"})
            ))
            .is_err());
        assert!(OpenAi
            .request_pointers(&with_option("modalities", json!(["text", "audio"])))
            .is_err());
        assert!(OpenAi
            .request_pointers(&with_option("modalities", json!(["text"])))
            .is_ok());
    }

    #[test]
    fn a_streamed_audio_transcript_is_refused() {
        let event = json!({"choices": [{"index": 0,
                                        "delta": {"audio": {"transcript": "[PER"}}}]});
        assert!(OpenAi.stream_slots(&event).is_err());
    }

    #[test]
    fn logprobs_on_a_choice_without_a_delta_are_still_checked() {
        // The first choice produces a slot, so the event takes the rewriting
        // path and the second choice is forwarded exactly as it came.
        let event = json!({"choices": [
            {"index": 0, "delta": {"content": "a"}},
            {"index": 1, "logprobs": {"content": [{"token": "[PERSON_1]"}]}}
        ]});
        assert!(OpenAi.stream_slots(&event).is_err());
    }

    #[test]
    fn a_streamed_logprobs_field_that_is_not_demonstrably_empty_is_refused() {
        // The slot path rewrites `delta`, not this field: whatever is here is
        // forwarded exactly as it came.
        for carrying in [
            json!("[PERSON_1]"),
            json!({"content": "[PERSON_1]"}),
            json!({"content": [{"token": "[PER"}]}),
            json!({"refusal": [{"token": "[PER"}]}),
            json!({"tokens": ["[PERSON_1]"]}),
            json!(7),
        ] {
            assert!(
                OpenAi
                    .stream_slots(&streamed_logprobs(carrying.clone()))
                    .is_err(),
                "{carrying} allowed"
            );
        }
    }

    #[test]
    fn streamed_logprobs_are_refused_too() {
        let event = json!({"choices": [{"index": 0, "delta": {"content": "a"},
                                        "logprobs": {"content": [{"token": "[PER"}]}}]});
        assert!(OpenAi.stream_slots(&event).is_err());
    }
}
