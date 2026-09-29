//! OpenCode Zen and Go gateway providers.
//!
//! OpenCode publishes models with different wire protocols. This module owns
//! that selection and delegates normalized requests to the shared Responses
//! and Messages adapters, or to the compatible Chat Completions adapter below.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::BufReader;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::anthropic::AnthropicProvider;
use super::openai::OpenAIProvider;
use super::protocol::ProtocolAdapter;
use super::transport::{for_each_sse_data_cancellable, request_get, request_json};
use super::{
    ContentBlock, FinishReason, Message, ModelCapabilities, ModelConfig, ModelDescriptor,
    ModelProvider, ModelRequest, ModelResponse, ModelStreamEvent, ProviderError, ReasoningEffort,
    Role, ToolCall, Usage,
};

const CATALOG_URL: &str = "https://models.dev/api.json";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const STREAM_READ_TIMEOUT: Duration = Duration::from_millis(250);
const STREAM_TIMEOUT: Duration = Duration::from_secs(600);
const CATALOG_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
static LOCAL_CATALOGS: OnceLock<Mutex<HashMap<String, CachedCatalog>>> = OnceLock::new();

/// OpenCode hosted service family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenCodeProduct {
    Zen,
    Go,
}

impl OpenCodeProduct {
    fn provider_key(self) -> &'static str {
        match self {
            Self::Zen => "opencode",
            Self::Go => "opencode-go",
        }
    }

    fn namespace(self) -> &'static str {
        match self {
            Self::Zen => "opencode-zen",
            Self::Go => "opencode-go",
        }
    }

    fn endpoint(self, base_url: &str) -> String {
        format!("{}/models", base_url.trim_end_matches('/'))
    }

    fn auth_provider(self) -> &'static str {
        match self {
            Self::Zen => "opencode-zen",
            Self::Go => "opencode-go",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WireProtocol {
    Responses,
    ChatCompletions,
    Messages,
}

#[derive(Clone, Debug)]
struct ModelMetadata {
    descriptor: ModelDescriptor,
    protocol: WireProtocol,
}

#[derive(Clone)]
struct CachedCatalog {
    refreshed_at: Instant,
    available: Vec<ModelDescriptor>,
    by_id: HashMap<String, ModelMetadata>,
}

/// User-facing router for the OpenCode Zen or Go hosted model catalog.
///
/// Credentials are held in memory only and excluded from `Debug`. Both
/// products use `OPENCODE_API_KEY`; Go access is granted by a Go subscription
/// on the same OpenCode account.
pub struct OpenCodeProvider {
    product: OpenCodeProduct,
    base_url: String,
    catalog_url: String,
    api_key_env: String,
    api_key: Option<String>,
    model: String,
    raw_model: String,
    responses: OpenAIProvider,
    messages: AnthropicProvider,
    chat: OpenAIChatCompletionsTransport,
    catalog: Mutex<Option<CachedCatalog>>,
}

impl fmt::Debug for OpenCodeProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenCodeProvider")
            .field("product", &self.product)
            .field("base_url", &self.base_url)
            .field("catalog_url", &self.catalog_url)
            .field("api_key_env", &self.api_key_env)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model", &self.model)
            .finish()
    }
}

impl OpenCodeProvider {
    pub fn from_config(config: &ModelConfig) -> Result<Self, ProviderError> {
        config
            .validate()
            .map_err(|reason| ProviderError::Configuration { reason })?;
        let product = match config.provider {
            super::ProviderKind::OpenCodeZen => OpenCodeProduct::Zen,
            super::ProviderKind::OpenCodeGo => OpenCodeProduct::Go,
            _ => {
                return Err(ProviderError::Configuration {
                    reason: "OpenCode provider requires an OpenCode Zen or Go configuration"
                        .to_owned(),
                });
            }
        };
        let api_key = std::env::var(&config.api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty());
        Ok(Self::new(
            product,
            config.base_url.clone(),
            config.model.clone(),
            config.api_key_env.clone(),
            api_key,
            CATALOG_URL.to_owned(),
        ))
    }

    pub fn with_api_key(
        product: OpenCodeProduct,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key_env: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let api_key = api_key.into();
        Self::new(
            product,
            base_url.into(),
            model.into(),
            api_key_env.into(),
            (!api_key.trim().is_empty()).then_some(api_key),
            CATALOG_URL.to_owned(),
        )
    }

    /// Constructor that accepts a metadata catalog URL, useful for private
    /// catalog mirrors and deterministic adapter tests.
    pub fn with_api_key_and_catalog_url(
        product: OpenCodeProduct,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key_env: impl Into<String>,
        api_key: impl Into<String>,
        catalog_url: impl Into<String>,
    ) -> Self {
        let api_key = api_key.into();
        Self::new(
            product,
            base_url.into(),
            model.into(),
            api_key_env.into(),
            (!api_key.trim().is_empty()).then_some(api_key),
            catalog_url.into(),
        )
    }

    fn new(
        product: OpenCodeProduct,
        base_url: String,
        model: String,
        api_key_env: String,
        api_key: Option<String>,
        catalog_url: String,
    ) -> Self {
        let base_url = base_url.trim_end_matches('/').to_owned();
        let raw_model = strip_namespace(product, &model).to_owned();
        let key_for_adapters = api_key.clone().unwrap_or_default();
        Self {
            product,
            responses: OpenAIProvider::with_api_key(
                &base_url,
                &raw_model,
                &api_key_env,
                key_for_adapters.clone(),
            ),
            messages: AnthropicProvider::with_api_key(
                &base_url,
                &raw_model,
                &api_key_env,
                key_for_adapters.clone(),
            ),
            chat: OpenAIChatCompletionsTransport::new(
                &base_url,
                &api_key_env,
                key_for_adapters,
                product.auth_provider(),
            ),
            base_url,
            catalog_url: catalog_url.trim_end_matches('/').to_owned(),
            api_key_env,
            api_key,
            model,
            raw_model,
            catalog: Mutex::new(None),
        }
    }

