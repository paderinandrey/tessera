use serde_json::Value;

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
}

/// Where the text lives. Providers describe locations; masking and restoration
/// are written once against them, so a new shape adds no new rewriting code.
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn upstream_path(&self) -> &'static str;
    fn request_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError>;
    fn response_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError>;
    /// Where the text lives inside one streamed event. An event type we do not
    /// know carries no slots and is forwarded as it came: both protocols add
    /// event types over time, and `ping` must not break a stream.
    fn stream_slots(&self, event: &Value) -> Result<Vec<TextSlot>, ShapeError>;
    /// Which runs of text this event ends. A keepalive ends none: draining a
    /// buffer on one would release half a placeholder as ordinary text, and the
    /// client would reassemble the token we were hiding.
    fn stream_terminates(&self, event: &Value) -> Terminates;
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

pub struct OpenAi;
pub struct Anthropic;

/// Content parts we deliberately do not scan. Anything else without a `text`
/// string is refused rather than forwarded: a shape we do not understand may
/// carry personal data we would pass through untouched. Tool blocks are on this
/// list by absence — masking their arguments is a later slice, and until then
/// a request carrying them is refused rather than silently leaked.
const UNSCANNED_PART_TYPES: [&str; 4] = ["image_url", "image", "input_audio", "audio"];

/// Fields carrying tool definitions or tool traffic. Masking their arguments is
/// a later slice, so a request that uses them is refused: forwarding it would
/// send arbitrary strings past the masker.
const TOOL_FIELDS: [&str; 5] = [
    "tools",
    "tool_choice",
    "functions",
    "function_call",
    "tool_calls",
];

/// Every message must be an object carrying `content`. A bare string entry, or
/// an object without content, produced no pointer and was forwarded untouched —
/// the same silence-is-a-leak shape as the others.
fn require_scannable_message(message: &Value, provider: &'static str) -> Result<(), ShapeError> {
    if !message.is_object() || message.get("content").is_none() {
        return Err(ShapeError::Request(provider));
    }
    Ok(())
}

fn reject_tool_fields(body: &Value, provider: &'static str) -> Result<(), ShapeError> {
    for field in TOOL_FIELDS {
        if body.get(field).is_some_and(|value| !value.is_null()) {
            return Err(ShapeError::Unsupported(provider, field));
        }
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
    out: &mut Vec<String>,
) -> Result<(), ShapeError> {
    match body.pointer(lookup) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(_)) => {
            out.push(output);
            Ok(())
        }
        Some(_) => Err(ShapeError::Request(provider)),
    }
}

/// Whether a `logprobs` object actually carries token strings.
fn carries_tokens(logprobs: &Value) -> bool {
    ["content", "refusal"].iter().any(|field| {
        logprobs
            .get(field)
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
    })
}

