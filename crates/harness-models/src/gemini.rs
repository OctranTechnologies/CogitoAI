//! Native Google Gemini Generate Content API provider.
//!
//! Gemini `Content`, `Part`, function-call, and thought-signature wire data is
//! private to this adapter. The harness sees only canonical messages, tools,
//! usage, and stream events.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::BufReader;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use super::protocol::ProtocolAdapter;
use super::transport::{for_each_sse_data_cancellable, request_get, request_json};
use super::{
    ContentBlock, FinishReason, Message, ModelCapabilities, ModelConfig, ModelDescriptor,
    ModelProvider, ModelRequest, ModelResponse, ModelStreamEvent, ProviderError, ReasoningEffort,
    Role, ToolCall, Usage,
};

const PROVIDER: &str = "gemini";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const STREAM_READ_TIMEOUT: Duration = Duration::from_millis(250);
const STREAM_TIMEOUT: Duration = Duration::from_secs(600);
const MODEL_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_CACHED_TURNS: usize = 256;

/// Native Gemini provider using the Generate Content API.
pub struct GeminiProvider {
    transport: GeminiNativeTransport,
    model: String,
}

/// HTTP transport and private Gemini Generate Content adapter.
///
/// API credentials and Gemini thought signatures are kept in memory only and
/// omitted from debug output, normalized events, and canonical responses.
pub struct GeminiNativeTransport {
    base_url: String,
    api_key_env: String,
    api_key: Option<String>,
    request_agent: ureq::Agent,
    stream_agent: ureq::Agent,
    stream_timeout: Duration,
    next_call_id: AtomicU64,
    assistant_turns: Mutex<HashMap<String, CachedTurn>>,
    model_cache: Mutex<Option<CachedModelList>>,
}

#[derive(Clone)]
struct CachedTurn {
    calls: Vec<ToolCall>,
    raw_parts: Vec<Value>,
    native_calls: HashMap<String, NativeCall>,
}

#[derive(Clone)]
struct NativeCall {
    name: String,
    native_id: Option<String>,
}

struct CachedModelList {
    refreshed_at: Instant,
    models: Vec<ModelDescriptor>,
    metadata: HashMap<String, ModelMetadata>,
}

#[derive(Clone, Debug, Default)]
struct ModelMetadata {
    thinking: Option<bool>,
}

struct ParsedResponse {
    response: ModelResponse,
    raw_parts: Vec<Value>,
    native_calls: HashMap<String, NativeCall>,
}

impl fmt::Debug for GeminiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeminiProvider")
            .field("transport", &self.transport)
            .field("model", &self.model)
            .finish()
    }
}

impl fmt::Debug for GeminiNativeTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GeminiNativeTransport")
            .field("base_url", &self.base_url)
            .field("api_key_env", &self.api_key_env)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("stream_timeout", &self.stream_timeout)
            .finish()
    }
}

impl GeminiProvider {
    pub fn from_config(config: &ModelConfig) -> Result<Self, ProviderError> {
        config
            .validate()
            .map_err(|reason| ProviderError::Configuration { reason })?;
        Ok(Self {
            transport: GeminiNativeTransport::from_env(
                config.base_url.clone(),
                config.api_key_env.clone(),
            ),
            model: config.model.clone(),
        })
    }

    pub fn with_api_key(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key_env: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            transport: GeminiNativeTransport::with_api_key(base_url, api_key_env, api_key),
            model: model.into(),
        }
    }

    /// Returns discovered Generate Content models, using a 15-minute cache.
    pub fn discover_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        self.transport.discover_models()
    }

    /// Bypasses the discovery cache and refreshes model metadata now.
    pub fn refresh_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        self.transport.refresh_models()
    }
}

impl GeminiNativeTransport {
    /// Creates a transport using a named environment variable for its key.
    pub fn from_env(base_url: impl Into<String>, api_key_env: impl Into<String>) -> Self {
        let api_key_env = api_key_env.into();
        let api_key = std::env::var(&api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty());
        Self::new(base_url.into(), api_key_env, api_key)
    }

    /// Creates a transport with an in-memory API key. Intended for embedding
    /// and tests; the key is never returned in Debug output or errors.
    pub fn with_api_key(
        base_url: impl Into<String>,
        api_key_env: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let api_key = api_key.into();
        Self::new(
            base_url.into(),
            api_key_env.into(),
            (!api_key.trim().is_empty()).then_some(api_key),
        )
    }

    fn new(base_url: String, api_key_env: String, api_key: Option<String>) -> Self {
        let request_agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_write(REQUEST_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build();
        let stream_agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_write(REQUEST_TIMEOUT)
            .timeout_read(STREAM_READ_TIMEOUT)
            .build();
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key_env,
            api_key,
            request_agent,
            stream_agent,
            stream_timeout: STREAM_TIMEOUT,
            next_call_id: AtomicU64::new(1),
            assistant_turns: Mutex::new(HashMap::new()),
            model_cache: Mutex::new(None),
        }
    }