    /// Returns the account-visible model list, joined with OpenCode's live
    /// metadata catalog for protocol and capability details.
    pub fn discover_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        if let Some(cache) = self.cached_catalog()? {
            return Ok(cache.available);
        }
        self.refresh_models()
    }

    /// Bypasses the local thirty-minute catalog cache.
    pub fn refresh_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        let metadata_url = self.catalog_url.clone();
        let models_url = self.product.endpoint(&self.base_url);
        let owned_headers = self.auth_headers()?;
        let headers = owned_headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect::<Vec<_>>();

        let request_agent = http_agent(REQUEST_TIMEOUT, None);
        let catalog_response = request_get(
            &request_agent,
            &metadata_url,
            &[],
            self.product.auth_provider(),
        )?;
        let metadata_value = catalog_response
            .into_json::<Value>()
            .map_err(|_| invalid_response(self.product, "metadata catalog was not valid JSON"))?;
        let model_response = request_get(
            &request_agent,
            &models_url,
            &headers,
            self.product.auth_provider(),
        )?;
        let model_value = model_response
            .into_json::<Value>()
            .map_err(|_| invalid_response(self.product, "model listing was not valid JSON"))?;
        let cache = parse_catalog(self.product, &metadata_value, &model_value)?;
        let available = cache.available.clone();
        LOCAL_CATALOGS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| ProviderError::Transport {
                provider: self.product.auth_provider(),
            })?
            .insert(self.cache_key(), cache.clone());
        *self.catalog.lock().map_err(|_| ProviderError::Transport {
            provider: self.product.auth_provider(),
        })? = Some(cache);
        Ok(available)
    }

    fn auth_headers(&self) -> Result<Vec<(&'static str, String)>, ProviderError> {
        let key = self
            .api_key
            .as_deref()
            .ok_or_else(|| ProviderError::MissingApiKey {
                provider: self.product.auth_provider(),
                env_var: self.api_key_env.clone(),
            })?;
        Ok(vec![
            ("Authorization", format!("Bearer {key}")),
            ("Content-Type", "application/json".to_owned()),
        ])
    }

    fn cached_catalog(&self) -> Result<Option<CachedCatalogView>, ProviderError> {
        let cache_key = self.cache_key();
        if let Some(cache) = self
            .catalog
            .lock()
            .map_err(|_| ProviderError::Transport {
                provider: self.product.auth_provider(),
            })?
            .as_ref()
            .filter(|catalog| catalog.refreshed_at.elapsed() < CATALOG_CACHE_TTL)
        {
            return Ok(Some(CachedCatalogView {
                available: cache.available.clone(),
            }));
        }
        let shared = LOCAL_CATALOGS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| ProviderError::Transport {
                provider: self.product.auth_provider(),
            })?
            .get(&cache_key)
            .filter(|catalog| catalog.refreshed_at.elapsed() < CATALOG_CACHE_TTL)
            .cloned();
        if let Some(shared) = shared {
            let view = CachedCatalogView {
                available: shared.available.clone(),
            };
            *self.catalog.lock().map_err(|_| ProviderError::Transport {
                provider: self.product.auth_provider(),
            })? = Some(shared);
            return Ok(Some(view));
        }
        Ok(None)
    }

    fn cache_key(&self) -> String {
        // The available-model list is account-specific. Partition this local
        // cache using a one-way key hash without retaining the credential.
        let mut key_hash = std::collections::hash_map::DefaultHasher::new();
        self.api_key
            .as_deref()
            .unwrap_or_default()
            .hash(&mut key_hash);
        format!(
            "{}:{}:{}:{:016x}",
            self.product.namespace(),
            self.base_url,
            self.catalog_url,
            key_hash.finish()
        )
    }

    fn selected_metadata(&self) -> Result<ModelMetadata, ProviderError> {
        let _ = self.discover_models()?;
        let cache = self.catalog.lock().map_err(|_| ProviderError::Transport {
            provider: self.product.auth_provider(),
        })?;
        cache
            .as_ref()
            .and_then(|catalog| catalog.by_id.get(&self.raw_model))
            .cloned()
            .ok_or_else(|| {
                invalid_response(
                    self.product,
                    "configured model has no supported protocol metadata",
                )
            })
    }

    fn route_complete(
        &self,
        metadata: &ModelMetadata,
        request: &ModelRequest,
    ) -> Result<ModelResponse, ProviderError> {
        let result = match metadata.protocol {
            WireProtocol::Responses => self.responses.complete(request),
            WireProtocol::ChatCompletions => ProtocolAdapter::complete(&self.chat, request),
            WireProtocol::Messages => self.messages.complete(request),
        };
        result.map_err(|error| self.normalize_error(error))
    }

    fn route_stream(
        &self,
        metadata: &ModelMetadata,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ModelResponse, ProviderError> {
        let result = match metadata.protocol {
            WireProtocol::Responses => {
                self.responses
                    .stream_cancellable(request, on_event, is_cancelled)
            }
            WireProtocol::ChatCompletions => {
                ProtocolAdapter::stream_cancellable(&self.chat, request, on_event, is_cancelled)
            }
            WireProtocol::Messages => {
                self.messages
                    .stream_cancellable(request, on_event, is_cancelled)
            }
        };
        result.map_err(|error| self.normalize_error(error))
    }

    fn configured_request(&self, request: &ModelRequest) -> ModelRequest {
        let mut request = request.clone();
        request.model.clone_from(&self.raw_model);
        request
    }

    fn normalize_error(&self, error: ProviderError) -> ProviderError {
        let provider = self.product.auth_provider();
        match error {
            ProviderError::MissingApiKey { env_var, .. } => {
                ProviderError::MissingApiKey { provider, env_var }
            }
            ProviderError::Request { status, .. } => ProviderError::Request { provider, status },
            ProviderError::InvalidResponse { reason, .. } => {
                ProviderError::InvalidResponse { provider, reason }
            }
            ProviderError::Transport { .. } => ProviderError::Transport { provider },
            ProviderError::Timeout { .. } => ProviderError::Timeout { provider },
            other => other,
        }
    }
}

#[derive(Clone)]
struct CachedCatalogView {
    available: Vec<ModelDescriptor>,
}

impl ModelProvider for OpenCodeProvider {
    fn descriptor(&self) -> ModelDescriptor {
        let _ = self.cached_catalog();
        self.catalog
            .lock()
            .ok()
            .and_then(|cache| {
                cache
                    .as_ref()
                    .and_then(|catalog| catalog.by_id.get(&self.raw_model))
                    .map(|metadata| metadata.descriptor.clone())
            })
            .unwrap_or_else(|| ModelDescriptor {
                provider: self.product.namespace().to_owned(),
                id: self.model.clone(),
                display_name: self.raw_model.clone(),
                capabilities: ModelCapabilities {
                    text_input: true,
                    ..ModelCapabilities::default()
                },
                metadata: Default::default(),
            })
    }

    fn discover_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        OpenCodeProvider::discover_models(self)
    }

    fn refresh_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        OpenCodeProvider::refresh_models(self)
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        let metadata = self.selected_metadata()?;
        self.route_complete(&metadata, &self.configured_request(request))
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        self.stream_cancellable(request, on_event, &|| false)
    }

    fn stream_cancellable(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ModelResponse, ProviderError> {
        let metadata = self.selected_metadata()?;
        self.route_stream(
            &metadata,
            &self.configured_request(request),
            on_event,
            is_cancelled,
        )
    }

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
            let metadata = self.selected_metadata()?;
            validate_request(&metadata.descriptor.capabilities, request)?;
            self.route_stream(
                &metadata,
                &self.configured_request(request),
                on_event,
                is_cancelled,
            )
        })();
        if let Err(error) = &result {
            let _ = on_event(ModelStreamEvent::ResponseFailed {
                error: error.to_string(),
            });
        }
        result
    }
}

fn validate_request(
    capabilities: &ModelCapabilities,
    request: &ModelRequest,
) -> Result<(), ProviderError> {
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
    if request
        .reasoning
        .as_ref()
        .is_some_and(|reasoning| reasoning.effort.is_some() || reasoning.budget_tokens.is_some())
        && !capabilities.configurable_reasoning_effort
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
    Ok(())
}

