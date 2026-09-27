//! Native Anthropic Messages API provider.
//!
//! The API wire format, including `tool_use`, `tool_result`, thinking
//! signatures, and SSE events, stays private to this adapter. The rest of the
//! harness sees only canonical messages, tool calls, usage, and stream events.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::BufReader;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use super::protocol::ProtocolAdapter;
use super::transport::{for_each_sse_data_cancellable, request_get, request_json};
use super::{
    ContentBlock, FinishReason, Message, ModelCapabilities, ModelConfig, ModelDescriptor,
    ModelProvider, ModelRequest, ModelResponse, ModelStreamEvent, ProviderError, ReasoningEffort,
    Role, ToolCall, Usage,
};

const PROVIDER: &str = "anthropic";
const API_VERSION: &str = "2023-06-01";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const STREAM_READ_TIMEOUT: Duration = Duration::from_millis(250);
const STREAM_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
const MAX_CACHED_TOOL_TURNS: usize = 256;

/// Anthropic provider using the native Messages API.
pub struct AnthropicProvider {
    transport: AnthropicMessagesTransport,
    model: String,
}

/// HTTP transport and private Messages API protocol adapter.
///
/// Credentials and signed thinking blocks are held in memory only and omitted
/// from Debug output.
pub struct AnthropicMessagesTransport {
    base_url: String,
    api_key_env: String,
    api_key: Option<String>,
    request_agent: ureq::Agent,
    stream_agent: ureq::Agent,
    stream_timeout: Duration,
    /// Signed thinking content must be echoed byte-for-byte when continuing a
    /// tool turn. Keep the provider-native assistant blocks private here.
    assistant_turns: Mutex<HashMap<String, Vec<Value>>>,
}

impl fmt::Debug for AnthropicProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicProvider")
            .field("transport", &self.transport)
            .field("model", &self.model)
            .finish()
    }
}

impl fmt::Debug for AnthropicMessagesTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicMessagesTransport")
            .field("base_url", &self.base_url)
            .field("api_key_env", &self.api_key_env)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("stream_timeout", &self.stream_timeout)
            .finish()
    }
}

impl AnthropicProvider {
    pub fn from_config(config: &ModelConfig) -> Result<Self, ProviderError> {
        config
            .validate()
            .map_err(|reason| ProviderError::Configuration { reason })?;
        Ok(Self {
            transport: AnthropicMessagesTransport::from_env(
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
            transport: AnthropicMessagesTransport::with_api_key(base_url, api_key_env, api_key),
            model: model.into(),
        }
    }

    /// Lists API-available Claude models recognized by the local capability
    /// catalog. Explicitly configured model IDs remain accepted independently.
    pub fn discover_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        self.transport.discover_models()
    }
}

impl AnthropicMessagesTransport {
    /// Creates a transport using the named environment variable for its key.
    pub fn from_env(base_url: impl Into<String>, api_key_env: impl Into<String>) -> Self {
        let api_key_env = api_key_env.into();
        let api_key = std::env::var(&api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty());
        Self::new(base_url.into(), api_key_env, api_key)
    }

    /// Creates a transport with an in-memory key. Intended for embedding and
    /// tests; the key is not placed in errors, events, or Debug output.
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
            assistant_turns: Mutex::new(HashMap::new()),
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
            ("x-api-key", key.to_owned()),
            ("anthropic-version", API_VERSION.to_owned()),
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
        let continuations = self
            .assistant_turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let body = request_body(request, model, false, &continuations)?;
        let response = request_json(
            &self.request_agent,
            "POST",
            &self.endpoint("messages"),
            &headers,
            &body,
            PROVIDER,
        )?;
        let value = response
            .into_json::<Value>()
            .map_err(|_| invalid_response("response body was not valid JSON"))?;
        if value.get("type").and_then(Value::as_str) == Some("error") {
            return Err(stream_error(&value));
        }
        let parsed = parse_response(&value, model, include_reasoning_summary(request))?;
        self.remember_tool_turn(&parsed);
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
        let continuations = self
            .assistant_turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let body = request_body(request, model, true, &continuations)?;
        let response = request_json(
            &self.stream_agent,
            "POST",
            &self.endpoint("messages"),
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
            |data| state.handle(data, on_event),
        )?;
        if is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let parsed = state
            .parsed_response
            .take()
            .ok_or_else(|| invalid_response("stream ended before message_stop"))?;
        self.remember_tool_turn(&parsed);
        Ok(parsed.response)
    }