    fn endpoint(&self, resource: &str) -> String {
        format!("{}/{resource}", self.base_url)
    }

    fn auth_headers(&self) -> Result<Vec<(&'static str, String)>, ProviderError> {
        let key = self
            .api_key
            .as_deref()
            .ok_or_else(|| ProviderError::MissingApiKey {
                provider: PROVIDER,
                env_var: self.api_key_env.clone(),
            })?;
        Ok(vec![
            ("x-goog-api-key", key.to_owned()),
            ("content-type", "application/json".to_owned()),
        ])
    }

    fn complete_request(
        &self,
        request: &ModelRequest,
        model: &str,
    ) -> Result<ModelResponse, ProviderError> {
        let headers = self.auth_headers()?;
        let headers = borrowed_headers(&headers);
        let metadata = self.model_metadata(model);
        let body = request_body(request, model, &self.cached_turns(), metadata.as_ref())?;
        let response = request_json(
            &self.request_agent,
            "POST",
            &self.generation_endpoint(model, false),
            &headers,
            &body,
            PROVIDER,
        )?;
        let value = response
            .into_json::<Value>()
            .map_err(|_| invalid_response("response body was not valid JSON"))?;
        if value.get("error").is_some() {
            return Err(api_error(&value));
        }
        let parsed = parse_response(
            &value,
            model,
            include_reasoning_summary(request),
            &self.next_call_id,
            None,
        )?;
        self.remember_turn(&parsed);
        Ok(parsed.response)
    }

    fn stream_request(
        &self,
        request: &ModelRequest,
        model: &str,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ModelResponse, ProviderError> {
        if is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let headers = self.auth_headers()?;
        let headers = borrowed_headers(&headers);
        let metadata = self.model_metadata(model);
        let body = request_body(request, model, &self.cached_turns(), metadata.as_ref())?;
        let response = request_json(
            &self.stream_agent,
            "POST",
            &self.generation_endpoint(model, true),
            &headers,
            &body,
            PROVIDER,
        )?;
        let reader = BufReader::new(response.into_reader());
        let mut state = StreamState::new(model, include_reasoning_summary(request));
        for_each_sse_data_cancellable(
            reader,
            PROVIDER,
            Some(self.stream_timeout),
            is_cancelled,
            |data| state.handle(data, on_event, &self.next_call_id),
        )?;
        if is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let parsed = state
            .finish(on_event)?
            .ok_or_else(|| invalid_response("stream ended before a candidate response"))?;
        self.remember_turn(&parsed);
        Ok(parsed.response)
    }

    fn generation_endpoint(&self, model: &str, stream: bool) -> String {
        let model = model.strip_prefix("models/").unwrap_or(model);
        let model = path_encode(model);
        if stream {
            self.endpoint(&format!("models/{model}:streamGenerateContent?alt=sse"))
        } else {
            self.endpoint(&format!("models/{model}:generateContent"))
        }
    }

    fn cached_turns(&self) -> HashMap<String, CachedTurn> {
        self.assistant_turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn remember_turn(&self, parsed: &ParsedResponse) {
        if parsed.response.tool_calls.is_empty() {
            return;
        }
        let cached = CachedTurn {
            calls: parsed.response.tool_calls.clone(),
            raw_parts: parsed.raw_parts.clone(),
            native_calls: parsed.native_calls.clone(),
        };
        let mut turns = self
            .assistant_turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for call in &parsed.response.tool_calls {
            turns.insert(call.id.clone(), cached.clone());
        }
        while turns.len() > MAX_CACHED_TURNS {
            let Some(oldest) = turns.keys().next().cloned() else {
                break;
            };
            turns.remove(&oldest);
        }
    }

    /// Returns cached discovery results until their 15-minute TTL expires.
    pub fn discover_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        if let Some(cached) = self
            .model_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .filter(|cached| cached.refreshed_at.elapsed() < MODEL_CACHE_TTL)
        {
            return Ok(cached.models.clone());
        }
        self.refresh_models()
    }

    /// Forces a fresh request to Gemini's paginated `models.list` endpoint.
    pub fn refresh_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        let headers = self.auth_headers()?;
        let headers = borrowed_headers(&headers);
        let mut token: Option<String> = None;
        let mut visited = std::collections::HashSet::new();
        let mut models = Vec::new();
        let mut metadata = HashMap::new();
        loop {
            let query = match &token {
                Some(token) => format!("models?pageSize=1000&pageToken={}", query_encode(token)),
                None => "models?pageSize=1000".to_owned(),
            };
            let response = request_get(
                &self.request_agent,
                &self.endpoint(&query),
                &headers,
                PROVIDER,
            )?;
            let value = response
                .into_json::<Value>()
                .map_err(|_| invalid_response("model listing was not valid JSON"))?;
            if value.get("error").is_some() {
                return Err(api_error(&value));
            }
            let page = value
                .get("models")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid_response("model listing did not contain a models array"))?;
            for model in page {
                let Some(methods) = model
                    .get("supportedGenerationMethods")
                    .and_then(Value::as_array)
                else {
                    continue;
                };
                let supported_methods = methods
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                if !supported_methods
                    .iter()
                    .any(|method| method == "generateContent")
                {
                    continue;
                }
                let id = model
                    .get("baseModelId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .or_else(|| {
                        model
                            .get("name")
                            .and_then(Value::as_str)
                            .map(|name| name.trim_start_matches("models/"))
                    });
                let Some(id) = id else { continue };
                let thinking = model.get("thinking").and_then(Value::as_bool);
                // The listing field is optional in practice even though it is
                // documented. Retain model-family metadata as a conservative
                // fallback when an endpoint omits it.
                let thinking_supported = thinking.unwrap_or_else(|| known_thinking_model(id));
                let configurable_thinking = thinking_supported && can_configure_thinking(id);
                let descriptor = ModelDescriptor {
                    provider: PROVIDER.to_owned(),
                    id: id.to_owned(),
                    display_name: model
                        .get("displayName")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .unwrap_or(id)
                        .to_owned(),
                    capabilities: ModelCapabilities {
                        text_input: true,
                        image_input: known_image_input_model(id),
                        streaming: supported_methods
                            .iter()
                            .any(|method| method == "streamGenerateContent")
                            || supported_methods
                                .iter()
                                .any(|method| method == "generateContent"),
                        tool_calling: true,
                        parallel_tool_calls: true,
                        reasoning: thinking_supported,
                        configurable_reasoning_effort: configurable_thinking,
                        context_window: token_limit(model.get("inputTokenLimit")),
                        max_output_tokens: token_limit(model.get("outputTokenLimit")),
                        system_instructions: true,
                        developer_instructions: true,
                        prompt_caching: false,
                        structured_output: true,
                    },
                };
                metadata.insert(id.to_owned(), ModelMetadata { thinking });
                models.push(descriptor);
            }
            let Some(next) = value
                .get("nextPageToken")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
            else {
                break;
            };
            if !visited.insert(next.to_owned()) {
                break;
            }
            token = Some(next.to_owned());
        }
        models.sort_by(|left, right| left.id.cmp(&right.id));
        models.dedup_by(|left, right| left.id == right.id);
        let mut cache = self
            .model_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *cache = Some(CachedModelList {
            refreshed_at: Instant::now(),
            models: models.clone(),
            metadata,
        });
        Ok(models)
    }

    fn cached_descriptor(&self, model: &str) -> Option<ModelDescriptor> {
        self.model_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .filter(|cache| cache.refreshed_at.elapsed() < MODEL_CACHE_TTL)
            .and_then(|cache| {
                cache
                    .models
                    .iter()
                    .find(|descriptor| descriptor.id == model)
                    .cloned()
            })
    }

    fn model_metadata(&self, model: &str) -> Option<ModelMetadata> {
        self.model_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .filter(|cache| cache.refreshed_at.elapsed() < MODEL_CACHE_TTL)
            .and_then(|cache| cache.metadata.get(model).cloned())
    }
}

