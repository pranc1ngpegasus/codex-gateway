use serde::Deserialize;
use serde_json::Value;
use std::fmt::Write as _;

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub tools: Vec<Value>,
    pub tool_choice: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Option<Value>,
    #[serde(default)]
    pub tool_calls: Vec<Value>,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: Value,
    pub instructions: Option<String>,
    #[serde(default)]
    pub stream: bool,
    pub text: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ForcedTool {
    pub name: String,
    pub schema: Value,
}

#[must_use]
pub fn chat_prompt(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter_map(|message| {
            let content = content_text(Some(message.content.as_ref()?));
            if content.is_empty() && message.tool_calls.is_empty() {
                return None;
            }
            let mut block = format!("[{}]\n{}", message.role, content);
            if !message.tool_calls.is_empty() {
                block.push_str("\nTool calls: ");
                block.push_str(&Value::Array(message.tool_calls.clone()).to_string());
            }
            if let Some(tool_call_id) = &message.tool_call_id {
                let _ = write!(block, "\nTool call id: {tool_call_id}");
            }
            Some(block)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[must_use]
pub fn responses_prompt(request: &ResponsesRequest) -> String {
    let input = content_text(Some(&request.input));
    match request.instructions.as_deref() {
        Some(instructions) if !instructions.is_empty() => {
            format!("[instructions]\n{instructions}\n\n[input]\n{input}")
        },
        _ => input,
    }
}

#[must_use]
pub fn forced_tool(request: &ChatRequest) -> Option<ForcedTool> {
    let choice = request.tool_choice.as_ref()?;
    let name = choice
        .pointer("/function/name")
        .and_then(Value::as_str)
        .or_else(|| {
            choice
                .as_str()
                .filter(|choice| *choice != "auto" && *choice != "none" && *choice != "required")
        })?;
    let schema = request
        .tools
        .iter()
        .find(|tool| tool.pointer("/function/name").and_then(Value::as_str) == Some(name))
        .and_then(|tool| tool.pointer("/function/parameters"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"type": "object"}));
    Some(ForcedTool {
        name: name.to_owned(),
        schema,
    })
}

#[must_use]
pub fn response_output_schema(request: &ResponsesRequest) -> Option<Value> {
    let text = request.text.as_ref()?;
    text.pointer("/format/schema")
        .or_else(|| text.pointer("/format/json_schema/schema"))
        .cloned()
}

fn content_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                let text = content_text(Some(part));
                (!text.is_empty()).then_some(text)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Object(object)) => {
            if let Some(text) = object.get("text").and_then(Value::as_str) {
                text.to_owned()
            } else if let Some(content) = object.get("content") {
                let text = content_text(Some(content));
                match object.get("role").and_then(Value::as_str) {
                    Some(role) if !text.is_empty() => format!("[{role}]\n{text}"),
                    _ => text,
                }
            } else {
                String::new()
            }
        },
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn extracts_forced_function_schema() {
        let request: ChatRequest = serde_json::from_value(json!({
            "model": "codex",
            "messages": [{"role": "user", "content": "name this"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "session_title",
                    "parameters": {"type": "object", "required": ["title"]}
                }
            }],
            "tool_choice": {"type": "function", "function": {"name": "session_title"}}
        }))
        .unwrap();
        let tool = forced_tool(&request).unwrap();
        assert_eq!(tool.name, "session_title");
        assert_eq!(tool.schema["required"][0], "title");
    }

    #[test]
    fn flattens_content_blocks() {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model": "codex",
            "instructions": "Be terse",
            "input": [{
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }]
        }))
        .unwrap();
        assert!(responses_prompt(&request).contains("hello"));
    }
}