    fn remember_tool_turn(&self, parsed: &ParsedResponse) {
        if parsed.response.tool_calls.is_empty() {
            return;
        }
        let mut turns = self
            .assistant_turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for call in &parsed.response.tool_calls {
            turns.insert(call.id.clone(), parsed.raw_content.clone());
        }
        while turns.len() > MAX_CACHED_TOOL_TURNS {
            let Some(oldest) = turns.keys().next().cloned() else {
                break;
            };
            turns.remove(&oldest);
        }
    }

    /// Queries the official model-list endpoint and filters it through local
    /// coding-agent capability metadata; the endpoint itself only reports model
    /// availability and descriptive metadata.
    pub fn discover_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        let headers = self.auth_headers()?;
        let headers = borrowed_headers(&headers);
        let mut after_id: Option<String> = None;
        let mut visited_cursors = std::collections::HashSet::new();
        let mut descriptors = Vec::new();

        loop {
            let query = match &after_id {
                Some(cursor) => format!("models?limit=1000&after_id={}", query_encode(cursor)),
                None => "models?limit=1000".to_owned(),
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
            let models = value
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid_response("model listing did not contain a data array"))?;
            descriptors.extend(models.iter().filter_map(|model| {
                let id = model.get("id")?.as_str()?;
                let capabilities = catalog_capabilities(id)?;
                Some(ModelDescriptor {
                    provider: PROVIDER.to_owned(),
                    id: id.to_owned(),
                    display_name: model
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned(),
                    capabilities,
                })
            }));
            let has_more = value.get("has_more").and_then(Value::as_bool) == Some(true);
            if !has_more {
                break;
            }
            let cursor = value
                .get("last_id")
                .and_then(Value::as_str)
                .or_else(|| models.last().and_then(|item| item.get("id"))?.as_str())
                .ok_or_else(|| invalid_response("model listing pagination cursor was missing"))?
                .to_owned();
            if !visited_cursors.insert(cursor.clone()) {
                break;
            }
            after_id = Some(cursor);
        }

        descriptors.sort_by(|left, right| left.id.cmp(&right.id));
        descriptors.dedup_by(|left, right| left.id == right.id);
        Ok(descriptors)
    }
}

impl ModelProvider for AnthropicProvider {
    fn descriptor(&self) -> ModelDescriptor {
        ModelDescriptor {
            provider: PROVIDER.to_owned(),
            id: self.model.clone(),
            display_name: self.model.clone(),
            capabilities: configured_capabilities(&self.model),
        }
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

impl ProtocolAdapter for AnthropicMessagesTransport {
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
    stream: bool,
    continuations: &HashMap<String, Vec<Value>>,
) -> Result<Value, ProviderError> {
    let max_tokens = request
        .max_output_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    if max_tokens == 0 {
        return Err(ProviderError::Configuration {
            reason: "max_output_tokens must be greater than zero".to_owned(),
        });
    }

    let mut system = Vec::new();
    let mut messages: Vec<(String, Vec<Value>)> = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::System | Role::Developer => {
                if !message.tool_calls.is_empty() || message.tool_call_id.is_some() {
                    return Err(invalid_response(
                        "instruction messages cannot contain tool calls",
                    ));
                }
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } | ContentBlock::Reasoning { text } => {
                            system.push(text.as_str());
                        }
                        ContentBlock::Image { .. } => {
                            return Err(ProviderError::UnsupportedCapability {
                                capability: "images in system instructions",
                            });
                        }
                    }
                }
            }
            Role::User => {
                if !message.tool_calls.is_empty() || message.tool_call_id.is_some() {
                    return Err(invalid_response("user messages cannot contain tool calls"));
                }
                let blocks = message
                    .content
                    .iter()
                    .filter_map(user_content_block)
                    .collect::<Vec<_>>();
                if !blocks.is_empty() {
                    append_message(&mut messages, "user", blocks);
                }
            }
            Role::Assistant => {
                if message.tool_call_id.is_some() {
                    return Err(invalid_response(
                        "assistant message contained a tool result ID",
                    ));
                }
                let cached = cached_assistant_content(message, continuations);
                let blocks = cached.unwrap_or_else(|| {
                    let mut blocks = message
                        .content
                        .iter()
                        .filter_map(assistant_content_block)
                        .collect::<Vec<_>>();
                    blocks.extend(message.tool_calls.iter().map(anthropic_tool_use));
                    blocks
                });
                if !blocks.is_empty() {
                    append_message(&mut messages, "assistant", blocks);
                }
            }
            Role::Tool => {
                if !message.tool_calls.is_empty() {
                    return Err(invalid_response(
                        "tool result messages cannot contain tool calls",
                    ));
                }
                let id = message
                    .tool_call_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        invalid_response("tool result was missing its call identifier")
                    })?;
                let content = content_text(&message.content);
                let mut result = json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": content,
                });
                if message.is_error {
                    result["is_error"] = json!(true);
                }
                append_message(&mut messages, "user", vec![result]);
            }
        }
    }
    if !messages.iter().any(|(role, _)| role == "user") {
        return Err(invalid_response(
            "Messages API requests require a user message",
        ));
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages
            .into_iter()
            .map(|(role, content)| json!({ "role": role, "content": content }))
            .collect::<Vec<_>>(),
        "stream": stream,
    });
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    if let Some(temperature) = request.temperature {
        if request.reasoning.is_some() {
            return Err(ProviderError::Configuration {
                reason: "Anthropic thinking cannot be combined with temperature".to_owned(),
            });
        }
        if matches!(thinking_mode(model), ThinkingMode::Adaptive) {
            if temperature != 1.0 {
                return Err(ProviderError::UnsupportedCapability {
                    capability: "temperature control for this model",
                });
            }
            // Newer Claude model families accept only the default value for
            // compatibility; omitting it avoids suggesting that it has an effect.
        } else {
            body["temperature"] = json!(temperature);
        }
    }
    if !request.tools.is_empty() {
        body["tools"] = json!(request.tools.iter().map(anthropic_tool).collect::<Vec<_>>());
    }
    if let Some(reasoning) = &request.reasoning {
        let (thinking, effort) = anthropic_reasoning(request, model, max_tokens, reasoning)?;
        if let Some(thinking) = thinking {
            body["thinking"] = thinking;
        }
        if let Some(effort) = effort {
            body["output_config"] = json!({ "effort": effort });
        }
    }
    Ok(body)
}