impl ModelProvider for GeminiProvider {
    fn descriptor(&self) -> ModelDescriptor {
        self.transport
            .cached_descriptor(&self.model)
            .unwrap_or_else(|| ModelDescriptor {
                provider: PROVIDER.to_owned(),
                id: self.model.clone(),
                display_name: self.model.clone(),
                capabilities: configured_capabilities(&self.model),
            })
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        let mut request = request.clone();
        request.model.clone_from(&self.model);
        ProtocolAdapter::complete(&self.transport, &request)
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        let mut request = request.clone();
        request.model.clone_from(&self.model);
        ProtocolAdapter::stream(&self.transport, &request, on_event)
    }

    fn stream_cancellable(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ModelResponse, ProviderError> {
        let mut request = request.clone();
        request.model.clone_from(&self.model);
        ProtocolAdapter::stream_cancellable(&self.transport, &request, on_event, is_cancelled)
    }
}

impl ProtocolAdapter for GeminiNativeTransport {
    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.complete_request(request, &request.model)
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        self.stream_request(request, &request.model, on_event, &|| false)
    }

    fn stream_cancellable(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ModelResponse, ProviderError> {
        self.stream_request(request, &request.model, on_event, is_cancelled)
    }
}

fn borrowed_headers<'a>(headers: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect()
}

