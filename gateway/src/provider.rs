use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum ShapeError {
    #[error("request body is not in the expected {0} shape")]
    Request(&'static str),
    #[error("upstream response is not in the expected {0} shape")]
    Response(&'static str),
    #[error("no value at {0}")]
    Pointer(String),
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
        let mut pointers = Vec::new();
        // Personal data does not only live in `content`. OpenAI's optional
        // per-message `name` and top-level `user` are identifiers, and
        // forwarding them verbatim would leak exactly what the proxy exists to
        // stop.
        if body.get("user").and_then(Value::as_str).is_some() {
            pointers.push("/user".to_owned());
        }
        for (index, message) in messages.iter().enumerate() {
            if message.get("name").and_then(Value::as_str).is_some() {
                pointers.push(format!("/messages/{index}/name"));
            }
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
        let mut pointers = Vec::new();
        // Anthropic carries a caller-supplied identifier here.
        if body
            .pointer("/metadata/user_id")
            .and_then(Value::as_str)
            .is_some()
        {
            pointers.push("/metadata/user_id".to_owned());
        }
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
            }
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
    fn pointers_round_trip_through_read_and_write() {
        let mut body = json!({"messages": [{"content": "Weber"}]});
        assert_eq!(read_pointer(&body, "/messages/0/content").unwrap(), "Weber");
        write_pointer(&mut body, "/messages/0/content", "[PERSON_1]").unwrap();
        assert_eq!(body["messages"][0]["content"], "[PERSON_1]");
    }
}
