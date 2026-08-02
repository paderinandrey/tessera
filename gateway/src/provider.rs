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
        let mut pointers = Vec::new();
        // Personal data does not only live in `content`. OpenAI's optional
        // per-message `name` and top-level `user` are identifiers, and
        // forwarding them verbatim would leak exactly what the proxy exists to
        // stop.
        identifier_pointer(body, "/user", "/user".to_owned(), "openai", &mut pointers)?;
        for (index, message) in messages.iter().enumerate() {
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
}