fn request_body(
    request: &ModelRequest,
    model: &str,
    cached_turns: &HashMap<String, CachedTurn>,
    model_metadata: Option<&ModelMetadata>,
) -> Result<Value, ProviderError> {
    let mut contents = Vec::new();
    let mut system_parts = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::System | Role::Developer => {
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => system_parts.push(json!({ "text": text })),
                        ContentBlock::Image { .. } | ContentBlock::Reasoning { .. } => {
                            return Err(ProviderError::UnsupportedCapability {
                                capability: "non-text system instructions",
                            });
                        }
                    }
                }
            }
            Role::User => contents.push(message_content("user", &message.content, &[])?),
            Role::Assistant => {
                let parts = cached_assistant_parts(message, cached_turns)
                    .unwrap_or(assistant_parts(message)?);
                contents.push(json!({ "role": "model", "parts": parts }));
            }
            Role::Tool => {
                let call_id = message.tool_call_id.as_deref().ok_or_else(|| {
                    invalid_response("tool result was missing its canonical tool call ID")
                })?;
                let native_call = cached_turns
                    .get(call_id)
                    .and_then(|turn| turn.native_calls.get(call_id))
                    .cloned()
                    .or_else(|| {
                        request.messages.iter().find_map(|assistant| {
                            assistant
                                .tool_calls
                                .iter()
                                .find(|call| call.id == call_id)
                                .map(|call| NativeCall {
                                    name: call.name.clone(),
                                    native_id: (!call.id.starts_with("gemini-call-"))
                                        .then(|| call.id.clone()),
                                })
                        })
                    })
                    .or_else(|| {
                        message.name.as_ref().map(|name| NativeCall {
                            name: name.clone(),
                            native_id: None,
                        })
                    })
                    .ok_or_else(|| {
                        invalid_response("tool result did not match a Gemini function call")
                    })?;
                let mut function_response = json!({
                    "name": native_call.name,
                    "response": if message.is_error {
                        json!({ "error": message_text(message) })
                    } else {
                        json!({ "result": message_text(message) })
                    }
                });
                if let Some(native_id) = native_call.native_id {
                    function_response["id"] = json!(native_id);
                }
                contents.push(json!({
                    "role": "function",
                    "parts": [{ "functionResponse": function_response }]
                }));
            }
        }
    }
    if contents.is_empty() {
        return Err(invalid_response(
            "request did not contain any Gemini conversation content",
        ));
    }

    let mut body = json!({ "contents": contents });
    if !system_parts.is_empty() {
        body["systemInstruction"] = json!({ "parts": system_parts });
    }
    if !request.tools.is_empty() {
        let declarations = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": gemini_schema(&tool.input_schema),
                })
            })
            .collect::<Vec<_>>();
        body["tools"] = json!([{ "functionDeclarations": declarations }]);
    }
    let mut generation_config = Map::new();
    generation_config.insert("responseModalities".to_owned(), json!(["TEXT"]));
    if let Some(max_output_tokens) = request.max_output_tokens {
        generation_config.insert("maxOutputTokens".to_owned(), json!(max_output_tokens));
    }
    if let Some(temperature) = request.temperature {
        generation_config.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(reasoning) = &request.reasoning {
        let config = reasoning_config(model, reasoning, model_metadata)?;
        if let Some(config) = config {
            generation_config.insert("thinkingConfig".to_owned(), config);
        }
    }
    if !generation_config.is_empty() {
        body["generationConfig"] = Value::Object(generation_config);
    }
    Ok(body)
}

fn message_content(
    role: &str,
    content: &[ContentBlock],
    extra_parts: &[Value],
) -> Result<Value, ProviderError> {
    let mut parts = content_parts(content)?;
    parts.extend_from_slice(extra_parts);
    Ok(json!({ "role": role, "parts": parts }))
}

fn content_parts(content: &[ContentBlock]) -> Result<Vec<Value>, ProviderError> {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => Ok(json!({ "text": text })),
            ContentBlock::Image { media_type, data } => Ok(json!({
                "inlineData": { "mimeType": media_type, "data": data }
            })),
            ContentBlock::Reasoning { text } => Ok(json!({ "text": text, "thought": true })),
        })
        .collect()
}

fn assistant_parts(message: &Message) -> Result<Vec<Value>, ProviderError> {
    let mut parts = content_parts(&message.content)?;
    for call in &message.tool_calls {
        parts.push(json!({ "functionCall": { "name": call.name, "args": call.arguments } }));
    }
    Ok(parts)
}

fn cached_assistant_parts(
    message: &Message,
    cached_turns: &HashMap<String, CachedTurn>,
) -> Option<Vec<Value>> {
    if message.tool_calls.is_empty() {
        return None;
    }
    let turn = cached_turns.get(&message.tool_calls.first()?.id)?;
    (turn.calls == message.tool_calls).then(|| turn.raw_parts.clone())
}

fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Image { .. } | ContentBlock::Reasoning { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn include_reasoning_summary(request: &ModelRequest) -> bool {
    request
        .reasoning
        .as_ref()
        .is_some_and(|reasoning| reasoning.include_summary)
}

fn gemini_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(object) => {
            const ALLOWED: &[&str] = &[
                "type",
                "format",
                "description",
                "nullable",
                "enum",
                "properties",
                "required",
                "items",
                "minimum",
                "maximum",
                "minItems",
                "maxItems",
                "minLength",
                "maxLength",
                "pattern",
                "propertyOrdering",
            ];
            let mut mapped = Map::new();
            for (key, value) in object {
                if ALLOWED.contains(&key.as_str()) {
                    mapped.insert(key.clone(), gemini_schema(value));
                }
            }
            if !mapped.contains_key("type") {
                mapped.insert("type".to_owned(), json!("object"));
            }
            Value::Object(mapped)
        }
        Value::Array(values) => Value::Array(values.iter().map(gemini_schema).collect()),
        _ => schema.clone(),
    }
}

