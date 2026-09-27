use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::json;

use super::{
    emit_response_events, ContentBlock, FinishReason, Message, ModelCapabilities, ModelDescriptor,
    ModelProvider, ModelRequest, ModelResponse, ModelStreamEvent, ProviderError, ToolCall, Usage,
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
        let tool_calls = request
            .tools
            .first()
            .map(|tool| {
                vec![ToolCall {
                    id: format!("mock-call-{}", self.model),
                    name: tool.name.clone(),
                    arguments: json!({ "input": last_message_text(&request.messages) }),
                }]
            })
            .unwrap_or_default();
        let (content, finish_reason, output_tokens) = if tool_calls.is_empty() {
            let text = format!("mock response: {}", last_message_text(&request.messages));
            let output_tokens = text.split_whitespace().count() as u32;
            (
                vec![ContentBlock::Text { text }],
                FinishReason::Stop,
                output_tokens,
            )
        } else {
            (Vec::new(), FinishReason::ToolCalls, 1)
        };
        ModelResponse {
            id: format!("mock-response-{}", self.model),
            model: self.model.clone(),
            content,
            tool_calls,
            finish_reason,
            usage: Some(Usage::new(input_tokens, output_tokens)),
        }
    }
}

impl ModelProvider for MockProvider {
    fn descriptor(&self) -> ModelDescriptor {
        ModelDescriptor {
            provider: "mock".to_owned(),
            id: self.model.clone(),
            display_name: self.model.clone(),
            capabilities: ModelCapabilities {
                text_input: true,
                streaming: true,
                tool_calling: true,
                parallel_tool_calls: true,
                system_instructions: true,
                developer_instructions: true,
                context_window: self.context_window,
                ..ModelCapabilities::default()
            },
        }
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        Ok(self.complete_response(request))
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self.complete_response(request);
        emit_response_events(&response, on_event)?;
        Ok(response)
    }
}

pub struct ScriptedMockProvider {
    model: String,
    responses: Mutex<VecDeque<ModelResponse>>,
}

impl ScriptedMockProvider {
    pub fn new(model: impl Into<String>, responses: Vec<ModelResponse>) -> Self {
        Self {
            model: model.into(),
            responses: Mutex::new(responses.into()),
        }
    }

    fn next_response(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.responses
            .lock()
            .expect("scripted mock lock poisoned")
            .pop_front()
            .map(|mut response| {
                response.model = self.model.clone();
                response.usage.get_or_insert_with(|| {
                    Usage::new(
                        request
                            .messages
                            .iter()
                            .map(message_text)
                            .map(|text| text.len() as u32)
                            .sum(),
                        1,
                    )
                });
                response
            })
            .ok_or_else(|| ProviderError::InvalidResponse {
                provider: "scripted-mock",
                reason: "no scripted response remains".to_owned(),
            })
    }
}

impl ModelProvider for ScriptedMockProvider {
    fn descriptor(&self) -> ModelDescriptor {
        ModelDescriptor {
            provider: "scripted-mock".to_owned(),
            id: self.model.clone(),
            display_name: self.model.clone(),
            capabilities: mock_capabilities(Some(8_192), true),
        }
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.next_response(request)
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self.next_response(request)?;
        emit_response_events(&response, on_event)?;
        Ok(response)
    }
}

/// Deterministic scenarios used by tests and offline runtime checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MockScenario {
    TextOnly { text: String },
    StreamingToolCall { call: ToolCall },
    MultipleToolCalls { calls: Vec<ToolCall> },
    ErrorMidStream { partial_text: String, error: String },
}

pub struct DeterministicMockProvider {
    model: String,
    scenario: MockScenario,
}

impl DeterministicMockProvider {
    pub fn new(model: impl Into<String>, scenario: MockScenario) -> Self {
        Self {
            model: model.into(),
            scenario,
        }
    }

    pub fn text_only(model: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(model, MockScenario::TextOnly { text: text.into() })
    }

    pub fn streaming_tool_call(model: impl Into<String>, call: ToolCall) -> Self {
        Self::new(model, MockScenario::StreamingToolCall { call })
    }

    pub fn multiple_tool_calls(model: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self::new(model, MockScenario::MultipleToolCalls { calls })
    }

    pub fn error_mid_stream(
        model: impl Into<String>,
        partial_text: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self::new(
            model,
            MockScenario::ErrorMidStream {
                partial_text: partial_text.into(),
                error: error.into(),
            },
        )
    }

    fn response(&self) -> ModelResponse {
        let (content, tool_calls, finish_reason) = match &self.scenario {
            MockScenario::TextOnly { text } => (
                vec![ContentBlock::Text { text: text.clone() }],
                Vec::new(),
                FinishReason::Stop,
            ),
            MockScenario::StreamingToolCall { call } => {
                (Vec::new(), vec![call.clone()], FinishReason::ToolCalls)
            }
            MockScenario::MultipleToolCalls { calls } => {
                (Vec::new(), calls.clone(), FinishReason::ToolCalls)
            }
            MockScenario::ErrorMidStream { partial_text, .. } => (
                vec![ContentBlock::Text {
                    text: partial_text.clone(),
                }],
                Vec::new(),
                FinishReason::Error,
            ),
        };
        ModelResponse {
            id: format!("deterministic-{}", self.model),
            model: self.model.clone(),
            content,
            tool_calls,
            finish_reason,
            usage: Some(Usage::new(0, 0)),
        }
    }
}

impl ModelProvider for DeterministicMockProvider {
    fn descriptor(&self) -> ModelDescriptor {
        let supports_tools = matches!(
            self.scenario,
            MockScenario::StreamingToolCall { .. } | MockScenario::MultipleToolCalls { .. }
        );
        ModelDescriptor {
            provider: "deterministic-mock".to_owned(),
            id: self.model.clone(),
            display_name: self.model.clone(),
            capabilities: mock_capabilities(None, supports_tools),
        }
    }

    fn complete(&self, _request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        Ok(self.response())
    }

    fn stream(
        &self,
        _request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self.response();
        if let MockScenario::ErrorMidStream {
            partial_text,
            error,
        } = &self.scenario
        {
            on_event(ModelStreamEvent::ResponseStarted {
                id: Some(response.id),
                model: self.model.clone(),
            })?;
            if !partial_text.is_empty() {
                on_event(ModelStreamEvent::TextDelta {
                    text: partial_text.clone(),
                })?;
            }
            return Err(ProviderError::InvalidResponse {
                provider: "deterministic-mock",
                reason: error.clone(),
            });
        }
        emit_response_events(&response, on_event)?;
        Ok(response)
    }
}

fn mock_capabilities(context_window: Option<u32>, tool_calling: bool) -> ModelCapabilities {
    ModelCapabilities {
        text_input: true,
        streaming: true,
        tool_calling,
        parallel_tool_calls: tool_calling,
        system_instructions: true,
        developer_instructions: true,
        context_window,
        ..ModelCapabilities::default()
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
