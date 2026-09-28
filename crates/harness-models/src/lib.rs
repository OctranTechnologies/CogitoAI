mod anthropic;
mod credentials;
mod gemini;
mod mock;
mod openai;
mod opencode;
mod preferences;
mod protocol;
mod registry;
mod transport;

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use anthropic::{AnthropicMessagesTransport, AnthropicProvider};
pub use credentials::{
    CredentialError, CredentialSecret, CredentialSource, CredentialStatus, CredentialStore,
    EnvironmentCredentialStore, KeychainBackend, SystemCredentialStore,
};
pub use gemini::{GeminiNativeTransport, GeminiProvider};
pub use mock::{DeterministicMockProvider, MockProvider, MockScenario, ScriptedMockProvider};
pub use openai::{OpenAIProvider, OpenAIResponsesTransport, OpenAiProvider};
pub use opencode::{OpenCodeProduct, OpenCodeProvider};
pub use preferences::{
    has_model_environment_override, load_project_model_preference, load_user_model_preference,
    load_user_model_preference_from, save_project_model_preference, save_user_model_preference,
    save_user_model_preference_to, user_model_preferences_path, ModelPreference,
    ModelPreferenceError,
};
pub use registry::{
    CapabilityKnowledge, CapabilityRequirement, CatalogRefreshReport, CatalogRefreshResult,
    ModelCapability, ModelCapabilityKnowledgeMap, ModelMetadata, ModelMetadataSource, ModelPricing,
    ModelRegistry, ModelRegistryError, ModelRegistryFilter,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    Developer,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            is_error: false,
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            is_error: false,
        }
    }

    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: calls,
            is_error: false,
        }
    }

    pub fn tool_result(result: ToolResult) -> Self {
        Self {
            role: Role::Tool,
            content: vec![ContentBlock::Text {
                text: result.content,
            }],
            name: None,
            tool_call_id: Some(result.tool_call_id),
            tool_calls: Vec::new(),
            is_error: result.is_error,
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
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    #[serde(default = "default_true")]
    pub text_input: bool,
    #[serde(rename = "vision", alias = "image_input", default)]
    pub image_input: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub tool_calling: bool,
    #[serde(default)]
    pub parallel_tool_calls: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub configurable_reasoning_effort: bool,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub system_instructions: bool,
    #[serde(default)]
    pub developer_instructions: bool,
    #[serde(default)]
    pub prompt_caching: bool,
    #[serde(default)]
    pub structured_output: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    /// Stable provider family identifier, used for display and configuration only.
    pub provider: String,
    /// Provider-native model identifier selected for this descriptor.
    pub id: String,
    pub display_name: String,
    pub capabilities: ModelCapabilities,
    /// Provenance and certainty for model metadata. Provider adapters leave
    /// this unknown; the registry fills it when catalog results are cached.
    #[serde(default)]
    pub metadata: ModelMetadata,
}

impl ModelDescriptor {
    /// Stable, provider-qualified identity. Provider model IDs can overlap.
    pub fn canonical_id(&self) -> String {
        let model_id = self
            .id
            .strip_prefix(&self.provider)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(&self.id);
        format!("{}/{}", self.provider, model_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReasoningConfig {
    pub effort: Option<ReasoningEffort>,
    pub budget_tokens: Option<u32>,
    #[serde(default)]
    pub include_summary: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub metadata: BTreeMap<String, String>,
    pub reasoning: Option<ReasoningConfig>,
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
            reasoning: None,
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
#[serde(tag = "type")]
pub enum ModelStreamEvent {
    #[serde(rename = "response.started")]
    ResponseStarted { id: Option<String>, model: String },
    #[serde(rename = "text.delta")]
    TextDelta { text: String },
    /// Only adapters that can safely expose a reasoning summary should emit this.
    #[serde(rename = "reasoning.delta")]
    ReasoningDelta { text: String },
    #[serde(rename = "tool_call.started")]
    ToolCallStarted {
        index: u32,
        id: Option<String>,
        name: Option<String>,
    },
    #[serde(rename = "tool_call.arguments.delta")]
    ToolCallArgumentsDelta { index: u32, delta: String },
    #[serde(rename = "tool_call.completed")]
    ToolCallCompleted { index: u32, call: ToolCall },
    #[serde(rename = "usage.updated")]
    UsageUpdated { usage: Usage },
    #[serde(rename = "response.completed")]
    ResponseCompleted { finish_reason: FinishReason },
    #[serde(rename = "response.failed")]
    ResponseFailed { error: String },
}

/// Emits the normalized event sequence for a completed non-streaming response.
pub fn emit_response_events(
    response: &ModelResponse,
    on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
) -> Result<(), ProviderError> {
    on_event(ModelStreamEvent::ResponseStarted {
        id: Some(response.id.clone()),
        model: response.model.clone(),
    })?;
    for block in &response.content {
        match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                let characters = text.chars().collect::<Vec<_>>();
                for chunk in characters.chunks(8) {
                    on_event(ModelStreamEvent::TextDelta {
                        text: chunk.iter().collect(),
                    })?;
                }
            }
            // Reasoning is intentionally omitted from generic synthesis. An adapter
            // must explicitly opt in only when it has a safe summary to expose.
            ContentBlock::Image { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::Text { .. } => {}
        }
    }
    for (index, call) in response.tool_calls.iter().enumerate() {
        on_event(ModelStreamEvent::ToolCallStarted {
            index: index as u32,
            id: Some(call.id.clone()),
            name: Some(call.name.clone()),
        })?;
        on_event(ModelStreamEvent::ToolCallArgumentsDelta {
            index: index as u32,
            delta: call.arguments.to_string(),
        })?;
        on_event(ModelStreamEvent::ToolCallCompleted {
            index: index as u32,
            call: call.clone(),
        })?;
    }
    if let Some(usage) = &response.usage {
        on_event(ModelStreamEvent::UsageUpdated {
            usage: usage.clone(),
        })?;
    }
    on_event(ModelStreamEvent::ResponseCompleted {
        finish_reason: response.finish_reason.clone(),
    })
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
    #[error("provider request was cancelled")]
    Cancelled,
    #[error("provider {provider} request timed out")]
    Timeout { provider: &'static str },
    #[error("provider configuration is invalid: {reason}")]
    Configuration { reason: String },
    #[error("model does not support requested capability: {capability}")]
    UnsupportedCapability { capability: &'static str },
}

pub trait ModelProvider: Send + Sync {
    fn descriptor(&self) -> ModelDescriptor;

    /// Returns model descriptors if this provider exposes runtime discovery.
    /// Providers without a discovery endpoint return an empty list.
    fn discover_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        Ok(Vec::new())
    }

    /// Refreshes a provider's cached model catalog. The default delegates to
    /// discovery; cache-aware adapters can bypass their normal freshness TTL.
    fn refresh_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        self.discover_models()
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError>;
    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError>;

    /// Cancellation-aware streaming hook. Providers with blocking transports
    /// should override this to interrupt reads promptly; simpler providers can
    /// rely on the generic preflight check and consumer callback.
    fn stream_cancellable(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ModelResponse, ProviderError> {
        if is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        self.stream(request, on_event)
    }

    /// Validates generic capability requirements and exposes one normalized stream contract.
    fn generate(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        self.generate_cancellable(request, on_event, &|| false)
    }

    /// Validates generic capability requirements and exposes one normalized
    /// event stream while allowing the transport to observe cancellation.
    fn generate_cancellable(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ModelResponse, ProviderError> {
        let result = (|| {
            if is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let capabilities = self.descriptor().capabilities;
            if !capabilities.text_input {
                return Err(ProviderError::UnsupportedCapability {
                    capability: "text input",
                });
            }
            if request.messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Image { .. }))
            }) && !capabilities.image_input
            {
                return Err(ProviderError::UnsupportedCapability {
                    capability: "image input",
                });
            }
            if !request.tools.is_empty() && !capabilities.tool_calling {
                return Err(ProviderError::UnsupportedCapability {
                    capability: "tool calling",
                });
            }
            if request.reasoning.is_some() && !capabilities.reasoning {
                return Err(ProviderError::UnsupportedCapability {
                    capability: "reasoning",
                });
            }
            if request.reasoning.as_ref().is_some_and(|reasoning| {
                reasoning.effort.is_some() || reasoning.budget_tokens.is_some()
            }) && !capabilities.configurable_reasoning_effort
            {
                return Err(ProviderError::UnsupportedCapability {
                    capability: "configurable reasoning effort",
                });
            }
            if request
                .messages
                .iter()
                .any(|message| message.role == Role::System)
                && !capabilities.system_instructions
            {
                return Err(ProviderError::UnsupportedCapability {
                    capability: "system instructions",
                });
            }
            if request
                .messages
                .iter()
                .any(|message| message.role == Role::Developer)
                && !capabilities.developer_instructions
            {
                return Err(ProviderError::UnsupportedCapability {
                    capability: "developer instructions",
                });
            }
            if capabilities.streaming {
                self.stream_cancellable(request, on_event, is_cancelled)
            } else {
                let response = self.complete(request)?;
                emit_response_events(&response, on_event)?;
                Ok(response)
            }
        })();
        if let Err(error) = &result {
            let _ = on_event(ModelStreamEvent::ResponseFailed {
                error: error.to_string(),
            });
        }
        result
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Mock,
    OpenAi,
    Anthropic,
    Gemini,
    #[serde(rename = "opencode-zen", alias = "opencode_zen", alias = "opencodezen")]
    OpenCodeZen,
    #[serde(rename = "opencode-go", alias = "opencode_go", alias = "opencodego")]
    OpenCodeGo,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider: ProviderKind,
    pub model: String,
    pub api_key_env: String,
    pub base_url: String,
    pub context_window: Option<u32>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self::for_provider(ProviderKind::Mock)
    }
}

impl ModelConfig {
    /// Sensible provider-specific defaults for local runtime configuration.
    /// Explicit model IDs remain valid even when absent from local metadata.
    pub fn for_provider(provider: ProviderKind) -> Self {
        match provider {
            ProviderKind::Mock => Self {
                provider,
                model: "mock".to_owned(),
                api_key_env: "OPENAI_API_KEY".to_owned(),
                base_url: "https://api.openai.com/v1".to_owned(),
                context_window: Some(8_192),
                reasoning_effort: None,
            },
            ProviderKind::OpenAi => Self {
                provider,
                model: "gpt-4.1-mini".to_owned(),
                api_key_env: "OPENAI_API_KEY".to_owned(),
                base_url: "https://api.openai.com/v1".to_owned(),
                context_window: None,
                reasoning_effort: None,
            },
            ProviderKind::Anthropic => Self {
                provider,
                model: "claude-sonnet-4-6".to_owned(),
                api_key_env: "ANTHROPIC_API_KEY".to_owned(),
                base_url: "https://api.anthropic.com/v1".to_owned(),
                context_window: None,
                reasoning_effort: None,
            },
            ProviderKind::Gemini => Self {
                provider,
                model: "gemini-3.8-flash".to_owned(),
                api_key_env: "GEMINI_API_KEY".to_owned(),
                base_url: "https://generativelanguage.googleapis.com/v1beta".to_owned(),
                context_window: None,
                reasoning_effort: None,
            },
            ProviderKind::OpenCodeZen => Self {
                provider,
                model: "opencode-zen/gpt-5.6-sol".to_owned(),
                api_key_env: "OPENCODE_API_KEY".to_owned(),
                base_url: "https://opencode.ai/zen/v1".to_owned(),
                context_window: None,
                reasoning_effort: None,
            },
            ProviderKind::OpenCodeGo => Self {
                provider,
                model: "opencode-go/glm-5.3".to_owned(),
                api_key_env: "OPENCODE_API_KEY".to_owned(),
                base_url: "https://opencode.ai/zen/go/v1".to_owned(),
                context_window: None,
                reasoning_effort: None,
            },
        }
    }

    /// Changes providers and refreshes only fields that still use the old
    /// provider's defaults. User-specified model IDs, endpoints, and key-env
    /// names survive a provider switch.
    pub fn select_provider(&mut self, provider: ProviderKind) {
        if provider == self.provider {
            return;
        }
        let previous = Self::for_provider(self.provider);
        let next = Self::for_provider(provider);
        self.reasoning_effort = None;
        if self.model == previous.model {
            self.model.clone_from(&next.model);
        }
        if self.api_key_env == previous.api_key_env {
            self.api_key_env.clone_from(&next.api_key_env);
        }
        if self.base_url == previous.base_url {
            self.base_url.clone_from(&next.base_url);
        }
        if self.context_window == previous.context_window {
            self.context_window = next.context_window;
        }
        self.provider = provider;
    }

    pub fn reasoning_config(&self) -> Option<ReasoningConfig> {
        self.reasoning_effort.map(|effort| ReasoningConfig {
            effort: Some(effort),
            budget_tokens: None,
            include_summary: false,
        })
    }

    /// Rejects settings that would leave the runtime unable to reach a model.
    ///
    /// Validation happens before anything is applied so an invalid value from a
    /// client can never leave the runtime half-configured.
    pub fn validate(&self) -> Result<(), String> {
        if self.model.trim().is_empty() {
            return Err("model must not be empty".to_owned());
        }
        if self.model.len() > 200 {
            return Err("model name is too long".to_owned());
        }
        if !self.base_url.trim().is_empty() {
            if !self.base_url.starts_with("https://") && !self.base_url.starts_with("http://") {
                return Err("base_url must start with http:// or https://".to_owned());
            }
            if self.base_url.len() > 2048 {
                return Err("base_url is too long".to_owned());
            }
        }
        // The API key environment variable is named, never the key itself, so
        // this must be a valid variable name.
        if self.api_key_env.trim().is_empty() {
            return Err("api_key_env must not be empty".to_owned());
        }
        if !self
            .api_key_env
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(format!(
                "api_key_env {:?} is not a valid environment variable name",
                self.api_key_env
            ));
        }
        Ok(())
    }

    pub fn from_env() -> Self {
        let provider = std::env::var("COGITO_MODEL_PROVIDER")
            .ok()
            .and_then(|value| serde_json::from_value(serde_json::Value::String(value)).ok())
            .unwrap_or(ProviderKind::Mock);
        let defaults = Self::for_provider(provider);
        Self {
            provider,
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
            reasoning_effort: std::env::var("COGITO_MODEL_REASONING_EFFORT")
                .ok()
                .and_then(|value| serde_json::from_value(serde_json::Value::String(value)).ok()),
        }
    }
}

pub fn provider_from_config(config: &ModelConfig) -> Result<Box<dyn ModelProvider>, ProviderError> {
    match config.provider {
        ProviderKind::Mock => Ok(Box::new(MockProvider::new(config.model.clone()))),
        ProviderKind::OpenAi => Ok(Box::new(OpenAiProvider::from_config(config)?)),
        ProviderKind::Anthropic => Ok(Box::new(AnthropicProvider::from_config(config)?)),
        ProviderKind::Gemini => Ok(Box::new(GeminiProvider::from_config(config)?)),
        ProviderKind::OpenCodeZen | ProviderKind::OpenCodeGo => {
            Ok(Box::new(OpenCodeProvider::from_config(config)?))
        }
    }
}

/// Creates a provider using a credential resolved by the runtime's credential
/// store. If the system credential store cannot be reached, provider creation
/// falls back to its environment-backed constructor so config inspection still
/// works and request failures remain normalized by the adapter.
pub fn provider_from_config_with_store(
    config: &ModelConfig,
    store: &dyn CredentialStore,
) -> Result<Box<dyn ModelProvider>, ProviderError> {
    if config.provider == ProviderKind::Mock {
        return provider_from_config(config);
    }
    let credential = store
        .get(provider_id(config.provider), &config.api_key_env)
        .ok()
        .flatten();
    if let Some(credential) = credential {
        provider_from_config_with_api_key(config, credential.expose_secret())
    } else {
        provider_from_config(config)
    }
}

/// Creates a provider using one explicitly supplied credential. The value is
/// kept in the provider adapter's private in-memory transport only.
pub fn provider_from_config_with_api_key(
    config: &ModelConfig,
    api_key: &str,
) -> Result<Box<dyn ModelProvider>, ProviderError> {
    config
        .validate()
        .map_err(|reason| ProviderError::Configuration { reason })?;
    harness_core::register_sensitive_value(api_key);
    match config.provider {
        ProviderKind::Mock => provider_from_config(config),
        ProviderKind::OpenAi => Ok(Box::new(OpenAiProvider::with_api_key(
            config.base_url.clone(),
            config.model.clone(),
            config.api_key_env.clone(),
            api_key,
        ))),
        ProviderKind::Anthropic => Ok(Box::new(AnthropicProvider::with_api_key(
            config.base_url.clone(),
            config.model.clone(),
            config.api_key_env.clone(),
            api_key,
        ))),
        ProviderKind::Gemini => Ok(Box::new(GeminiProvider::with_api_key(
            config.base_url.clone(),
            config.model.clone(),
            config.api_key_env.clone(),
            api_key,
        ))),
        ProviderKind::OpenCodeZen | ProviderKind::OpenCodeGo => {
            let product = if config.provider == ProviderKind::OpenCodeZen {
                OpenCodeProduct::Zen
            } else {
                OpenCodeProduct::Go
            };
            Ok(Box::new(OpenCodeProvider::with_api_key(
                product,
                config.base_url.clone(),
                config.model.clone(),
                config.api_key_env.clone(),
                api_key,
            )))
        }
    }
}

/// Checks an entered key with a minimal provider catalog request. Catalog
/// contents never escape; this returns only success or a normalized error.
pub fn validate_provider_credential(
    config: &ModelConfig,
    api_key: &str,
) -> Result<(), ProviderError> {
    config
        .validate()
        .map_err(|reason| ProviderError::Configuration { reason })?;
    harness_core::register_sensitive_value(api_key);
    let provider = provider_from_config_with_api_key(config, api_key)?;
    match config.provider {
        ProviderKind::Mock => Ok(()),
        ProviderKind::OpenCodeZen | ProviderKind::OpenCodeGo => {
            provider.refresh_models().map(|_| ())
        }
        ProviderKind::OpenAi | ProviderKind::Anthropic | ProviderKind::Gemini => {
            provider.discover_models().map(|_| ())
        }
    }
}

fn provider_id(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Mock => "mock",
        ProviderKind::OpenAi => "openai",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Gemini => "gemini",
        ProviderKind::OpenCodeZen => "opencode-zen",
        ProviderKind::OpenCodeGo => "opencode-go",
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
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}