fn append_message(messages: &mut Vec<(String, Vec<Value>)>, role: &str, content: Vec<Value>) {
    if let Some((last_role, last_content)) = messages.last_mut().filter(|(last, _)| last == role) {
        let _ = last_role;
        last_content.extend(content);
    } else {
        messages.push((role.to_owned(), content));
    }
}

fn user_content_block(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text } => Some(json!({ "type": "text", "text": text })),
        ContentBlock::Image { media_type, data } => Some(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data },
        })),
        ContentBlock::Reasoning { .. } => None,
    }
}

fn assistant_content_block(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text } => Some(json!({ "type": "text", "text": text })),
        ContentBlock::Image { .. } | ContentBlock::Reasoning { .. } => None,
    }
}

fn anthropic_tool(tool: &super::ToolDefinition) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.input_schema,
    })
}

fn anthropic_tool_use(call: &ToolCall) -> Value {
    json!({
        "type": "tool_use",
        "id": call.id,
        "name": call.name,
        "input": call.arguments,
    })
}

fn cached_assistant_content(
    message: &Message,
    continuations: &HashMap<String, Vec<Value>>,
) -> Option<Vec<Value>> {
    let cached = message
        .tool_calls
        .iter()
        .find_map(|call| continuations.get(&call.id))?;
    let cached_calls = cached
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| {
            Some(ToolCall {
                id: block.get("id")?.as_str()?.to_owned(),
                name: block.get("name")?.as_str()?.to_owned(),
                arguments: block.get("input")?.clone(),
            })
        })
        .collect::<Vec<_>>();
    if cached_calls != message.tool_calls {
        return None;
    }
    let cached_text = cached
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .map(|block| {
            block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
        })
        .collect::<String>();
    let message_text = content_text(&message.content);
    (cached_text == message_text).then(|| cached.clone())
}