fn parse_catalog(
    product: OpenCodeProduct,
    metadata: &Value,
    available: &Value,
) -> Result<CachedCatalog, ProviderError> {
    let provider = metadata.get(product.provider_key()).ok_or_else(|| {
        invalid_response(product, "metadata catalog did not contain this provider")
    })?;
    let model_catalog = provider
        .get("models")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_response(product, "metadata catalog did not contain a model map"))?;
    let available_models = available
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response(product, "model listing did not contain a data array"))?;
    let available_ids = available_models
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .collect::<std::collections::HashSet<_>>();

    let mut by_id = HashMap::new();
    for (id, model) in model_catalog {
        let protocol_name = model
            .get("provider")
            .and_then(|provider| provider.get("npm"))
            .and_then(Value::as_str)
            .or_else(|| provider.get("npm").and_then(Value::as_str));
        let Some(protocol) = protocol_name.and_then(parse_protocol) else {
            continue;
        };
        let descriptor = ModelDescriptor {
            provider: product.namespace().to_owned(),
            id: format!("{}/{id}", product.namespace()),
            display_name: model
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_owned(),
            capabilities: model_capabilities(model),
            metadata: Default::default(),
        };
        by_id.insert(
            id.clone(),
            ModelMetadata {
                descriptor,
                protocol,
            },
        );
    }
    let mut visible = available_ids
        .iter()
        .filter_map(|id| by_id.get(*id).map(|metadata| metadata.descriptor.clone()))
        .collect::<Vec<_>>();
    visible.sort_by(|left, right| left.display_name.cmp(&right.display_name));
    visible.dedup_by(|left, right| left.id == right.id);
    Ok(CachedCatalog {
        refreshed_at: Instant::now(),
        available: visible,
        by_id,
    })
}

fn parse_protocol(package: &str) -> Option<WireProtocol> {
    match package {
        "@ai-sdk/openai" | "ai-sdk:openai" => Some(WireProtocol::Responses),
        "@ai-sdk/openai-compatible" | "ai-sdk:openai-compatible" => {
            Some(WireProtocol::ChatCompletions)
        }
        "@ai-sdk/anthropic" | "ai-sdk:anthropic" => Some(WireProtocol::Messages),
        _ => None,
    }
}

fn model_capabilities(model: &Value) -> ModelCapabilities {
    let limit = model.get("limit").unwrap_or(&Value::Null);
    let reasoning = model
        .get("reasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let tool_calling = model
        .get("tool_call")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    ModelCapabilities {
        text_input: true,
        image_input: model
            .get("attachment")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        streaming: true,
        tool_calling,
        // The catalog exposes whether tools exist, but does not state whether
        // a model supports parallel calls. Keep that fact unknown in the
        // registry instead of inferring it from ordinary tool support.
        parallel_tool_calls: false,
        reasoning,
        // OpenCode's endpoint docs list wire protocols, but do not promise that
        // model-native reasoning controls pass through those gateways.
        configurable_reasoning_effort: false,
        context_window: limit
            .get("context")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        max_output_tokens: limit
            .get("output")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        system_instructions: true,
        developer_instructions: true,
        prompt_caching: model
            .get("prompt_caching")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        structured_output: model
            .get("structured_output")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn strip_namespace(product: OpenCodeProduct, model: &str) -> &str {
    model
        .strip_prefix(&format!("{}/", product.namespace()))
        .unwrap_or(model)
}

fn invalid_response(product: OpenCodeProduct, reason: &str) -> ProviderError {
    ProviderError::InvalidResponse {
        provider: product.auth_provider(),
        reason: reason.to_owned(),
    }
}

fn http_agent(timeout: Duration, read_timeout: Option<Duration>) -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_write(REQUEST_TIMEOUT)
        .timeout(timeout);
    if let Some(read_timeout) = read_timeout {
        builder = builder.timeout_read(read_timeout);
    }
    builder.build()
}

struct OpenAIChatCompletionsTransport {
    base_url: String,
    api_key_env: String,
    api_key: Option<String>,
    provider: &'static str,
    request_agent: ureq::Agent,
    stream_agent: ureq::Agent,
    stream_timeout: Duration,
}

impl fmt::Debug for OpenAIChatCompletionsTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAIChatCompletionsTransport")
            .field("base_url", &self.base_url)
            .field("api_key_env", &self.api_key_env)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("provider", &self.provider)
            .finish()
    }
}

impl OpenAIChatCompletionsTransport {
    fn new(base_url: &str, api_key_env: &str, api_key: String, provider: &'static str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key_env: api_key_env.to_owned(),
            api_key: (!api_key.trim().is_empty()).then_some(api_key),
            provider,
            request_agent: http_agent(REQUEST_TIMEOUT, None),
            stream_agent: http_agent(REQUEST_TIMEOUT, Some(STREAM_READ_TIMEOUT)),
            stream_timeout: STREAM_TIMEOUT,
        }
    }

    fn auth_headers(&self) -> Result<Vec<(&'static str, String)>, ProviderError> {
        let key = self
            .api_key
            .as_deref()
            .ok_or_else(|| ProviderError::MissingApiKey {
                provider: self.provider,
                env_var: self.api_key_env.clone(),
            })?;
        Ok(vec![
            ("Authorization", format!("Bearer {key}")),
            ("Content-Type", "application/json".to_owned()),
        ])
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn complete_request(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        let headers = self.auth_headers()?;
        let headers = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect::<Vec<_>>();
        let body = chat_request_body(request, false)?;
        let response = request_json(
            &self.request_agent,
            "POST",
            &self.endpoint(),
            &headers,
            &body,
            self.provider,
        )?;
        let value = response
            .into_json::<Value>()
            .map_err(|_| chat_invalid(self.provider, "response body was not valid JSON"))?;
        parse_chat_response(&value, &request.model)
    }

    fn stream_request(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ModelResponse, ProviderError> {
        if is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let headers = self.auth_headers()?;
        let headers = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect::<Vec<_>>();
        let body = chat_request_body(request, true)?;
        let response = request_json(
            &self.stream_agent,
            "POST",
            &self.endpoint(),
            &headers,
            &body,
            self.provider,
        )?;
        let mut state = ChatStreamState::new(self.provider, &request.model);
        let reader = BufReader::new(response.into_reader());
        for_each_sse_data_cancellable(
            reader,
            self.provider,
            Some(self.stream_timeout),
            is_cancelled,
            |data| state.handle(data, on_event),
        )?;
        if is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        state.finish(on_event)
    }
}

impl ProtocolAdapter for OpenAIChatCompletionsTransport {
    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.complete_request(request)
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        self.stream_request(request, on_event, &|| false)
    }

    fn stream_cancellable(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ModelResponse, ProviderError> {
        self.stream_request(request, on_event, is_cancelled)
    }
}

fn chat_request_body(request: &ModelRequest, stream: bool) -> Result<Value, ProviderError> {
    if request
        .reasoning
        .as_ref()
        .is_some_and(|reasoning| reasoning.budget_tokens.is_some())
    {
        return Err(ProviderError::UnsupportedCapability {
            capability: "reasoning token budget for Chat Completions",
        });
    }
    let messages = request
        .messages
        .iter()
        .map(chat_message)
        .collect::<Result<Vec<_>, _>>()?;
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": stream,
    });
    if let Some(max_output_tokens) = request.max_output_tokens {
        body["max_completion_tokens"] = json!(max_output_tokens);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if !request.tools.is_empty() {
        body["tools"] = json!(request
            .tools
            .iter()
            .map(|tool| json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }
            }))
            .collect::<Vec<_>>());
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(true);
    }
    if stream {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if let Some(effort) = request
        .reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.effort)
    {
        body["reasoning_effort"] = json!(chat_reasoning_effort(effort));
    }
    Ok(body)
}