fn reasoning_config(
    model: &str,
    reasoning: &super::ReasoningConfig,
    metadata: Option<&ModelMetadata>,
) -> Result<Option<Value>, ProviderError> {
    if reasoning.effort.is_none() && reasoning.budget_tokens.is_none() && !reasoning.include_summary
    {
        return Ok(None);
    }
    let thinking_supported = metadata
        .and_then(|metadata| metadata.thinking)
        .unwrap_or_else(|| known_thinking_model(model));
    if !thinking_supported {
        return Err(ProviderError::UnsupportedCapability {
            capability: "reasoning for this Gemini model",
        });
    }
    let mut config = Map::new();
    if reasoning.include_summary {
        config.insert("includeThoughts".to_owned(), json!(true));
    }
    if uses_gemini_thinking_level(model) {
        if reasoning.budget_tokens.is_some() {
            return Err(ProviderError::UnsupportedCapability {
                capability: "Gemini 3 thinking token budgets",
            });
        }
        if let Some(effort) = reasoning.effort {
            if !gemini_three_supports_effort(model, effort) {
                return Err(ProviderError::UnsupportedCapability {
                    capability: "requested Gemini 3 thinking level for this model",
                });
            }
            let level = match effort {
                ReasoningEffort::Minimal => "MINIMAL",
                ReasoningEffort::Low => "LOW",
                ReasoningEffort::Medium => "MEDIUM",
                ReasoningEffort::High => "HIGH",
                ReasoningEffort::XHigh | ReasoningEffort::Max => {
                    return Err(ProviderError::UnsupportedCapability {
                        capability: "Gemini thinking level",
                    });
                }
            };
            config.insert("thinkingLevel".to_owned(), json!(level));
        }
    } else if is_gemini_25(model) {
        if reasoning.effort.is_some() {
            return Err(ProviderError::UnsupportedCapability {
                capability: "Gemini 2.5 effort level; configure a thinking token budget",
            });
        }
        if let Some(budget) = reasoning.budget_tokens {
            if !gemini_25_accepts_budget(model, budget) {
                return Err(ProviderError::Configuration {
                    reason:
                        "Gemini 2.5 thinking budget is outside the selected model's supported range"
                            .to_owned(),
                });
            }
            config.insert("thinkingBudget".to_owned(), json!(budget));
        }
    } else if reasoning.effort.is_some() || reasoning.budget_tokens.is_some() {
        return Err(ProviderError::UnsupportedCapability {
            capability: "configurable Gemini reasoning for this model",
        });
    }
    Ok(Some(Value::Object(config)))
}

fn configured_capabilities(model: &str) -> ModelCapabilities {
    let thinking = known_thinking_model(model);
    ModelCapabilities {
        text_input: true,
        image_input: known_image_input_model(model),
        streaming: true,
        tool_calling: true,
        parallel_tool_calls: true,
        reasoning: thinking,
        configurable_reasoning_effort: thinking && can_configure_thinking(model),
        context_window: None,
        max_output_tokens: None,
        system_instructions: true,
        developer_instructions: true,
        prompt_caching: false,
        structured_output: true,
    }
}

fn known_image_input_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    [
        "gemini-1.5-",
        "gemini-2.0-",
        "gemini-2.5-",
        "gemini-3.",
        "gemini-3-",
    ]
    .iter()
    .any(|family| model.starts_with(family))
}

fn is_gemini_three(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("gemini-3.") || model.starts_with("gemini-3-")
}

fn is_gemini_25(model: &str) -> bool {
    model.to_ascii_lowercase().starts_with("gemini-2.5")
}

fn known_thinking_model(model: &str) -> bool {
    uses_gemini_thinking_level(model) || is_gemini_25(model)
}

fn can_configure_thinking(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    if uses_gemini_thinking_level(&model) {
        return known_gemini_three_effort_model(&model);
    }
    model.starts_with("gemini-2.5-pro") || model.starts_with("gemini-2.5-flash")
}

fn uses_gemini_thinking_level(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    is_gemini_three(&model) || model.starts_with("gemini-robotics-er-2")
}

fn gemini_three_supports_effort(model: &str, effort: ReasoningEffort) -> bool {
    let model = model.to_ascii_lowercase();
    let minimal_or_high_only = model.starts_with("gemini-3.1-flash-lite-image");
    let no_minimal = model.starts_with("gemini-3.8-flash")
        || model.starts_with("gemini-3.7-flash")
        || model.starts_with("gemini-3.1-pro");
    if !known_gemini_three_effort_model(&model) {
        return false;
    }
    match effort {
        ReasoningEffort::Minimal => !no_minimal,
        ReasoningEffort::Low | ReasoningEffort::Medium => !minimal_or_high_only,
        ReasoningEffort::High => true,
        ReasoningEffort::XHigh | ReasoningEffort::Max => false,
    }
}