fn anthropic_reasoning(
    request: &ModelRequest,
    model: &str,
    max_tokens: u32,
    reasoning: &super::ReasoningConfig,
) -> Result<(Option<Value>, Option<&'static str>), ProviderError> {
    match thinking_mode(model) {
        ThinkingMode::Adaptive => {
            if reasoning.budget_tokens.is_some() {
                return Err(ProviderError::UnsupportedCapability {
                    capability: "fixed reasoning token budgets for this model",
                });
            }
            let effort = reasoning
                .effort
                .map(|effort| map_effort(effort, model))
                .transpose()?;
            let mut thinking = json!({ "type": "adaptive" });
            if reasoning.include_summary {
                thinking["display"] = json!("summarized");
            } else {
                // Do not stream internal reasoning unless a summary was asked
                // for. Signatures remain in private continuation state.
                thinking["display"] = json!("omitted");
            }
            Ok((Some(thinking), effort))
        }
        ThinkingMode::Manual { effort_supported } => {
            let Some(budget) = reasoning
                .budget_tokens
                .or_else(|| Some(default_thinking_budget(max_tokens)))
            else {
                unreachable!()
            };
            if budget < 1_024 || budget >= max_tokens {
                return Err(ProviderError::Configuration {
                    reason: "Anthropic manual thinking budget must be at least 1024 and less than max_output_tokens".to_owned(),
                });
            }
            let effort = reasoning
                .effort
                .map(|effort| {
                    if !effort_supported {
                        return Err(ProviderError::UnsupportedCapability {
                            capability: "reasoning effort for this model",
                        });
                    }
                    map_effort(effort, model)
                })
                .transpose()?;
            let mut thinking = json!({
                "type": "enabled",
                "budget_tokens": budget,
            });
            if reasoning.include_summary {
                thinking["display"] = json!("summarized");
            } else {
                thinking["display"] = json!("omitted");
            }
            Ok((Some(thinking), effort))
        }
        ThinkingMode::Unsupported => {
            let _ = request;
            Err(ProviderError::UnsupportedCapability {
                capability: "reasoning configuration for this model",
            })
        }
    }
}

fn default_thinking_budget(max_tokens: u32) -> u32 {
    (max_tokens / 2).clamp(1_024, 4_096)
}

fn map_effort(effort: ReasoningEffort, model: &str) -> Result<&'static str, ProviderError> {
    match effort {
        ReasoningEffort::Minimal | ReasoningEffort::Low => Ok("low"),
        ReasoningEffort::Medium => Ok("medium"),
        ReasoningEffort::High => Ok("high"),
        ReasoningEffort::XHigh if supports_xhigh(model) => Ok("xhigh"),
        ReasoningEffort::Max if matches!(thinking_mode(model), ThinkingMode::Adaptive) => Ok("max"),
        ReasoningEffort::XHigh | ReasoningEffort::Max => {
            Err(ProviderError::UnsupportedCapability {
                capability: "requested reasoning effort for this model",
            })
        }
    }
}

