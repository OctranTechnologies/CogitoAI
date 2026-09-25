use serde_json::json;

use super::{
    ContentBlock, FinishReason, Message, ModelCapabilities, ModelProvider, ModelRequest,
    ModelResponse, ProviderError, StreamDelta, StreamDeltaKind, ToolCall, ToolCallDelta, Usage,
};

#[derive(Clone)]
pub struct MockProvider {
    model: String,
    context_window: Option<u32>,
}

impl MockProvider {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            context_window: Some(8_192),
        }
    }

    pub fn with_context_window(mut self, context_window: u32) -> Self {
        self.context_window = Some(context_window);
        self
    }

    fn complete_response(&self, request: &ModelRequest) -> ModelResponse {
        let input_tokens = request
            .messages
            .iter()
            .map(message_text)
            .map(|text| text.len() as u32)
            .sum();
        if let Some(tool) = request.tools.first() {
            let arguments = json!({ "input": last_message_text(&request.messages) });
            let output_tokens = 1;
            return ModelResponse {
                id: format!("mock-response-{}", self.model),
                model: self.model.clone(),
                content: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: format!("mock-call-{}", self.model),
                    name: tool.name.clone(),
                    arguments,
                }],
                finish_reason: FinishReason::ToolCalls,
                usage: Some(Usage::new(input_tokens, output_tokens)),
            };
        }
        let text = format!("mock response: {}", last_message_text(&request.messages));
        let output_tokens = text.split_whitespace().count() as u32;
        ModelResponse {
            id: format!("mock-response-{}", self.model),
            model: self.model.clone(),
            content: vec![ContentBlock::Text { text }],
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: Some(Usage::new(input_tokens, output_tokens)),
        }
    }
}

impl ModelProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
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
        Ok(self.complete_response(request))
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_delta: &mut dyn FnMut(StreamDelta) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self.complete_response(request);
        let mut sequence = 0;
        if let Some(tool_call) = response.tool_calls.first() {
            on_delta(StreamDelta {
                sequence,
                delta: StreamDeltaKind::ToolCall {
                    call: ToolCallDelta {
                        index: 0,
                        id: Some(tool_call.id.clone()),
                        name: Some(tool_call.name.clone()),
                        arguments_delta: Some(tool_call.arguments.to_string()),
                    },
                },
                finish_reason: Some(FinishReason::ToolCalls),
                usage: response.usage.clone(),
            })?;
            return Ok(response);
        }
        let text = response.text();
        let chunks = text
            .as_bytes()
            .chunks(8)
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned());
        let chunks = chunks.collect::<Vec<_>>();
        for (index, chunk) in chunks.iter().enumerate() {
            let is_last = index + 1 == chunks.len();
            on_delta(StreamDelta {
                sequence,
                delta: StreamDeltaKind::Text {
                    text: chunk.clone(),
                },
                finish_reason: is_last.then_some(FinishReason::Stop),
                usage: is_last.then(|| response.usage.clone()).flatten(),
            })?;
            sequence += 1;
        }
        if chunks.is_empty() {
            on_delta(StreamDelta {
                sequence,
                delta: StreamDeltaKind::Text {
                    text: String::new(),
                },
                finish_reason: Some(FinishReason::Stop),
                usage: response.usage.clone(),
            })?;
        }
        Ok(response)
    }
}

fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Image { .. } | ContentBlock::Reasoning { .. } => None,
        })
        .collect()
}

fn last_message_text(messages: &[Message]) -> String {
    messages.last().map(message_text).unwrap_or_default()
}
