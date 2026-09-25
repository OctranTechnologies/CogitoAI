use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};

use serde_json::{json, Value};

use super::{
    ContentBlock, FinishReason, Message, ModelCapabilities, ModelConfig, ModelProvider,
    ModelRequest, ModelResponse, ProviderError, Role, StreamDelta, StreamDeltaKind, ToolCall,
    ToolCallDelta, Usage,
};

pub struct OpenAiProvider {
    base_url: String,
    model: String,
    api_key_env: String,
    api_key: Option<String>,
    context_window: Option<u32>,
}

impl OpenAiProvider {
    pub fn from_config(config: &ModelConfig) -> Result<Self, ProviderError> {
        Ok(Self {
            base_url: config.base_url.clone(),
            model: config.model.clone(),
            api_key_env: config.api_key_env.clone(),
            api_key: std::env::var(&config.api_key_env)
                .ok()
                .filter(|value| !value.trim().is_empty()),
            context_window: config.context_window,
        })
    }

    pub fn with_api_key(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key_env: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let api_key = api_key.into();
        Self {
            base_url: base_url.into(),
            model: model.into(),
            api_key_env: api_key_env.into(),
            api_key: (!api_key.trim().is_empty()).then_some(api_key),
            context_window: Some(128_000),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn require_api_key(&self) -> Result<&str, ProviderError> {
        self.api_key
            .as_deref()
            .ok_or_else(|| ProviderError::MissingApiKey {
                provider: "openai",
                env_var: self.api_key_env.clone(),
            })
    }

    fn request_body(&self, request: &ModelRequest, stream: bool) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": request.messages.iter().map(openai_message).collect::<Vec<_>>(),
            "tools": request.tools.iter().map(openai_tool).collect::<Vec<_>>(),
            "stream": stream,
        });
        if let Some(max_output_tokens) = request.max_output_tokens {
            body["max_tokens"] = json!(max_output_tokens);
        }
        if let Some(temperature) = request.temperature {
            body["temperature"] = json!(temperature);
        }
        if stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        body
    }

    fn send(&self, body: &Value) -> Result<ureq::Response, ProviderError> {
        let api_key = self.require_api_key()?;
        ureq::post(&self.endpoint())
            .set("Authorization", &format!("Bearer {api_key}"))
            .set("Content-Type", "application/json")
            .send_json(body)
            .map_err(map_ureq_error)
    }

    fn complete_response(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        let response = self.send(&self.request_body(request, false))?;
        let value =
            response
                .into_json::<Value>()
                .map_err(|error| ProviderError::InvalidResponse {
                    provider: "openai",
                    reason: error.to_string(),
                })?;
        parse_completion(&value, &self.model)
    }
}

impl ModelProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: true,
            reasoning: true,
            context_window: self.context_window,
        }
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.complete_response(request)
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_delta: &mut dyn FnMut(StreamDelta) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self.send(&self.request_body(request, true))?;
        let reader = BufReader::new(response.into_reader());
        let mut sequence = 0;
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut tool_calls = BTreeMap::<u32, ToolAccumulator>::new();
        let mut finish_reason = None;
        let mut usage = None;
        for line in reader.lines() {
            let line = line.map_err(|_| ProviderError::Transport { provider: "openai" })?;
            if !line.starts_with("data:") {
                continue;
            }
            let data = line[5..].trim();
            if data == "[DONE]" {
                break;
            }
            let chunk = serde_json::from_str::<Value>(data).map_err(|error| {
                ProviderError::InvalidResponse {
                    provider: "openai",
                    reason: error.to_string(),
                }
            })?;
            if let Some(value) = chunk.get("usage").filter(|value| !value.is_null()) {
                usage = Some(parse_usage(value));
            }
            let Some(choice) = chunk
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| choices.first())
            else {
                continue;
            };
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                finish_reason = Some(parse_finish_reason(reason));
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(value) = delta.get("content").and_then(Value::as_str) {
                text.push_str(value);
                on_delta(StreamDelta {
                    sequence,
                    delta: StreamDeltaKind::Text {
                        text: value.to_owned(),
                    },
                    finish_reason: None,
                    usage: None,
                })?;
                sequence += 1;
            }
            if let Some(value) = delta.get("reasoning_content").and_then(Value::as_str) {
                reasoning.push_str(value);
                on_delta(StreamDelta {
                    sequence,
                    delta: StreamDeltaKind::Reasoning {
                        text: value.to_owned(),
                    },
                    finish_reason: None,
                    usage: None,
                })?;
                sequence += 1;
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let accumulator = tool_calls.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        accumulator.id.get_or_insert_with(|| id.to_owned());
                    }
                    if let Some(function) = call.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            accumulator.name.get_or_insert_with(|| name.to_owned());
                        }
                        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                            accumulator.arguments.push_str(arguments);
                        }
                    }
                    let delta = ToolCallDelta {
                        index,
                        id: call.get("id").and_then(Value::as_str).map(str::to_owned),
                        name: call
                            .get("function")
                            .and_then(|function| function.get("name"))
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        arguments_delta: call
                            .get("function")
                            .and_then(|function| function.get("arguments"))
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    };
                    on_delta(StreamDelta {
                        sequence,
                        delta: StreamDeltaKind::ToolCall { call: delta },
                        finish_reason: None,
                        usage: None,
                    })?;
                    sequence += 1;
                }
            }
        }
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text });
        }
        if !reasoning.is_empty() {
            content.push(ContentBlock::Reasoning { text: reasoning });
        }
        let tool_calls = tool_calls
            .into_values()
            .enumerate()
            .map(|(index, call)| call.finish(index))
            .collect();
        Ok(ModelResponse {
            id: format!("openai-stream-{}", self.model),
            model: self.model.clone(),
            content,
            tool_calls,
            finish_reason: finish_reason.unwrap_or(FinishReason::Stop),
            usage,
        })
    }
}