fn known_gemini_three_effort_model(model: &str) -> bool {
    model.starts_with("gemini-3.8-flash")
        || model.starts_with("gemini-3.7-flash")
        || model.starts_with("gemini-3.1-pro")
        || model.starts_with("gemini-3.1-flash-lite-image")
        || model.starts_with("gemini-3.6-flash")
        || model.starts_with("gemini-3.5-flash")
        || model.starts_with("gemini-3.1-flash-lite")
        || model.starts_with("gemini-3-flash")
        || model.starts_with("gemini-robotics-er-2")
}

fn gemini_25_accepts_budget(model: &str, budget: u32) -> bool {
    let model = model.to_ascii_lowercase();
    if model.starts_with("gemini-2.5-pro") {
        (128..=32_768).contains(&budget)
    } else if model.starts_with("gemini-2.5-flash-lite") {
        budget == 0 || (512..=24_576).contains(&budget)
    } else if model.starts_with("gemini-2.5-flash") {
        budget <= 24_576
    } else {
        false
    }
}

fn token_limit(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(Value::as_u64)
        .and_then(|limit| u32::try_from(limit).ok())
}

fn query_encode(value: &str) -> String {
    percent_encode(value, true)
}

fn path_encode(value: &str) -> String {
    percent_encode(value, false)
}

fn percent_encode(value: &str, query: bool) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            encoded.push(byte as char);
        } else if query && byte == b' ' {
            encoded.push('+');
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn parse_response(
    value: &Value,
    model: &str,
    include_reasoning_summary: bool,
    next_call_id: &AtomicU64,
    id_overrides: Option<&HashMap<usize, String>>,
) -> Result<ParsedResponse, ProviderError> {
    if value.get("error").is_some() {
        return Err(api_error(value));
    }
    let candidate = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first());
    if candidate.is_none()
        && value
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
            .is_none()
    {
        return Err(invalid_response("response did not contain a candidate"));
    }
    let raw_parts = candidate
        .and_then(|candidate| candidate.pointer("/content/parts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let id = value
        .get("responseId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or("gemini-response")
        .to_owned();
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    let mut native_calls = HashMap::new();
    for (part_index, part) in raw_parts.iter().enumerate() {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                if include_reasoning_summary && !text.is_empty() {
                    content.push(ContentBlock::Reasoning {
                        text: text.to_owned(),
                    });
                }
            } else if !text.is_empty() {
                content.push(ContentBlock::Text {
                    text: text.to_owned(),
                });
            }
        }
        let Some(function_call) = part.get("functionCall") else {
            continue;
        };
        let name = function_call
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| invalid_response("function call was missing its name"))?;
        let arguments = function_call
            .get("args")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !arguments.is_object() {
            return Err(invalid_response(
                "function call arguments were not an object",
            ));
        }
        let native_id = function_call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let id = id_overrides
            .and_then(|overrides| overrides.get(&part_index).cloned())
            .or_else(|| native_id.clone())
            .unwrap_or_else(|| {
                format!(
                    "gemini-call-{}",
                    next_call_id.fetch_add(1, Ordering::Relaxed)
                )
            });
        let call = ToolCall {
            id: id.clone(),
            name: name.to_owned(),
            arguments,
        };
        native_calls.insert(
            id,
            NativeCall {
                name: name.to_owned(),
                native_id,
            },
        );
        tool_calls.push(call);
    }
    let usage = parse_usage(value.get("usageMetadata"));
    let finish_reason = if !tool_calls.is_empty() {
        FinishReason::ToolCalls
    } else {
        value
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
            .map(|_| FinishReason::ContentFilter)
            .or_else(|| {
                candidate
                    .and_then(|candidate| candidate.get("finishReason"))
                    .and_then(Value::as_str)
                    .map(map_finish_reason)
            })
            .unwrap_or(FinishReason::Stop)
    };
    let response = ModelResponse {
        id,
        model: value
            .get("modelVersion")
            .and_then(Value::as_str)
            .unwrap_or(model)
            .to_owned(),
        content,
        tool_calls,
        finish_reason,
        usage,
    };
    Ok(ParsedResponse {
        response,
        raw_parts,
        native_calls,
    })
}

fn parse_usage(value: Option<&Value>) -> Option<Usage> {
    let value = value?;
    let input_tokens = token_limit(value.get("promptTokenCount"));
    let output_tokens = token_limit(value.get("candidatesTokenCount"));
    let total_tokens = token_limit(value.get("totalTokenCount"));
    let cache_read_tokens = token_limit(value.get("cachedContentTokenCount"));
    (input_tokens.is_some()
        || output_tokens.is_some()
        || total_tokens.is_some()
        || cache_read_tokens.is_some())
    .then_some(Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        cache_read_tokens,
        cache_creation_tokens: None,
    })
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_owned()),
    }
}