fn chat_message(message: &Message) -> Result<Value, ProviderError> {
    let role = match message.role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    if message.role == Role::Tool {
        let call_id = message
            .tool_call_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                chat_invalid("opencode", "tool result was missing its call identifier")
            })?;
        let content = content_text(&message.content);
        let content = if message.is_error {
            format!("Tool execution error: {content}")
        } else {
            content
        };
        return Ok(json!({ "role": role, "tool_call_id": call_id, "content": content }));
    }
    let mut result = json!({ "role": role, "content": chat_content(&message.content) });
    if let Some(name) = &message.name {
        result["name"] = json!(name);
    }
    if !message.tool_calls.is_empty() {
        result["tool_calls"] = json!(message
            .tool_calls
            .iter()
            .map(|call| json!({
                "id": call.id,
                "type": "function",
                "function": { "name": call.name, "arguments": call.arguments.to_string() }
            }))
            .collect::<Vec<_>>());
    }
    Ok(result)
}

fn chat_content(blocks: &[ContentBlock]) -> Value {
    if blocks.iter().all(|block| {
        matches!(
            block,
            ContentBlock::Text { .. } | ContentBlock::Reasoning { .. }
        )
    }) {
        return json!(blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.as_str(),
                ContentBlock::Image { .. } => "",
            })
            .collect::<String>());
    }
    Value::Array(
        blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } | ContentBlock::Reasoning { text } => {
                    json!({ "type": "text", "text": text })
                }
                ContentBlock::Image { media_type, data } => json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{media_type};base64,{data}") }
                }),
            })
            .collect(),
    )
}

fn parse_chat_response(
    value: &Value,
    selected_model: &str,
) -> Result<ModelResponse, ProviderError> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| chat_invalid("opencode", "response did not contain a choice"))?;
    let message = choice
        .get("message")
        .ok_or_else(|| chat_invalid("opencode", "choice was missing its message"))?;
    let mut content = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) if !text.is_empty() => {
            content.push(ContentBlock::Text { text: text.clone() })
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    content.push(ContentBlock::Text {
                        text: text.to_owned(),
                    });
                }
            }
        }
        _ => {}
    }
    let calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .map(parse_chat_tool_call)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(ModelResponse {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("chatcmpl-opencode")
            .to_owned(),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(selected_model)
            .to_owned(),
        content,
        finish_reason: chat_finish_reason(
            choice.get("finish_reason").and_then(Value::as_str),
            !calls.is_empty(),
        ),
        tool_calls: calls,
        usage: value
            .get("usage")
            .filter(|usage| !usage.is_null())
            .map(parse_chat_usage),
    })
}

fn parse_chat_tool_call(value: &Value) -> Result<ToolCall, ProviderError> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| chat_invalid("opencode", "tool call was missing its ID"))?;
    let function = value
        .get("function")
        .ok_or_else(|| chat_invalid("opencode", "tool call was missing its function"))?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| chat_invalid("opencode", "tool call was missing its function name"))?;
    let arguments = match function.get("arguments") {
        Some(Value::String(arguments)) => serde_json::from_str(arguments)
            .map_err(|_| chat_invalid("opencode", "tool arguments were not valid JSON"))?,
        Some(arguments) => arguments.clone(),
        None => json!({}),
    };
    if !arguments.is_object() {
        return Err(chat_invalid(
            "opencode",
            "tool arguments were not a JSON object",
        ));
    }
    Ok(ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments,
    })
}

#[derive(Default)]
struct PartialChatToolCall {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

struct ChatStreamState<'a> {
    provider: &'static str,
    selected_model: &'a str,
    started: bool,
    id: Option<String>,
    model: Option<String>,
    finish_reason: Option<String>,
    content: String,
    usage: Option<Usage>,
    calls: BTreeMap<u32, PartialChatToolCall>,
}

impl<'a> ChatStreamState<'a> {
    fn new(provider: &'static str, selected_model: &'a str) -> Self {
        Self {
            provider,
            selected_model,
            started: false,
            id: None,
            model: None,
            finish_reason: None,
            content: String::new(),
            usage: None,
            calls: BTreeMap::new(),
        }
    }

    fn handle(
        &mut self,
        data: &str,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<bool, ProviderError> {
        if data == "[DONE]" {
            return Ok(false);
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| chat_invalid(self.provider, "stream event was not valid JSON"))?;
        if value.get("error").is_some() {
            return Err(ProviderError::Request {
                provider: self.provider,
                status: None,
            });
        }
        if !self.started {
            self.id = value.get("id").and_then(Value::as_str).map(str::to_owned);
            self.model = value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned);
            self.started = true;
            on_event(ModelStreamEvent::ResponseStarted {
                id: self.id.clone(),
                model: self
                    .model
                    .clone()
                    .unwrap_or_else(|| self.selected_model.to_owned()),
            })?;
        }
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            let parsed = parse_chat_usage(usage);
            self.usage = Some(parsed.clone());
            on_event(ModelStreamEvent::UsageUpdated { usage: parsed })?;
        }
        if let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        {
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_owned());
            }
            let Some(delta) = choice.get("delta") else {
                return Ok(true);
            };
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                self.content.push_str(text);
                on_event(ModelStreamEvent::TextDelta {
                    text: text.to_owned(),
                })?;
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|index| u32::try_from(index).ok())
                        .ok_or_else(|| {
                            chat_invalid(self.provider, "tool call delta was missing its index")
                        })?;
                    let state = self.calls.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        state.id.push_str(id);
                    }
                    let function = call.get("function");
                    if let Some(name) = function
                        .and_then(|value| value.get("name"))
                        .and_then(Value::as_str)
                    {
                        state.name.push_str(name);
                    }
                    if !state.started {
                        state.started = true;
                        on_event(ModelStreamEvent::ToolCallStarted {
                            index,
                            id: (!state.id.is_empty()).then(|| state.id.clone()),
                            name: (!state.name.is_empty()).then(|| state.name.clone()),
                        })?;
                    }
                    if let Some(arguments) = function
                        .and_then(|value| value.get("arguments"))
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                    {
                        state.arguments.push_str(arguments);
                        on_event(ModelStreamEvent::ToolCallArgumentsDelta {
                            index,
                            delta: arguments.to_owned(),
                        })?;
                    }
                }
            }
        }
        Ok(true)
    }

    fn finish(
        mut self,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        if !self.started {
            return Err(chat_invalid(
                self.provider,
                "stream ended before a response started",
            ));
        }
        let mut tool_calls = Vec::new();
        for (index, partial) in self.calls {
            let call = parse_chat_tool_call(&json!({
                "id": partial.id,
                "function": { "name": partial.name, "arguments": partial.arguments }
            }))
            .map_err(|_| chat_invalid(self.provider, "streamed tool call was incomplete"))?;
            on_event(ModelStreamEvent::ToolCallCompleted {
                index,
                call: call.clone(),
            })?;
            tool_calls.push(call);
        }
        if let Some(usage) = &self.usage {
            on_event(ModelStreamEvent::UsageUpdated {
                usage: usage.clone(),
            })?;
        }
        let finish_reason =
            chat_finish_reason(self.finish_reason.as_deref(), !tool_calls.is_empty());
        on_event(ModelStreamEvent::ResponseCompleted {
            finish_reason: finish_reason.clone(),
        })?;
        Ok(ModelResponse {
            id: self
                .id
                .take()
                .unwrap_or_else(|| "chatcmpl-opencode".to_owned()),
            model: self
                .model
                .take()
                .unwrap_or_else(|| self.selected_model.to_owned()),
            content: (!self.content.is_empty())
                .then_some(ContentBlock::Text { text: self.content })
                .into_iter()
                .collect(),
            tool_calls,
            finish_reason,
            usage: self.usage,
        })
    }
}