#[derive(Default)]
struct ToolAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolAccumulator {
    fn finish(self, index: usize) -> ToolCall {
        ToolCall {
            id: self.id.unwrap_or_else(|| format!("openai-tool-{index}")),
            name: self.name.unwrap_or_default(),
            arguments: serde_json::from_str(&self.arguments).unwrap_or(Value::Null),
        }
    }
}

fn openai_message(message: &Message) -> Value {
    let mut result = json!({ "role": role_name(message.role) });
    if let Some(name) = &message.name {
        result["name"] = json!(name);
    }
    if let Some(tool_call_id) = &message.tool_call_id {
        result["tool_call_id"] = json!(tool_call_id);
    }
    if message.content.len() == 1 {
        if let ContentBlock::Text { text } = &message.content[0] {
            result["content"] = json!(text);
        }
    }
    if result.get("content").is_none() {
        result["content"] = json!(message
            .content
            .iter()
            .map(openai_content)
            .collect::<Vec<_>>());
    }
    result
}

fn openai_content(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } | ContentBlock::Reasoning { text } => {
            json!({ "type": "text", "text": text })
        }
        ContentBlock::Image { media_type, data } => json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{media_type};base64,{data}") }
        }),
    }
}

fn openai_tool(tool: &super::ToolDefinition) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.input_schema,
        }
    })
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn parse_completion(value: &Value, model: &str) -> Result<ModelResponse, ProviderError> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| ProviderError::InvalidResponse {
            provider: "openai",
            reason: "response did not contain a choice".to_owned(),
        })?;
    let message = choice
        .get("message")
        .ok_or_else(|| ProviderError::InvalidResponse {
            provider: "openai",
            reason: "response did not contain a message".to_owned(),
        })?;
    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(ContentBlock::Text {
                text: text.to_owned(),
            });
        }
    }
    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (index, call) in calls.iter().enumerate() {
            let function = call
                .get("function")
                .ok_or_else(|| ProviderError::InvalidResponse {
                    provider: "openai",
                    reason: "tool call did not contain a function".to_owned(),
                })?;
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .map(|value| serde_json::from_str(value).unwrap_or(Value::Null))
                .unwrap_or(Value::Null);
            tool_calls.push(ToolCall {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("openai-tool-{index}"), str::to_owned),
                name: function
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                arguments,
            });
        }
    }
    Ok(ModelResponse {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("openai-response")
            .to_owned(),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(model)
            .to_owned(),
        content,
        tool_calls,
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map_or(FinishReason::Stop, parse_finish_reason),
        usage: value.get("usage").map(parse_usage),
    })
}

fn parse_usage(value: &Value) -> Usage {
    Usage {
        input_tokens: value
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        output_tokens: value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        total_tokens: value
            .get("total_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        cache_read_tokens: value
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        cache_creation_tokens: None,
    }
}

fn parse_finish_reason(value: &str) -> FinishReason {
    match value {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        "error" => FinishReason::Error,
        other => FinishReason::Other(other.to_owned()),
    }
}

fn map_ureq_error(error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::Status(status, _) => ProviderError::Request {
            provider: "openai",
            status: Some(status),
        },
        ureq::Error::Transport(_) => ProviderError::Transport { provider: "openai" },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ModelRequest, ToolDefinition};

    #[test]
    fn request_body_never_contains_api_key() {
        let provider = OpenAiProvider::with_api_key(
            "https://example.invalid/v1",
            "gpt-test",
            "TEST_KEY",
            "super-secret-key",
        );
        let request = ModelRequest {
            tools: vec![ToolDefinition {
                name: "read_file".to_owned(),
                description: "Read a file".to_owned(),
                input_schema: json!({ "type": "object" }),
            }],
            ..ModelRequest::new("gpt-test", vec![Message::user_text("hello")])
        };
        let body = provider.request_body(&request, true).to_string();

        assert!(body.contains("read_file"));
        assert!(!body.contains("super-secret-key"));
    }

    #[test]
    fn parses_provider_usage_and_finish_reason() {
        let value = json!({
            "id": "response-1",
            "model": "gpt-test",
            "choices": [{
                "message": { "role": "assistant", "content": "done" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6 }
        });

        let response = parse_completion(&value, "fallback").unwrap();

        assert_eq!(response.text(), "done");
        assert_eq!(response.finish_reason, FinishReason::Stop);
        assert_eq!(response.usage.unwrap().total_tokens, Some(6));
    }
}