fn api_error(value: &Value) -> ProviderError {
    let status = value
        .pointer("/error/code")
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok());
    ProviderError::Request {
        provider: PROVIDER,
        status,
    }
}

fn invalid_response(reason: &str) -> ProviderError {
    ProviderError::InvalidResponse {
        provider: PROVIDER,
        reason: reason.to_owned(),
    }
}

struct StreamState {
    model: String,
    include_reasoning_summary: bool,
    id: Option<String>,
    started: bool,
    raw_parts: Vec<Value>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    tool_ids: HashMap<usize, String>,
    started_tools: HashSet<usize>,
}

impl StreamState {
    fn new(model: &str, include_reasoning_summary: bool) -> Self {
        Self {
            model: model.to_owned(),
            include_reasoning_summary,
            id: None,
            started: false,
            raw_parts: Vec::new(),
            usage: None,
            finish_reason: None,
            tool_ids: HashMap::new(),
            started_tools: HashSet::new(),
        }
    }

    fn handle(
        &mut self,
        data: &str,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
        next_call_id: &AtomicU64,
    ) -> Result<bool, ProviderError> {
        let event = serde_json::from_str::<Value>(data)
            .map_err(|_| invalid_response("stream event was not valid JSON"))?;
        if event.get("error").is_some() {
            return Err(api_error(&event));
        }
        if let Some(id) = event
            .get("responseId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.id = Some(id.to_owned());
        }
        self.ensure_started(on_event)?;
        let candidate = event
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first());
        if let Some(candidate) = candidate {
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_owned());
            }
            if let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
            {
                for (index, part) in parts.iter().enumerate() {
                    self.merge_raw_part(index, part);
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if part.get("thought").and_then(Value::as_bool) == Some(true) {
                            if self.include_reasoning_summary && !text.is_empty() {
                                on_event(ModelStreamEvent::ReasoningDelta {
                                    text: text.to_owned(),
                                })?;
                            }
                        } else if !text.is_empty() {
                            on_event(ModelStreamEvent::TextDelta {
                                text: text.to_owned(),
                            })?;
                        }
                    }
                    if let Some(function_call) = part.get("functionCall") {
                        let name = function_call.get("name").and_then(Value::as_str);
                        let native_id = function_call.get("id").and_then(Value::as_str);
                        if let Some(name) = name {
                            let id = self.tool_ids.entry(index).or_insert_with(|| {
                                native_id.map(str::to_owned).unwrap_or_else(|| {
                                    format!(
                                        "gemini-call-{}",
                                        next_call_id.fetch_add(1, Ordering::Relaxed)
                                    )
                                })
                            });
                            if self.started_tools.insert(index) {
                                let event_index = self.started_tools.len() as u32 - 1;
                                on_event(ModelStreamEvent::ToolCallStarted {
                                    index: event_index,
                                    id: Some(id.clone()),
                                    name: Some(name.to_owned()),
                                })?;
                            }
                        }
                    }
                }
            }
        }
        if let Some(usage) = parse_usage(event.get("usageMetadata")) {
            if self.usage.as_ref() != Some(&usage) {
                self.usage = Some(usage.clone());
                on_event(ModelStreamEvent::UsageUpdated { usage })?;
            }
        }
        Ok(true)
    }

    fn merge_raw_part(&mut self, index: usize, incoming: &Value) {
        while self.raw_parts.len() <= index {
            self.raw_parts.push(json!({}));
        }
        if !incoming.is_object() {
            self.raw_parts[index] = incoming.clone();
            return;
        }
        if !self.raw_parts[index].is_object() {
            self.raw_parts[index] = json!({});
        }
        let existing = self.raw_parts[index]
            .as_object_mut()
            .expect("part is an object");
        let incoming = incoming.as_object().expect("incoming part is an object");
        for (key, value) in incoming {
            if key == "text" {
                if let Some(delta) = value.as_str() {
                    let previous = existing
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    existing.insert(key.clone(), json!(format!("{previous}{delta}")));
                }
            } else if key == "functionCall" {
                let target = existing.entry(key.clone()).or_insert_with(|| json!({}));
                merge_function_call(target, value);
            } else {
                existing.insert(key.clone(), value.clone());
            }
        }
    }

    fn ensure_started(
        &mut self,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<(), ProviderError> {
        if !self.started {
            on_event(ModelStreamEvent::ResponseStarted {
                id: self.id.clone(),
                model: self.model.clone(),
            })?;
            self.started = true;
        }
        Ok(())
    }

    fn finish(
        self,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<Option<ParsedResponse>, ProviderError> {
        if self.raw_parts.is_empty() {
            return Ok(None);
        }
        let value = json!({
            "responseId": self.id,
            "modelVersion": self.model,
            "candidates": [{
                "content": { "role": "model", "parts": self.raw_parts },
                "finishReason": self.finish_reason
            }],
            "usageMetadata": self.usage
        });
        let parsed = parse_response(
            &value,
            &self.model,
            self.include_reasoning_summary,
            &AtomicU64::new(1),
            Some(&self.tool_ids),
        )?;
        let mut parsed = parsed;
        parsed.response.usage = self.usage.clone();
        for (index, call) in parsed.response.tool_calls.iter().enumerate() {
            let delta = call.arguments.to_string();
            let event_index = index as u32;
            on_event(ModelStreamEvent::ToolCallArgumentsDelta {
                index: event_index,
                delta,
            })?;
            on_event(ModelStreamEvent::ToolCallCompleted {
                index: event_index,
                call: call.clone(),
            })?;
        }
        on_event(ModelStreamEvent::ResponseCompleted {
            finish_reason: parsed.response.finish_reason.clone(),
        })?;
        Ok(Some(parsed))
    }
}

fn merge_function_call(target: &mut Value, incoming: &Value) {
    if !target.is_object() || !incoming.is_object() {
        *target = incoming.clone();
        return;
    }
    let target = target.as_object_mut().expect("checked object");
    let incoming = incoming.as_object().expect("checked object");
    for (key, value) in incoming {
        if key == "args" {
            let old = target.entry(key.clone()).or_insert_with(|| json!({}));
            if let (Some(old), Some(new)) = (old.as_object_mut(), value.as_object()) {
                for (arg, value) in new {
                    old.insert(arg.clone(), value.clone());
                }
            } else {
                target.insert(key.clone(), value.clone());
            }
        } else {
            target.insert(key.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ReasoningConfig, ToolDefinition};

    #[test]
    fn maps_model_families_to_distinct_thinking_controls() {
        let gemini_three = reasoning_config(
            "gemini-3.8-flash",
            &ReasoningConfig {
                effort: Some(ReasoningEffort::High),
                include_summary: true,
                ..ReasoningConfig::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            gemini_three,
            Some(json!({"includeThoughts": true, "thinkingLevel": "HIGH"}))
        );

        let gemini_two_five = reasoning_config(
            "gemini-2.5-flash",
            &ReasoningConfig {
                budget_tokens: Some(4096),
                ..ReasoningConfig::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(gemini_two_five, Some(json!({"thinkingBudget": 4096})));
        assert!(reasoning_config(
            "gemini-3.8-flash",
            &ReasoningConfig {
                budget_tokens: Some(4),
                ..ReasoningConfig::default()
            },
            None,
        )
        .is_err());
        assert!(reasoning_config(
            "gemini-2.5-flash",
            &ReasoningConfig {
                effort: Some(ReasoningEffort::High),
                ..ReasoningConfig::default()
            },
            None,
        )
        .is_err());
        assert!(reasoning_config(
            "gemini-3.8-flash",
            &ReasoningConfig {
                effort: Some(ReasoningEffort::Minimal),
                ..ReasoningConfig::default()
            },
            None,
        )
        .is_err());
        assert!(reasoning_config(
            "gemini-3.1-pro",
            &ReasoningConfig {
                effort: Some(ReasoningEffort::Minimal),
                ..ReasoningConfig::default()
            },
            None,
        )
        .is_err());
        assert!(reasoning_config(
            "gemini-3.1-flash-lite-image",
            &ReasoningConfig {
                effort: Some(ReasoningEffort::Medium),
                ..ReasoningConfig::default()
            },
            None,
        )
        .is_err());
        assert!(reasoning_config(
            "gemini-2.5-pro",
            &ReasoningConfig {
                budget_tokens: Some(0),
                ..ReasoningConfig::default()
            },
            None,
        )
        .is_err());
        assert!(reasoning_config(
            "gemini-2.5-pro",
            &ReasoningConfig {
                budget_tokens: Some(128),
                ..ReasoningConfig::default()
            },
            None,
        )
        .is_ok());
        assert!(reasoning_config(
            "gemini-2.5-flash-lite",
            &ReasoningConfig {
                budget_tokens: Some(128),
                ..ReasoningConfig::default()
            },
            None,
        )
        .is_err());
    }

    #[test]
    fn maps_harness_tools_to_gemini_function_declarations() {
        let request = ModelRequest {
            tools: vec![ToolDefinition {
                name: "read_file".to_owned(),
                description: "Read a workspace file".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                    "additionalProperties": false,
                    "$schema": "ignored"
                }),
            }],
            ..ModelRequest::new("gemini-3.8-flash", vec![Message::user_text("read")])
        };
        let body = request_body(&request, &request.model, &HashMap::new(), None).unwrap();
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["name"],
            "read_file"
        );
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parameters"]["required"][0],
            "path"
        );
        assert!(body["tools"][0]["functionDeclarations"][0]["parameters"]
            .get("additionalProperties")
            .is_none());
        assert!(body.get("stream").is_none());
    }
}