fn chat_finish_reason(reason: Option<&str>, has_calls: bool) -> FinishReason {
    match reason {
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls") => FinishReason::ToolCalls,
        Some("content_filter") => FinishReason::ContentFilter,
        Some(other) => FinishReason::Other(other.to_owned()),
        None if has_calls => FinishReason::ToolCalls,
        None => FinishReason::Stop,
    }
}

fn parse_chat_usage(value: &Value) -> Usage {
    let input = value
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    let output = value
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: value
            .get("total_tokens")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .or_else(|| input.zip(output).map(|(a, b)| a.saturating_add(b))),
        cache_read_tokens: value
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        cache_creation_tokens: None,
    }
}

fn content_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => Some(text.as_str()),
            ContentBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn chat_reasoning_effort(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::XHigh => "xhigh",
        ReasoningEffort::Max => "max",
    }
}

fn chat_invalid(provider: &'static str, reason: &str) -> ProviderError {
    ProviderError::InvalidResponse {
        provider,
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};

    const TEST_KEY: &str = "opencode-test-secret-never-log";

    struct MockResponse {
        status: u16,
        body: String,
        hold_open: bool,
    }

    impl MockResponse {
        fn json(value: Value) -> Self {
            Self {
                status: 200,
                body: value.to_string(),
                hold_open: false,
            }
        }

        fn status(status: u16) -> Self {
            Self {
                status,
                body: "{}".to_owned(),
                hold_open: false,
            }
        }

        fn sse(values: &[Value]) -> Self {
            Self {
                status: 200,
                body: values
                    .iter()
                    .map(|value| format!("data: {value}\n\n"))
                    .collect(),
                hold_open: false,
            }
        }

        fn held_sse(value: Value) -> Self {
            Self {
                status: 200,
                body: format!("data: {value}\n\n"),
                hold_open: true,
            }
        }
    }

    struct CapturedRequest {
        path: String,
        headers: String,
        body: Value,
    }

    struct MockServer {
        root: String,
        captures: Arc<Mutex<Vec<CapturedRequest>>>,
        join: Option<JoinHandle<()>>,
    }

    impl MockServer {
        fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("mock server bind");
            let address = listener.local_addr().expect("mock server address");
            let captures = Arc::new(Mutex::new(Vec::new()));
            let thread_captures = Arc::clone(&captures);
            let join = thread::spawn(move || {
                for response in responses {
                    let (mut stream, _) = listener.accept().expect("accept HTTP request");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .expect("read timeout");
                    let (path, headers, body) = read_request(&mut stream);
                    let parsed_body = if body.is_empty() {
                        json!({})
                    } else {
                        serde_json::from_slice(&body).expect("request JSON")
                    };
                    thread_captures
                        .lock()
                        .expect("capture mutex")
                        .push(CapturedRequest {
                            path,
                            headers,
                            body: parsed_body,
                        });
                    let reason = match response.status {
                        200 => "OK",
                        401 => "Unauthorized",
                        429 => "Too Many Requests",
                        500 => "Internal Server Error",
                        _ => "Mock Status",
                    };
                    let content_type = if response.body.starts_with("data:") {
                        "text/event-stream"
                    } else {
                        "application/json"
                    };
                    if response.hold_open {
                        write!(
                            stream,
                            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: 100000\r\nConnection: keep-alive\r\n\r\n",
                            response.status, reason, content_type
                        ).expect("held response headers");
                        stream
                            .write_all(response.body.as_bytes())
                            .expect("held response body");
                        stream.flush().expect("flush held response");
                        let mut byte = [0_u8; 1];
                        while stream.read(&mut byte).unwrap_or(0) > 0 {}
                        continue;
                    }
                    write!(
                        stream,
                        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response.status, reason, content_type, response.body.len()
                    ).expect("response headers");
                    stream
                        .write_all(response.body.as_bytes())
                        .expect("response body");
                    stream.flush().expect("flush response");
                }
            });
            Self {
                root: format!("http://{address}"),
                captures,
                join: Some(join),
            }
        }

        fn request(&self, index: usize) -> CapturedRequestView {
            let requests = self.captures.lock().expect("capture mutex");
            let request = &requests[index];
            CapturedRequestView {
                path: request.path.clone(),
                headers: request.headers.clone(),
                body: request.body.clone(),
            }
        }

        fn count(&self) -> usize {
            self.captures.lock().expect("capture mutex").len()
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    struct CapturedRequestView {
        path: String,
        headers: String,
        body: Value,
    }

    fn read_request(stream: &mut TcpStream) -> (String, String, Vec<u8>) {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        let (header_end, content_length) = loop {
            let count = stream.read(&mut chunk).expect("read request");
            assert_ne!(count, 0, "client closed before sending headers");
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let header_text = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = header_text
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                break (header_end, content_length);
            }
        };
        let expected_body_length = header_end + 4 + content_length;
        while bytes.len() < expected_body_length {
            let count = stream.read(&mut chunk).expect("read request body");
            assert_ne!(count, 0, "request ended before the declared body length");
            bytes.extend_from_slice(&chunk[..count]);
        }
        let header_text = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
        let path = header_text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_owned();
        (
            path,
            header_text,
            bytes[header_end + 4..expected_body_length].to_vec(),
        )
    }

    fn catalog_value(product: OpenCodeProduct, id: &str, package: &str) -> Value {
        json!({
            (product.provider_key()): {
                "npm": "@ai-sdk/openai-compatible",
                "models": {
                    (id): {
                        "name": "Discovered coding model",
                        "tool_call": true,
                        "attachment": true,
                        "reasoning": true,
                        "reasoning_options": [{ "type": "effort", "values": ["low", "high"] }],
                        "limit": { "context": 65536, "output": 8192 },
                        "provider": { "npm": package }
                    },
                    "ignored-google-model": {
                        "name": "Other protocol",
                        "provider": { "npm": "@ai-sdk/google" }
                    }
                }
            }
        })
    }

    fn listing_value(id: &str) -> Value {
        json!({ "object": "list", "data": [{ "id": id, "object": "model", "owned_by": "opencode" }] })
    }

    #[test]
    fn catalog_tool_support_does_not_fabricate_parallel_call_support() {
        let catalog = parse_catalog(
            OpenCodeProduct::Zen,
            &catalog_value(OpenCodeProduct::Zen, "test-model", "@ai-sdk/openai"),
            &listing_value("test-model"),
        )
        .unwrap();
        let descriptor = &catalog.available[0];
        assert!(descriptor.capabilities.tool_calling);
        assert!(!descriptor.capabilities.parallel_tool_calls);
        assert_eq!(
            descriptor.metadata.capabilities.parallel_tool_calls,
            crate::CapabilityKnowledge::Unknown
        );
    }

    fn response_for(protocol: WireProtocol, id: &str) -> Value {
        let call1 =
            json!({ "id": "out-1", "name": "read_file", "arguments": { "path": "README.md" } });
        let call2 =
            json!({ "id": "out-2", "name": "read_file", "arguments": { "path": "Cargo.toml" } });
        match protocol {
            WireProtocol::Responses => json!({
                "id": "resp-id", "model": id, "status": "completed",
                "output": [
                    { "type": "message", "content": [{ "type": "output_text", "text": "finished" }] },
                    { "type": "function_call", "call_id": call1["id"], "name": call1["name"], "arguments": call1["arguments"].to_string() },
                    { "type": "function_call", "call_id": call2["id"], "name": call2["name"], "arguments": call2["arguments"].to_string() }
                ],
                "usage": { "input_tokens": 4, "output_tokens": 6 }
            }),
            WireProtocol::ChatCompletions => json!({
                "id": "chat-id", "model": id,
                "choices": [{
                    "message": { "content": "finished", "tool_calls": [
                        { "id": call1["id"], "type": "function", "function": { "name": call1["name"], "arguments": call1["arguments"].to_string() } },
                        { "id": call2["id"], "type": "function", "function": { "name": call2["name"], "arguments": call2["arguments"].to_string() } }
                    ] },
                    "finish_reason": "tool_calls"
                }],
                "usage": { "prompt_tokens": 4, "completion_tokens": 6, "total_tokens": 10 }
            }),
            WireProtocol::Messages => json!({
                "type": "message", "id": "msg-id", "model": id,
                "content": [
                    { "type": "text", "text": "finished" },
                    { "type": "tool_use", "id": call1["id"], "name": call1["name"], "input": call1["arguments"] },
                    { "type": "tool_use", "id": call2["id"], "name": call2["name"], "input": call2["arguments"] }
                ],
                "stop_reason": "tool_use",
                "usage": { "input_tokens": 4, "output_tokens": 6 }
            }),
        }
    }

    fn request_with_tool_round_trip() -> ModelRequest {
        let mut request = ModelRequest::new(
            "placeholder",
            vec![Message::user_text("inspect both files")],
        );
        request.tools.push(super::super::ToolDefinition {
            name: "read_file".to_owned(),
            description: "Read a workspace file".to_owned(),
            input_schema: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        });
        request.messages.push(Message::assistant_tool_calls(vec![
            ToolCall {
                id: "prior-1".to_owned(),
                name: "read_file".to_owned(),
                arguments: json!({ "path": "a" }),
            },
            ToolCall {
                id: "prior-2".to_owned(),
                name: "read_file".to_owned(),
                arguments: json!({ "path": "b" }),
            },
        ]));
        request
            .messages
            .push(Message::tool_result(super::super::ToolResult {
                tool_call_id: "prior-1".to_owned(),
                content: "first file".to_owned(),
                is_error: false,
            }));
        request
            .messages
            .push(Message::tool_result(super::super::ToolResult {
                tool_call_id: "prior-2".to_owned(),
                content: "second file".to_owned(),
                is_error: true,
            }));
        request
    }

    fn route_case(product: OpenCodeProduct, package: &str, expected_path: &str) {
        let id = "test-model";
        let server = MockServer::start(vec![
            MockResponse::json(catalog_value(product, id, package)),
            MockResponse::json(listing_value(id)),
            MockResponse::json(response_for(
                protocol_from_package(package).expect("known package"),
                id,
            )),
        ]);
        let base_url = format!(
            "{}{}",
            server.root,
            if product == OpenCodeProduct::Zen {
                "/zen/v1"
            } else {
                "/zen/go/v1"
            }
        );
        let provider = OpenCodeProvider::with_api_key_and_catalog_url(
            product,
            &base_url,
            format!("{}/{id}", product.namespace()),
            "OPENCODE_API_KEY",
            TEST_KEY,
            format!("{}/api.json", server.root),
        );
        let response = provider
            .complete(&request_with_tool_round_trip())
            .expect("gateway response");
        assert_eq!(response.text(), "finished");
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(
            response.usage.as_ref().and_then(|usage| usage.total_tokens),
            Some(10)
        );
        let catalog_request = server.request(0);
        assert_eq!(catalog_request.path, "/api.json");
        let listing_request = server.request(1);
        assert_eq!(
            listing_request.path,
            format!(
                "{}/models",
                if product == OpenCodeProduct::Zen {
                    "/zen/v1"
                } else {
                    "/zen/go/v1"
                }
            )
        );
        assert!(listing_request
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer opencode-test-secret-never-log"));
        let routed = server.request(2);
        assert_eq!(routed.path, expected_path);
        assert_eq!(routed.body["model"], id);
        match protocol_from_package(package).expect("known protocol") {
            WireProtocol::Responses => {
                assert_eq!(routed.body["tools"][0]["name"], "read_file");
                assert!(routed.body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["type"] == "function_call_output"));
            }
            WireProtocol::ChatCompletions => {
                assert_eq!(routed.body["tools"][0]["function"]["name"], "read_file");
                assert!(routed.body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["role"] == "tool" && item["tool_call_id"] == "prior-1"));
            }
            WireProtocol::Messages => {
                assert_eq!(routed.body["tools"][0]["input_schema"]["type"], "object");
                assert!(routed.body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(
                        |item| item["content"].as_array().is_some_and(|blocks| blocks
                            .iter()
                            .any(|block| block["type"] == "tool_result"))
                    ));
            }
        }
        assert!(!format!("{provider:?}").contains(TEST_KEY));
    }

    fn streamed_text_for(protocol: WireProtocol, id: &str) -> MockResponse {
        let events = match protocol {
            WireProtocol::Responses => vec![
                json!({ "type": "response.created", "response": { "id": "routed-stream" } }),
                json!({ "type": "response.output_text.delta", "delta": "streamed" }),
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "routed-stream",
                        "model": id,
                        "status": "completed",
                        "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "streamed" }] }],
                        "usage": { "input_tokens": 2, "output_tokens": 3, "total_tokens": 5 }
                    }
                }),
            ],
            WireProtocol::ChatCompletions => vec![
                json!({ "id": "routed-stream", "model": id, "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }] }),
                json!({ "id": "routed-stream", "model": id, "choices": [{ "index": 0, "delta": { "content": "streamed" }, "finish_reason": null }] }),
                json!({ "id": "routed-stream", "model": id, "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
                json!({ "id": "routed-stream", "model": id, "choices": [], "usage": { "prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5 } }),
                json!("[DONE]"),
            ],
            WireProtocol::Messages => vec![
                json!({ "type": "message_start", "message": { "id": "routed-stream", "type": "message", "role": "assistant", "model": id, "content": [], "stop_reason": null, "usage": { "input_tokens": 2, "output_tokens": 1 } } }),
                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "streamed" } }),
                json!({ "type": "content_block_stop", "index": 0 }),
                json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 3 } }),
                json!({ "type": "message_stop" }),
            ],
        };
        MockResponse::sse(&events)
    }

    fn stream_route_case(product: OpenCodeProduct, package: &str, expected_path: &str) {
        let id = "stream-model";
        let protocol = protocol_from_package(package).expect("known protocol");
        let server = MockServer::start(vec![
            MockResponse::json(catalog_value(product, id, package)),
            MockResponse::json(listing_value(id)),
            streamed_text_for(protocol, id),
        ]);
        let product_path = if product == OpenCodeProduct::Zen {
            "/zen/v1"
        } else {
            "/zen/go/v1"
        };
        let provider = OpenCodeProvider::with_api_key_and_catalog_url(
            product,
            format!("{}{product_path}", server.root),
            format!("{}/{id}", product.namespace()),
            "OPENCODE_API_KEY",
            TEST_KEY,
            format!("{}/api.json", server.root),
        );
        let mut events = Vec::new();
        let response = provider
            .stream(
                &ModelRequest::new("ignored", vec![Message::user_text("stream a response")]),
                &mut |event| {
                    events.push(event);
                    Ok(())
                },
            )
            .expect("gateway streaming response");
        assert_eq!(response.text(), "streamed");
        assert_eq!(
            response.usage.as_ref().and_then(|usage| usage.total_tokens),
            Some(5)
        );
        assert!(events.iter().any(
            |event| matches!(event, ModelStreamEvent::TextDelta { text } if text == "streamed")
        ));
        assert!(events
            .iter()
            .any(|event| matches!(event, ModelStreamEvent::ResponseCompleted { .. })));
        let routed = server.request(2);
        assert_eq!(routed.path, expected_path);
        assert_eq!(routed.body["stream"], true);
        assert_eq!(server.count(), 3);
    }

    fn protocol_from_package(package: &str) -> Option<WireProtocol> {
        parse_protocol(package)
    }

    #[test]
    fn routes_zen_and_go_models_by_catalog_protocol_and_round_trips_multiple_tools() {
        route_case(OpenCodeProduct::Zen, "@ai-sdk/openai", "/zen/v1/responses");
        route_case(
            OpenCodeProduct::Zen,
            "@ai-sdk/openai-compatible",
            "/zen/v1/chat/completions",
        );
        route_case(
            OpenCodeProduct::Zen,
            "@ai-sdk/anthropic",
            "/zen/v1/messages",
        );
        route_case(
            OpenCodeProduct::Go,
            "@ai-sdk/openai",
            "/zen/go/v1/responses",
        );
        route_case(
            OpenCodeProduct::Go,
            "@ai-sdk/openai-compatible",
            "/zen/go/v1/chat/completions",
        );
        route_case(
            OpenCodeProduct::Go,
            "@ai-sdk/anthropic",
            "/zen/go/v1/messages",
        );
    }

    #[test]
    fn routes_streaming_text_and_usage_through_each_protocol_for_zen_and_go() {
        for (product, package, path) in [
            (OpenCodeProduct::Zen, "@ai-sdk/openai", "/zen/v1/responses"),
            (
                OpenCodeProduct::Zen,
                "@ai-sdk/openai-compatible",
                "/zen/v1/chat/completions",
            ),
            (
                OpenCodeProduct::Zen,
                "@ai-sdk/anthropic",
                "/zen/v1/messages",
            ),
            (
                OpenCodeProduct::Go,
                "@ai-sdk/openai",
                "/zen/go/v1/responses",
            ),
            (
                OpenCodeProduct::Go,
                "@ai-sdk/openai-compatible",
                "/zen/go/v1/chat/completions",
            ),
            (
                OpenCodeProduct::Go,
                "@ai-sdk/anthropic",
                "/zen/go/v1/messages",
            ),
        ] {
            stream_route_case(product, package, path);
        }
    }

    #[test]
    fn discovery_joins_account_visible_ids_with_protocol_metadata_caches_and_refreshes() {
        for product in [OpenCodeProduct::Zen, OpenCodeProduct::Go] {
            let server = MockServer::start(vec![
                MockResponse::json(catalog_value(product, "public-model", "@ai-sdk/openai")),
                MockResponse::json(json!({ "data": [
                    { "id": "public-model" }, { "id": "ignored-google-model" }, { "id": "no-metadata" }
                ] })),
                MockResponse::json(catalog_value(product, "public-model", "@ai-sdk/openai")),
                MockResponse::json(listing_value("public-model")),
            ]);
            let base_url = format!(
                "{}{}",
                server.root,
                if product == OpenCodeProduct::Zen {
                    "/zen/v1"
                } else {
                    "/zen/go/v1"
                }
            );
            let provider = OpenCodeProvider::with_api_key_and_catalog_url(
                product,
                &base_url,
                format!("{}/public-model", product.namespace()),
                "OPENCODE_API_KEY",
                TEST_KEY,
                format!("{}/api.json", server.root),
            );
            let listed = provider.discover_models().expect("discover models");
            assert_eq!(listed.len(), 1);
            assert_eq!(
                listed[0].id,
                format!("{}/public-model", product.namespace())
            );
            assert_eq!(listed[0].display_name, "Discovered coding model");
            assert_eq!(listed[0].capabilities.context_window, Some(65536));
            assert_eq!(listed[0].capabilities.max_output_tokens, Some(8192));
            assert!(listed[0].capabilities.tool_calling);
            assert!(!listed[0].capabilities.configurable_reasoning_effort);
            assert_eq!(
                provider.discover_models().expect("cached discovery"),
                listed
            );
            assert_eq!(server.count(), 2);
            assert_eq!(
                provider.refresh_models().expect("refresh discovery"),
                listed
            );
            assert_eq!(server.count(), 4);
        }
    }

    #[test]
    fn chat_completions_streams_text_parallel_calls_usage_and_tool_results() {
        let server = MockServer::start(vec![MockResponse::sse(&[
            json!({ "id": "stream-1", "model": "chat-model", "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }] }),
            json!({ "id": "stream-1", "model": "chat-model", "choices": [{ "index": 0, "delta": { "content": "Reading" }, "finish_reason": null }] }),
            json!({ "id": "stream-1", "model": "chat-model", "choices": [{ "index": 0, "delta": { "tool_calls": [
                { "index": 0, "id": "tool-a", "type": "function", "function": { "name": "read_file", "arguments": "{\"path\":" } },
                { "index": 1, "id": "tool-b", "type": "function", "function": { "name": "search", "arguments": "{\"query\":" } }
            ] }, "finish_reason": null }] }),
            json!({ "id": "stream-1", "model": "chat-model", "choices": [{ "index": 0, "delta": { "tool_calls": [
                { "index": 0, "function": { "arguments": "\"README.md\"}" } },
                { "index": 1, "function": { "arguments": "\"TODO\"}" } }
            ] }, "finish_reason": "tool_calls" }] }),
            json!({ "id": "stream-1", "model": "chat-model", "choices": [], "usage": { "prompt_tokens": 9, "completion_tokens": 5, "total_tokens": 14 } }),
            json!("[DONE]"),
        ])]);
        let transport = OpenAIChatCompletionsTransport::new(
            &format!("{}/zen/go/v1", server.root),
            "OPENCODE_API_KEY",
            TEST_KEY.to_owned(),
            "opencode-go",
        );
        let request = request_with_tool_round_trip();
        let mut events = Vec::new();
        let response = ProtocolAdapter::stream(&transport, &request, &mut |event| {
            events.push(event);
            Ok(())
        })
        .expect("streaming Chat response");
        assert_eq!(response.text(), "Reading");
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].arguments["path"], "README.md");
        assert_eq!(response.tool_calls[1].arguments["query"], "TODO");
        assert_eq!(
            response.usage.as_ref().and_then(|usage| usage.total_tokens),
            Some(14)
        );
        assert!(events.iter().any(
            |event| matches!(event, ModelStreamEvent::TextDelta { text } if text == "Reading")
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ModelStreamEvent::ToolCallCompleted { .. }))
                .count(),
            2
        );
        let sent = server.request(0);
        assert_eq!(sent.path, "/zen/go/v1/chat/completions");
        assert_eq!(
            sent.body["tools"][0]["function"]["parameters"]["type"],
            "object"
        );
        assert!(sent.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool"));
        assert_eq!(sent.body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn chat_completions_normalizes_stream_failures_and_observes_cancellation() {
        let error_server = MockServer::start(vec![MockResponse::sse(&[
            json!({ "id": "stream-error", "model": "chat-model", "choices": [{ "delta": { "content": "before error" }, "finish_reason": null }] }),
            json!({ "error": { "message": "upstream echoed opencode-test-secret-never-log" } }),
        ])]);
        let transport = OpenAIChatCompletionsTransport::new(
            &format!("{}/v1", error_server.root),
            "OPENCODE_API_KEY",
            TEST_KEY.to_owned(),
            "opencode-zen",
        );
        let mut events = Vec::new();
        let error = ProtocolAdapter::stream(
            &transport,
            &ModelRequest::new("model", vec![Message::user_text("x")]),
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )
        .expect_err("stream should fail");
        assert!(matches!(
            error,
            ProviderError::Request {
                provider: "opencode-zen",
                status: None
            }
        ));
        assert!(!error.to_string().contains(TEST_KEY));

        let cancel_server = MockServer::start(vec![MockResponse::held_sse(json!({
            "id": "stream-cancel", "model": "chat-model", "choices": [{ "delta": { "content": "cancel me" }, "finish_reason": null }]
        }))]);
        let transport = OpenAIChatCompletionsTransport::new(
            &format!("{}/v1", cancel_server.root),
            "OPENCODE_API_KEY",
            TEST_KEY.to_owned(),
            "opencode-go",
        );
        let cancelled = AtomicBool::new(false);
        let error = ProtocolAdapter::stream_cancellable(
            &transport,
            &ModelRequest::new("model", vec![Message::user_text("x")]),
            &mut |event| {
                if matches!(event, ModelStreamEvent::TextDelta { .. }) {
                    cancelled.store(true, Ordering::SeqCst);
                }
                Ok(())
            },
            &|| cancelled.load(Ordering::SeqCst),
        )
        .expect_err("cancellation should interrupt the stream");
        assert!(matches!(error, ProviderError::Cancelled));
    }

    #[test]
    fn chat_completions_retries_rate_limits_and_keeps_auth_errors_secret_safe() {
        let server = MockServer::start(vec![
            MockResponse::status(429),
            MockResponse::status(429),
            MockResponse::status(429),
        ]);
        let transport = OpenAIChatCompletionsTransport::new(
            &format!("{}/v1", server.root),
            "OPENCODE_API_KEY",
            TEST_KEY.to_owned(),
            "opencode-zen",
        );
        let error = ProtocolAdapter::complete(
            &transport,
            &ModelRequest::new("chat-model", vec![Message::user_text("hello")]),
        )
        .expect_err("repeated rate limit should be normalized");
        assert!(matches!(
            error,
            ProviderError::Request {
                provider: "opencode-zen",
                status: Some(429)
            }
        ));
        assert!(!error.to_string().contains(TEST_KEY));
        assert_eq!(
            server.count(),
            3,
            "transient rate limits retry a bounded number of times"
        );

        let missing_key = OpenAIChatCompletionsTransport::new(
            &format!("{}/v1", server.root),
            "OPENCODE_API_KEY",
            String::new(),
            "opencode-go",
        );
        let error = ProtocolAdapter::complete(
            &missing_key,
            &ModelRequest::new("chat-model", vec![Message::user_text("hello")]),
        )
        .expect_err("missing credentials should stop before HTTP");
        assert!(matches!(
            error,
            ProviderError::MissingApiKey {
                provider: "opencode-go",
                ..
            }
        ));
        assert!(!error.to_string().contains(TEST_KEY));
    }

    #[test]
    fn config_defaults_use_shared_credential_and_separate_product_endpoints() {
        let zen = ModelConfig::for_provider(super::super::ProviderKind::OpenCodeZen);
        let go = ModelConfig::for_provider(super::super::ProviderKind::OpenCodeGo);
        assert_eq!(zen.api_key_env, "OPENCODE_API_KEY");
        assert_eq!(go.api_key_env, "OPENCODE_API_KEY");
        assert_ne!(zen.base_url, go.base_url);
        assert_eq!(zen.model, "opencode-zen/gpt-5.6-sol");
        assert_eq!(go.model, "opencode-go/glm-5.3");
        assert_eq!(serde_json::to_value(zen.provider).unwrap(), "opencode-zen");
        assert_eq!(serde_json::to_value(go.provider).unwrap(), "opencode-go");
    }
}