fn include_reasoning_summary(request: &ModelRequest) -> bool {
    request
        .reasoning
        .as_ref()
        .is_some_and(|reasoning| reasoning.include_summary)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ThinkingMode {
    Unsupported,
    Adaptive,
    Manual { effort_supported: bool },
}

fn thinking_mode(model: &str) -> ThinkingMode {
    let model = model.to_ascii_lowercase();
    if [
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-mythos-preview",
    ]
    .iter()
    .any(|prefix| model.starts_with(prefix))
    {
        return ThinkingMode::Adaptive;
    }
    if [
        "claude-opus-4-5",
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
        "claude-opus-4-",
        "claude-sonnet-4-",
        "claude-haiku-4-",
        "claude-3-7-sonnet",
    ]
    .iter()
    .any(|prefix| model.starts_with(prefix))
    {
        return ThinkingMode::Manual {
            effort_supported: model.starts_with("claude-opus-4-5"),
        };
    }
    ThinkingMode::Unsupported
}

fn supports_xhigh(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    [
        "claude-fable-5-1",
        "claude-mythos-5-1",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-sonnet-5",
    ]
    .iter()
    .any(|prefix| model.starts_with(prefix))
}

/// Conservative provider-maintained model metadata. The official listing
/// endpoint reports availability, not tool, vision, or reasoning support.
fn catalog_capabilities(model: &str) -> Option<ModelCapabilities> {
    let model = model.to_ascii_lowercase();
    if !model.starts_with("claude-")
        || ["embedding", "moderation", "transcribe", "audio", "search"]
            .iter()
            .any(|part| model.contains(part))
    {
        return None;
    }
    let thinking = thinking_mode(&model);
    let known_legacy_model = [
        "claude-3-opus",
        "claude-3-sonnet",
        "claude-3-haiku",
        "claude-3-5-sonnet",
        "claude-3-5-haiku",
    ]
    .iter()
    .any(|prefix| model.starts_with(prefix));
    if thinking == ThinkingMode::Unsupported && !known_legacy_model {
        return None;
    }
    Some(ModelCapabilities {
        text_input: true,
        image_input: !model.starts_with("claude-2") && !model.starts_with("claude-instant"),
        streaming: true,
        tool_calling: true,
        parallel_tool_calls: true,
        reasoning: thinking != ThinkingMode::Unsupported,
        configurable_reasoning_effort: matches!(
            thinking,
            ThinkingMode::Adaptive
                | ThinkingMode::Manual {
                    effort_supported: true
                }
        ),
        system_instructions: true,
        developer_instructions: true,
        prompt_caching: true,
        ..ModelCapabilities::default()
    })
}

fn configured_capabilities(model: &str) -> ModelCapabilities {
    catalog_capabilities(model).unwrap_or(ModelCapabilities {
        // A manually configured model may not yet appear in the metadata
        // catalog. Assume the provider's common Messages features but do not
        // claim vision, reasoning, or model limits without evidence.
        text_input: true,
        image_input: false,
        streaming: true,
        tool_calling: true,
        parallel_tool_calls: true,
        system_instructions: true,
        developer_instructions: true,
        ..ModelCapabilities::default()
    })
}

fn parse_response(
    value: &Value,
    selected_model: &str,
    include_reasoning: bool,
) -> Result<ParsedResponse, ProviderError> {
    if value.get("type").and_then(Value::as_str) != Some("message") {
        return Err(invalid_response("response was not a Messages API message"));
    }
    let raw_content = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response("message content was missing"))?
        .clone();
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    for block in &raw_content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    content.push(ContentBlock::Text {
                        text: text.to_owned(),
                    });
                }
            }
            Some("tool_use") => tool_calls.push(parse_tool_call(block)?),
            Some("thinking") if include_reasoning => {
                if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                    if !text.is_empty() {
                        content.push(ContentBlock::Reasoning {
                            text: text.to_owned(),
                        });
                    }
                }
            }
            // Never expose signed thinking internals or provider-native blocks.
            _ => {}
        }
    }
    let usage = value
        .get("usage")
        .filter(|usage| !usage.is_null())
        .map(parse_usage);
    let finish_reason = parse_finish_reason(
        value.get("stop_reason").and_then(Value::as_str),
        !tool_calls.is_empty(),
    );
    Ok(ParsedResponse {
        response: ModelResponse {
            id: value
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| invalid_response("message ID was missing"))?
                .to_owned(),
            model: value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(selected_model)
                .to_owned(),
            content,
            tool_calls,
            finish_reason,
            usage,
        },
        raw_content,
    })
}

struct ParsedResponse {
    response: ModelResponse,
    raw_content: Vec<Value>,
}

fn parse_tool_call(block: &Value) -> Result<ToolCall, ProviderError> {
    let id = block
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid_response("tool-use block was missing its identifier"))?;
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid_response("tool-use block was missing its name"))?;
    let arguments = block
        .get("input")
        .filter(|input| input.is_object())
        .ok_or_else(|| invalid_response("tool-use input was not a JSON object"))?
        .clone();
    Ok(ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments,
    })
}

fn parse_usage(value: &Value) -> Usage {
    let input_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    let output_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    Usage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens
            .zip(output_tokens)
            .map(|(input, output)| input.saturating_add(output)),
        cache_read_tokens: value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        cache_creation_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
    }
}

fn parse_finish_reason(stop_reason: Option<&str>, has_tool_calls: bool) -> FinishReason {
    match stop_reason {
        Some("end_turn" | "stop_sequence") => FinishReason::Stop,
        Some("max_tokens" | "model_context_window_exceeded") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        Some("refusal") => FinishReason::ContentFilter,
        Some(reason) => FinishReason::Other(reason.to_owned()),
        None if has_tool_calls => FinishReason::ToolCalls,
        None => FinishReason::Stop,
    }
}

fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => Some(text.as_str()),
            ContentBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn invalid_response(reason: &str) -> ProviderError {
    ProviderError::InvalidResponse {
        provider: PROVIDER,
        reason: reason.to_owned(),
    }
}