fn content_pointers(
    prefix: &str,
    content: &Value,
    provider: &'static str,
    out: &mut Vec<String>,
) -> Result<(), ShapeError> {
    match content {
        Value::String(_) => out.push(prefix.to_owned()),
        Value::Array(parts) => {
            for (index, part) in parts.iter().enumerate() {
                if part.get("text").and_then(Value::as_str).is_some() {
                    out.push(format!("{prefix}/{index}/text"));
                    continue;
                }
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
                if !UNSCANNED_PART_TYPES.contains(&kind) {
                    return Err(ShapeError::Request(provider));
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

    fn request_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError> {
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .ok_or(ShapeError::Request("openai"))?;
        reject_tool_fields(body, "openai")?;
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
            reject_tool_fields(message, "openai")?;
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

    fn response_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError> {
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
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            // Tool arguments stream as their own field, past the masker.
            // Masking them is a later slice; until then they are refused.
            for field in ["tool_calls", "function_call"] {
                if delta.get(field).is_some_and(|value| !value.is_null()) {
                    return Err(ShapeError::Unsupported("openai", "tool_calls"));
                }
            }
            // Defence in depth: a provider sending token strings we did not ask
            // for does not get to put the masked output past restoration. An
            // empty or null `logprobs` field asks for nothing and costs nothing.
            if choice.get("logprobs").is_some_and(carries_tokens) {
                return Err(ShapeError::Unsupported("openai", "logprobs"));
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
}

impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn upstream_path(&self) -> &'static str {
        "/v1/messages"
    }

    fn request_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError> {
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .ok_or(ShapeError::Request("anthropic"))?;
        reject_tool_fields(body, "anthropic")?;
        // Extended thinking opens the stream with a `thinking` block whose text
        // this gateway neither masks nor restores, and whose signature is
        // computed over the text the provider saw — restoring it would break
        // verification on the caller's next turn. Refused here rather than at
        // the first streamed block, so the refusal does not cost the caller an
        // upstream call and the tokens with it.
        if body.get("thinking").is_some_and(|value| !value.is_null()) {
            return Err(ShapeError::Unsupported("anthropic", "thinking"));
        }
        let mut pointers = Vec::new();
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
            reject_tool_fields(message, "anthropic")?;
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

    fn response_pointers(&self, body: &Value) -> Result<Vec<String>, ShapeError> {
        let blocks = body
            .get("content")
            .and_then(Value::as_array)
            .ok_or(ShapeError::Response("anthropic"))?;
        let mut pointers = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            if block.get("text").and_then(Value::as_str).is_some() {
                pointers.push(format!("/content/{index}/text"));
                continue;
            }
            // A tool_use block's arguments can carry placeholders we issued.
            // Handing them to the client unrestored is the failure restoration
            // exists to prevent, so an unreadable block refuses the response.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pointers(slots: Result<Vec<TextSlot>, ShapeError>) -> Vec<String> {
        slots
            .unwrap()
            .into_iter()
            .map(|slot| slot.pointer)
            .collect()
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
            vec!["/messages/0/content"]
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
            vec!["/messages/0/content/0/text"]
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
            vec!["/user", "/messages/0/name", "/messages/0/content"]
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
            vec!["/metadata/user_id", "/messages/0/content"]
        );
    }

    #[test]
    fn openai_reads_the_response_content() {
        let body = json!({"choices": [{"message": {"role": "assistant", "content": "Hallo"}}]});
        assert_eq!(
            OpenAi.response_pointers(&body).unwrap(),
            vec!["/choices/0/message/content"]
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
            vec!["/system", "/messages/0/content/0/text"]
        );
    }

    #[test]
    fn anthropic_reads_the_response_blocks() {
        let body = json!({"content": [{"type": "text", "text": "Hallo"}]});
        assert_eq!(
            Anthropic.response_pointers(&body).unwrap(),
            vec!["/content/0/text"]
        );
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
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "content": "Weber"}
        ]}]});
        assert!(OpenAi.request_pointers(&body).is_err());
    }

    #[test]
    fn media_parts_are_allowed_through_unscanned() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "Weber"},
            {"type": "image_url", "image_url": {"url": "http://x"}}
        ]}]});
        assert_eq!(
            OpenAi.request_pointers(&body).unwrap(),
            vec!["/messages/0/content/0/text"]
        );
    }

    #[test]
    fn tool_bearing_requests_are_refused() {
        // Masking tool arguments is a later slice; until then a request that
        // uses them is refused rather than forwarded past the masker.
        let with_tools = json!({"messages": [{"role": "user", "content": "Hi"}],
                                "tools": [{"type": "function"}]});
        assert!(OpenAi.request_pointers(&with_tools).is_err());
        assert!(Anthropic.request_pointers(&with_tools).is_err());

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
        // A tool_use block can carry placeholders we issued; handing them to
        // the client unrestored is the failure restoration exists to prevent.
        let body = json!({"content": [{"type": "tool_use", "input": {"n": "[PERSON_1]"}}]});
        assert!(Anthropic.response_pointers(&body).is_err());
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

    #[test]
    fn an_extended_thinking_request_is_refused_before_the_call() {
        let body = json!({
            "model": "claude",
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "messages": [{"role": "user", "content": "Hallo"}]
        });
        assert!(Anthropic.request_pointers(&body).is_err());
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

    #[test]
    fn an_empty_streamed_logprobs_field_is_not_a_refusal() {
        // Providers send `logprobs: null` on every chunk when none were asked
        // for; refusing on that would break every stream.
        let event = json!({"choices": [{"index": 0, "delta": {"content": "a"}, "logprobs": null}]});
        assert!(OpenAi.stream_slots(&event).is_ok());
        let empty = json!({"choices": [{"index": 0, "delta": {"content": "a"},
                                        "logprobs": {"content": []}}]});
        assert!(OpenAi.stream_slots(&empty).is_ok());
    }

    #[test]
    fn streamed_logprobs_are_refused_too() {
        let event = json!({"choices": [{"index": 0, "delta": {"content": "a"},
                                        "logprobs": {"content": [{"token": "[PER"}]}}]});
        assert!(OpenAi.stream_slots(&event).is_err());
    }
}
