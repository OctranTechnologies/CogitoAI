mod mock;
mod openai;

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use mock::{MockProvider, ScriptedMockProvider};
pub use openai::OpenAiProvider;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { media_type: String, data: String },
    Reasoning { text: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
            name: None,
            tool_call_id: None,
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
            name: None,
            tool_call_id: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Error,
    Other(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub cache_read_tokens: Option<u32>,
    pub cache_creation_tokens: Option<u32>,
}

impl Usage {
    pub fn new(input_tokens: u32, output_tokens: u32) -> Self {
        Self {
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            total_tokens: Some(input_tokens + output_tokens),
            cache_read_tokens: None,
            cache_creation_tokens: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub vision: bool,
    pub reasoning: bool,
    pub context_window: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub metadata: BTreeMap<String, String>,
}

impl ModelRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            metadata: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub id: String,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
}

impl ModelResponse {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Image { .. } | ContentBlock::Reasoning { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub index: u32,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments_delta: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamDeltaKind {
    Text { text: String },
    Reasoning { text: String },
    ToolCall { call: ToolCallDelta },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StreamDelta {
    pub sequence: u32,
    pub delta: StreamDeltaKind,
    pub finish_reason: Option<FinishReason>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("missing API key for {provider}; set environment variable {env_var}")]
    MissingApiKey {
        provider: &'static str,
        env_var: String,
    },
    #[error("provider {provider} request failed")]
    Request {
        provider: &'static str,
        status: Option<u16>,
    },
    #[error("provider {provider} returned an invalid response: {reason}")]
    InvalidResponse {
        provider: &'static str,
        reason: String,
    },
    #[error("provider {provider} transport failed")]
    Transport { provider: &'static str },
    #[error("provider stream consumer failed")]
    StreamConsumer,
}

pub trait ModelProvider: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> ModelCapabilities;
    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError>;
    fn stream(
        &self,
        request: &ModelRequest,
        on_delta: &mut dyn FnMut(StreamDelta) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Mock,
    OpenAi,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: ProviderKind,
    pub model: String,
    pub api_key_env: String,
    pub base_url: String,
    pub context_window: Option<u32>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Mock,
            model: "mock".to_owned(),
            api_key_env: "OPENAI_API_KEY".to_owned(),
            base_url: "https://api.openai.com/v1".to_owned(),
            context_window: Some(8_192),
        }
    }
}

impl ModelConfig {
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            provider: std::env::var("COGITO_MODEL_PROVIDER")
                .ok()
                .and_then(|value| serde_json::from_value(serde_json::Value::String(value)).ok())
                .unwrap_or(defaults.provider),
            model: std::env::var("COGITO_MODEL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(defaults.model),
            api_key_env: std::env::var("COGITO_MODEL_API_KEY_ENV")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(defaults.api_key_env),
            base_url: std::env::var("COGITO_MODEL_BASE_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(defaults.base_url),
            context_window: Some(defaults.context_window.unwrap_or(8_192)),
        }
    }
}

pub fn provider_from_config(config: &ModelConfig) -> Result<Box<dyn ModelProvider>, ProviderError> {
    match config.provider {
        ProviderKind::Mock => Ok(Box::new(MockProvider::new(config.model.clone()))),
        ProviderKind::OpenAi => Ok(Box::new(OpenAiProvider::from_config(config)?)),
    }
}

impl fmt::Debug for ModelConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("api_key_env", &self.api_key_env)
            .field("base_url", &self.base_url)
            .field("context_window", &self.context_window)
            .finish()
    }
}