fn stream_error(value: &Value) -> ProviderError {
    let error_type = value
        .get("error")
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let status = match error_type {
        "authentication_error" => Some(401),
        "permission_error" => Some(403),
        "not_found_error" => Some(404),
        "rate_limit_error" => Some(429),
        "overloaded_error" => Some(529),
        "timeout_error" => Some(504),
        "api_error" => Some(500),
        "invalid_request_error" => Some(400),
        _ => None,
    };
    match status {
        Some(status) => ProviderError::Request {
            provider: PROVIDER,
            status: Some(status),
        },
        None => invalid_response("provider reported a failed stream"),
    }
}

fn query_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

struct StreamBlock {
    raw: Value,
    partial_json: String,
    tool_call_index: Option<u32>,
}

struct StreamState<'a> {
    model: &'a str,
    include_reasoning: bool,
    started: bool,
    message_stopped: bool,
    id: Option<String>,
    response_model: Option<String>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    blocks: BTreeMap<u32, StreamBlock>,
    next_tool_index: u32,
    parsed_response: Option<ParsedResponse>,
}

impl<'a> StreamState<'a> {
    fn new(model: &'a str, include_reasoning: bool) -> Self {
        Self {
            model,
            include_reasoning,
            started: false,
            message_stopped: false,
            id: None,
            response_model: None,
            finish_reason: None,
            usage: None,
            blocks: BTreeMap::new(),
            next_tool_index: 0,
            parsed_response: None,
        }
    }

    fn handle(
        &mut self,
        data: &str,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<bool, ProviderError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|_| invalid_response("stream event was not valid JSON"))?;
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = event
                    .get("message")
                    .ok_or_else(|| invalid_response("message_start was missing its message"))?;
                let id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| invalid_response("message_start was missing its ID"))?;
                self.id = Some(id.to_owned());
                self.response_model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.usage = message.get("usage").map(parse_usage);
                self.started = true;
                on_event(ModelStreamEvent::ResponseStarted {
                    id: Some(id.to_owned()),
                    model: self
                        .response_model
                        .clone()
                        .unwrap_or_else(|| self.model.to_owned()),
                })?;
                if let Some(usage) = self.usage.clone() {
                    on_event(ModelStreamEvent::UsageUpdated { usage })?;
                }
            }
            Some("content_block_start") => {
                self.require_started()?;
                let index = stream_index(&event)?;
                let raw = event
                    .get("content_block")
                    .filter(|block| block.is_object())
                    .ok_or_else(|| invalid_response("content block was missing"))?
                    .clone();
                let tool_call_index = if raw.get("type").and_then(Value::as_str) == Some("tool_use")
                {
                    let call = parse_tool_call(&raw)?;
                    let normalized_index = self.next_tool_index;
                    self.next_tool_index = self.next_tool_index.saturating_add(1);
                    on_event(ModelStreamEvent::ToolCallStarted {
                        index: normalized_index,
                        id: Some(call.id),
                        name: Some(call.name),
                    })?;
                    Some(normalized_index)
                } else {
                    None
                };
                self.blocks.insert(
                    index,
                    StreamBlock {
                        raw,
                        partial_json: String::new(),
                        tool_call_index,
                    },
                );
            }
            Some("content_block_delta") => {
                let index = stream_index(&event)?;
                let block = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(|| invalid_response("content delta referenced an unknown block"))?;
                let delta = event
                    .get("delta")
                    .ok_or_else(|| invalid_response("content delta was missing"))?;
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            append_json_string(&mut block.raw, "text", text);
                            if !text.is_empty() {
                                on_event(ModelStreamEvent::TextDelta {
                                    text: text.to_owned(),
                                })?;
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .ok_or_else(|| invalid_response("tool argument delta was missing"))?;
                        block.partial_json.push_str(partial);
                        if !partial.is_empty() {
                            let normalized_index = block.tool_call_index.ok_or_else(|| {
                                invalid_response("tool argument delta referenced a non-tool block")
                            })?;
                            on_event(ModelStreamEvent::ToolCallArgumentsDelta {
                                index: normalized_index,
                                delta: partial.to_owned(),
                            })?;
                        }
                    }
                    Some("thinking_delta") if self.include_reasoning => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            append_json_string(&mut block.raw, "thinking", text);
                            if !text.is_empty() {
                                on_event(ModelStreamEvent::ReasoningDelta {
                                    text: text.to_owned(),
                                })?;
                            }
                        }
                    }
                    Some("thinking_delta") => {
                        // Redact streamed thinking by default.
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            append_json_string(&mut block.raw, "thinking", text);
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                            append_json_string(&mut block.raw, "signature", signature);
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let index = stream_index(&event)?;
                let block = self.blocks.get_mut(&index).ok_or_else(|| {
                    invalid_response("content_block_stop referenced an unknown block")
                })?;
                if block.raw.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let input = if block.partial_json.is_empty() {
                        block.raw.get("input").cloned().unwrap_or_else(|| json!({}))
                    } else {
                        serde_json::from_str(&block.partial_json).map_err(|_| {
                            invalid_response("streamed tool input was not valid JSON")
                        })?
                    };
                    if !input.is_object() {
                        return Err(invalid_response(
                            "streamed tool input was not a JSON object",
                        ));
                    }
                    block.raw["input"] = input;
                    let call = parse_tool_call(&block.raw)?;
                    let normalized_index = block.tool_call_index.ok_or_else(|| {
                        invalid_response("tool-use block had no normalized index")
                    })?;
                    on_event(ModelStreamEvent::ToolCallCompleted {
                        index: normalized_index,
                        call,
                    })?;
                }
            }
            Some("message_delta") => {
                self.require_started()?;
                if let Some(reason) = event
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.finish_reason = Some(reason.to_owned());
                }
                if let Some(update) = event.get("usage") {
                    let mut usage = self.usage.clone().unwrap_or_default();
                    merge_usage(&mut usage, update);
                    self.usage = Some(usage.clone());
                    on_event(ModelStreamEvent::UsageUpdated { usage })?;
                }
            }
            Some("message_stop") => {
                self.require_started()?;
                let content = self
                    .blocks
                    .values()
                    .map(|block| block.raw.clone())
                    .collect::<Vec<_>>();
                let value = json!({
                    "type": "message",
                    "id": self.id,
                    "model": self.response_model.as_deref().unwrap_or(self.model),
                    "content": content,
                    "stop_reason": self.finish_reason,
                    "usage": self.usage,
                });
                let parsed = parse_response(&value, self.model, self.include_reasoning)?;
                if let Some(usage) = &parsed.response.usage {
                    on_event(ModelStreamEvent::UsageUpdated {
                        usage: usage.clone(),
                    })?;
                }
                on_event(ModelStreamEvent::ResponseCompleted {
                    finish_reason: parsed.response.finish_reason.clone(),
                })?;
                self.parsed_response = Some(parsed);
                self.message_stopped = true;
                return Ok(false);
            }
            Some("error") => return Err(stream_error(&event)),
            // `ping` and future event types are deliberately ignored.
            _ => {}
        }
        Ok(true)
    }

    fn require_started(&self) -> Result<(), ProviderError> {
        if self.started {
            Ok(())
        } else {
            Err(invalid_response(
                "stream content arrived before message_start",
            ))
        }
    }
}

fn append_json_string(object: &mut Value, key: &str, addition: &str) {
    let current = object.get(key).and_then(Value::as_str).unwrap_or_default();
    let mut value = String::with_capacity(current.len() + addition.len());
    value.push_str(current);
    value.push_str(addition);
    object[key] = json!(value);
}

fn stream_index(event: &Value) -> Result<u32, ProviderError> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .ok_or_else(|| invalid_response("stream event was missing its block index"))
}

fn merge_usage(usage: &mut Usage, update: &Value) {
    if let Some(value) = update
        .get("input_tokens")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
    {
        usage.input_tokens = Some(value);
    }
    if let Some(value) = update
        .get("output_tokens")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
    {
        usage.output_tokens = Some(value);
    }
    if let Some(value) = update
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
    {
        usage.cache_read_tokens = Some(value);
    }
    if let Some(value) = update
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
    {
        usage.cache_creation_tokens = Some(value);
    }
    usage.total_tokens = usage
        .input_tokens
        .zip(usage.output_tokens)
        .map(|(input, output)| input.saturating_add(output));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ReasoningConfig, ToolDefinition};

    #[test]
    fn maps_messages_system_tools_and_tool_results_to_native_shapes() {
        let request = ModelRequest {
            messages: vec![
                Message {
                    role: Role::System,
                    content: vec![ContentBlock::Text {
                        text: "system rule".to_owned(),
                    }],
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                    is_error: false,
                },
                Message::user_text("inspect the file"),
                Message::assistant_tool_calls(vec![ToolCall {
                    id: "toolu_1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: json!({ "path": "README.md" }),
                }]),
                Message::tool_result(super::super::ToolResult {
                    tool_call_id: "toolu_1".to_owned(),
                    content: "readme contents".to_owned(),
                    is_error: true,
                }),
            ],
            tools: vec![ToolDefinition {
                name: "read_file".to_owned(),
                description: "Read one file".to_owned(),
                input_schema: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            }],
            ..ModelRequest::new("claude-sonnet-4-6", vec![Message::user_text("ignored")])
        };
        let body = request_body(&request, &request.model, true, &HashMap::new()).unwrap();
        assert_eq!(body["system"], "system rule");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_OUTPUT_TOKENS);
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(body["messages"][2]["content"][0]["is_error"], true);
    }

    #[test]
    fn reasoning_configuration_follows_model_capability_metadata() {
        let adaptive = ModelRequest {
            reasoning: Some(ReasoningConfig {
                effort: Some(ReasoningEffort::High),
                include_summary: false,
                ..ReasoningConfig::default()
            }),
            ..ModelRequest::new("claude-sonnet-4-6", vec![Message::user_text("think")])
        };
        let body = request_body(&adaptive, &adaptive.model, false, &HashMap::new()).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "omitted");
        assert_eq!(body["output_config"]["effort"], "high");

        let manual = ModelRequest {
            reasoning: Some(ReasoningConfig {
                budget_tokens: Some(2_048),
                include_summary: true,
                ..ReasoningConfig::default()
            }),
            max_output_tokens: Some(8_192),
            ..ModelRequest::new("claude-sonnet-4-5", vec![Message::user_text("think")])
        };
        let body = request_body(&manual, &manual.model, false, &HashMap::new()).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 2_048);
        assert_eq!(body["thinking"]["display"], "summarized");
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn effort_levels_are_filtered_per_model() {
        assert!(matches!(
            map_effort(ReasoningEffort::Max, "claude-sonnet-4-6"),
            Ok("max")
        ));
        assert!(matches!(
            map_effort(ReasoningEffort::XHigh, "claude-opus-4-8"),
            Ok("xhigh")
        ));
        assert!(matches!(
            map_effort(ReasoningEffort::XHigh, "claude-sonnet-4-6"),
            Err(ProviderError::UnsupportedCapability { .. })
        ));
        assert!(matches!(
            map_effort(ReasoningEffort::XHigh, "claude-mythos-preview"),
            Err(ProviderError::UnsupportedCapability { .. })
        ));
    }

    #[test]
    fn adaptive_models_reject_non_default_temperature() {
        let request = ModelRequest {
            temperature: Some(0.5),
            ..ModelRequest::new("claude-sonnet-4-6", vec![Message::user_text("prompt")])
        };
        assert!(matches!(
            request_body(&request, &request.model, false, &HashMap::new()),
            Err(ProviderError::UnsupportedCapability {
                capability: "temperature control for this model"
            })
        ));
    }

    #[test]
    fn unknown_manual_model_keeps_baseline_capabilities_without_claiming_thinking() {
        let capabilities = configured_capabilities("claude-private-preview");
        assert!(capabilities.text_input);
        assert!(capabilities.tool_calling);
        assert!(!capabilities.reasoning);
        assert!(!capabilities.image_input);
    }

    #[test]
    fn cache_round_trips_unmodified_thinking_signature_privately() {
        let calls = vec![ToolCall {
            id: "toolu_1".to_owned(),
            name: "read_file".to_owned(),
            arguments: json!({ "path": "x" }),
        }];
        let message = Message::assistant_tool_calls(calls);
        let signature = "opaque-signed-thinking";
        let cache = HashMap::from([(
            "toolu_1".to_owned(),
            vec![
                json!({ "type": "thinking", "thinking": "", "signature": signature }),
                anthropic_tool_use(&message.tool_calls[0]),
            ],
        )]);
        let body = request_body(
            &ModelRequest {
                messages: vec![
                    Message::user_text("read"),
                    message,
                    Message::tool_result(super::super::ToolResult {
                        tool_call_id: "toolu_1".to_owned(),
                        content: "contents".to_owned(),
                        is_error: false,
                    }),
                ],
                ..ModelRequest::new("claude-sonnet-4-6", Vec::new())
            },
            "claude-sonnet-4-6",
            false,
            &cache,
        )
        .unwrap();
        assert_eq!(body["messages"][1]["content"][0]["signature"], signature);
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn query_cursor_is_encoded_without_external_url_dependencies() {
        assert_eq!(query_encode("model:id /+"), "model%3Aid%20%2F%2B");
    }
}
