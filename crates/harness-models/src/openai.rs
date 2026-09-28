use std::collections::BTreeMap;
use std::fmt;
use std::io::BufReader;
use std::time::Duration;

use serde_json::{json, Value};

use super::protocol::ProtocolAdapter;
use super::transport::{for_each_sse_data_cancellable, request_get, request_json};
use super::{
    ContentBlock, FinishReason, Message, ModelCapabilities, ModelConfig, ModelDescriptor,
    ModelProvider, ModelRequest, ModelResponse, ModelStreamEvent, ProviderError, ReasoningEffort,
    Role, ToolCall, Usage,
};

const PROVIDER: &str = "openai";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const STREAM_READ_TIMEOUT: Duration = Duration::from_millis(250);
const STREAM_TIMEOUT: Duration = Duration::from_secs(600);

/// Native OpenAI provider backed by the Responses API.
pub struct OpenAIProvider {
    transport: OpenAIResponsesTransport,
    model: String,
}

/// Retained spelling for callers that used the earlier provider type.
pub type OpenAiProvider = OpenAIProvider;

/// HTTP transport and private Responses API adapter.
///
/// API keys are kept only in memory and are deliberately omitted from Debug.
pub struct OpenAIResponsesTransport {
    base_url: String,
    api_key_env: String,
    api_key: Option<String>,
    request_agent: ureq::Agent,
    stream_agent: ureq::Agent,
    stream_timeout: Duration,
}

impl fmt::Debug for OpenAIResponsesTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAIResponsesTransport")
            .field("base_url", &self.base_url)
            .field("api_key_env", &self.api_key_env)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("stream_timeout", &self.stream_timeout)
            .finish()
    }
}

impl OpenAIProvider {
    pub fn from_config(config: &ModelConfig) -> Result<Self, ProviderError> {
        config
            .validate()
            .map_err(|reason| ProviderError::Configuration { reason })?;
        Ok(Self {
            transport: OpenAIResponsesTransport::from_env(
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
            transport: OpenAIResponsesTransport::with_api_key(base_url, api_key_env, api_key),
            model: model.into(),
        }
    }

    /// Lists API-available models that the local capability catalog identifies
    /// as suitable for text-based coding-agent work.
    pub fn discover_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        self.transport.discover_models()
    }
}

impl OpenAIResponsesTransport {
    /// Creates a transport using a named environment variable for its key.
    pub fn from_env(base_url: impl Into<String>, api_key_env: impl Into<String>) -> Self {
        let api_key_env = api_key_env.into();
        let api_key = std::env::var(&api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty());
        Self::new(base_url.into(), api_key_env, api_key)
    }

    /// Creates a transport with an in-memory key. Intended for embedding and
    /// tests; the key is never part of requests' error/debug messages.
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
            ("Authorization", format!("Bearer {key}")),
            ("Content-Type", "application/json".to_owned()),
        ])
    }

    fn complete_request(
        &self,
        request: &ModelRequest,
        model: &str,
    ) -> Result<ModelResponse, ProviderError> {
        let headers = self.auth_headers()?;
        let headers = borrowed_headers(&headers);
        let body = request_body(request, model, false)?;
        let response = request_json(
            &self.request_agent,
            "POST",
            &self.endpoint("responses"),
            &headers,
            &body,
            PROVIDER,
        )?;
        let value = response
            .into_json::<Value>()
            .map_err(|_| invalid_response("response body was not valid JSON"))?;
        parse_response(&value, model, include_reasoning_summary(request))
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
        let body = request_body(request, model, true)?;
        let response = request_json(
            &self.stream_agent,
            "POST",
            &self.endpoint("responses"),
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
        state
            .response
            .ok_or_else(|| invalid_response("stream ended before a completed response"))
    }

    /// Queries `GET /models`, then filters it using locally maintained
    /// capability metadata. The endpoint itself does not report tool or
    /// Responses API suitability.
    pub fn discover_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        let headers = self.auth_headers()?;
        let headers = borrowed_headers(&headers);
        let response = request_get(
            &self.request_agent,
            &self.endpoint("models"),
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
        let mut descriptors = models
            .iter()
            .filter_map(|model| model.get("id").and_then(Value::as_str))
            .filter_map(|id| catalog_capabilities(id).map(|capabilities| (id, capabilities)))
            .map(|(id, capabilities)| ModelDescriptor {
                provider: PROVIDER.to_owned(),
                id: id.to_owned(),
                display_name: id.to_owned(),
                capabilities,
                metadata: Default::default(),
            })
            .collect::<Vec<_>>();
        descriptors.sort_by(|left, right| left.id.cmp(&right.id));
        descriptors.dedup_by(|left, right| left.id == right.id);
        Ok(descriptors)
    }
}

impl ModelProvider for OpenAIProvider {
    fn descriptor(&self) -> ModelDescriptor {
        ModelDescriptor {
            provider: PROVIDER.to_owned(),
            id: self.model.clone(),
            display_name: self.model.clone(),
            capabilities: configured_capabilities(&self.model),
            metadata: Default::default(),
        }
    }

    fn discover_models(&self) -> Result<Vec<ModelDescriptor>, ProviderError> {
        OpenAIProvider::discover_models(self)
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

impl ProtocolAdapter for OpenAIResponsesTransport {
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

fn request_body(request: &ModelRequest, model: &str, stream: bool) -> Result<Value, ProviderError> {
    if request
        .reasoning
        .as_ref()
        .is_some_and(|reasoning| reasoning.budget_tokens.is_some())
    {
        return Err(ProviderError::UnsupportedCapability {
            capability: "reasoning token budget",
        });
    }
    let mut input = Vec::new();
    for message in &request.messages {
        input.extend(input_items(message)?);
    }
    let mut body = json!({
        "model": model,
        "input": input,
        "stream": stream,
        // Tool outputs and input messages are supplied explicitly by the
        // harness. Keeping API-side state disabled avoids persisting chats.
        "store": false,
    });
    if let Some(max_output_tokens) = request.max_output_tokens {
        body["max_output_tokens"] = json!(max_output_tokens);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if !request.tools.is_empty() {
        body["tools"] = json!(request.tools.iter().map(openai_tool).collect::<Vec<_>>());
        body["parallel_tool_calls"] = json!(true);
    }
    if let Some(reasoning) = &request.reasoning {
        let mut config = json!({});
        if let Some(effort) = reasoning.effort {
            config["effort"] = json!(reasoning_effort(effort));
        }
        if reasoning.include_summary {
            config["summary"] = json!("auto");
        }
        if !config.as_object().is_some_and(serde_json::Map::is_empty) {
            body["reasoning"] = config;
        }
    }
    Ok(body)
}

fn include_reasoning_summary(request: &ModelRequest) -> bool {
    request
        .reasoning
        .as_ref()
        .is_some_and(|reasoning| reasoning.include_summary)
}

fn reasoning_effort(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::XHigh => "xhigh",
        ReasoningEffort::Max => "max",
    }
}

fn input_items(message: &Message) -> Result<Vec<Value>, ProviderError> {
    if message.role == Role::Tool {
        let call_id = message
            .tool_call_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| invalid_response("tool result was missing its call identifier"))?;
        let output = content_text(&message.content);
        let output = if message.is_error {
            format!("Tool execution error: {output}")
        } else {
            output
        };
        return Ok(vec![json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        })]);
    }

    let mut input = Vec::new();
    if !message.content.is_empty() {
        let content = if message.content.len() == 1 {
            if let ContentBlock::Text { text } = &message.content[0] {
                json!(text)
            } else {
                json!(message
                    .content
                    .iter()
                    .map(input_content)
                    .collect::<Vec<_>>())
            }
        } else {
            json!(message
                .content
                .iter()
                .map(input_content)
                .collect::<Vec<_>>())
        };
        let mut item = json!({ "role": role_name(message.role), "content": content });
        if let Some(name) = &message.name {
            item["name"] = json!(name);
        }
        input.push(item);
    }
    for call in &message.tool_calls {
        input.push(json!({
            "type": "function_call",
            "call_id": call.id,
            "name": call.name,
            "arguments": call.arguments.to_string(),
        }));
    }
    Ok(input)
}

fn input_content(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } | ContentBlock::Reasoning { text } => {
            json!({ "type": "input_text", "text": text })
        }
        ContentBlock::Image { media_type, data } => json!({
            "type": "input_image",
            "image_url": format!("data:{media_type};base64,{data}"),
        }),
    }
}

fn openai_tool(tool: &super::ToolDefinition) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.input_schema,
        "strict": false,
    })
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn parse_response(
    value: &Value,
    selected_model: &str,
    include_reasoning_summary: bool,
) -> Result<ModelResponse, ProviderError> {
    if value.get("status").and_then(Value::as_str) == Some("failed") {
        return Err(invalid_response("provider returned a failed response"));
    }
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response("response did not contain an output array"))?;
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(blocks) = item.get("content").and_then(Value::as_array) {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) == Some("output_text") {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                content.push(ContentBlock::Text {
                                    text: text.to_owned(),
                                });
                            }
                        }
                        if block.get("type").and_then(Value::as_str) == Some("refusal") {
                            if let Some(text) = block.get("refusal").and_then(Value::as_str) {
                                content.push(ContentBlock::Text {
                                    text: text.to_owned(),
                                });
                            }
                        }
                    }
                }
            }
            Some("function_call") => tool_calls.push(parse_tool_call(item)?),
            Some("reasoning") if include_reasoning_summary => {
                if let Some(summaries) = item.get("summary").and_then(Value::as_array) {
                    for summary in summaries {
                        if summary.get("type").and_then(Value::as_str) == Some("summary_text") {
                            if let Some(text) = summary.get("text").and_then(Value::as_str) {
                                content.push(ContentBlock::Reasoning {
                                    text: text.to_owned(),
                                });
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let finish_reason = parse_finish_reason(value, !tool_calls.is_empty());
    Ok(ModelResponse {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("openai-response")
            .to_owned(),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(selected_model)
            .to_owned(),
        content,
        tool_calls,
        finish_reason,
        usage: value
            .get("usage")
            .filter(|usage| !usage.is_null())
            .map(parse_usage),
    })
}

fn parse_tool_call(item: &Value) -> Result<ToolCall, ProviderError> {
    let id = item
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid_response("function call was missing its call identifier"))?;
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid_response("function call was missing its name"))?;
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_response("function call arguments were missing"))?;
    let arguments = serde_json::from_str(arguments)
        .map_err(|_| invalid_response("function call arguments were not valid JSON"))?;
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
        .map(|n| n as u32);
    let output_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    Usage {
        input_tokens,
        output_tokens,
        total_tokens: value
            .get("total_tokens")
            .and_then(Value::as_u64)
            .map(|n| n as u32)
            .or_else(|| {
                input_tokens
                    .zip(output_tokens)
                    .map(|(input, output)| input + output)
            }),
        cache_read_tokens: value
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .map(|n| n as u32),
        cache_creation_tokens: None,
    }
}

fn parse_finish_reason(value: &Value, has_tool_calls: bool) -> FinishReason {
    match value.get("status").and_then(Value::as_str) {
        Some("incomplete") => match value
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => FinishReason::Length,
            Some(reason) => FinishReason::Other(reason.to_owned()),
            None => FinishReason::Length,
        },
        Some("failed") => FinishReason::Error,
        _ if has_tool_calls => FinishReason::ToolCalls,
        _ => FinishReason::Stop,
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

#[derive(Default)]
struct ToolAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    completed: Option<ToolCall>,
}

impl ToolAccumulator {
    fn call_from_fields(&self, args: &str) -> Result<ToolCall, ProviderError> {
        let arguments = serde_json::from_str(args)
            .map_err(|_| invalid_response("streamed function arguments were not valid JSON"))?;
        let id = self
            .id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid_response("streamed function call was missing its id"))?;
        let name = self
            .name
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid_response("streamed function call was missing its name"))?;
        Ok(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments,
        })
    }
}

struct StreamState<'a> {
    model: &'a str,
    include_reasoning_summary: bool,
    started: bool,
    text: String,
    tools: BTreeMap<u32, ToolAccumulator>,
    response: Option<ModelResponse>,
}

impl<'a> StreamState<'a> {
    fn new(model: &'a str, include_reasoning_summary: bool) -> Self {
        Self {
            model,
            include_reasoning_summary,
            started: false,
            text: String::new(),
            tools: BTreeMap::new(),
            response: None,
        }
    }

    fn handle(
        &mut self,
        data: &str,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<bool, ProviderError> {
        let event = serde_json::from_str::<Value>(data)
            .map_err(|_| invalid_response("stream event was not valid JSON"))?;
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "response.created" | "response.in_progress" => {
                self.ensure_started(
                    event.pointer("/response/id").and_then(Value::as_str),
                    on_event,
                )?;
            }
            "response.output_text.delta" => {
                self.ensure_started(None, on_event)?;
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.text.push_str(delta);
                    on_event(ModelStreamEvent::TextDelta {
                        text: delta.to_owned(),
                    })?;
                }
            }
            "response.refusal.delta" => {
                self.ensure_started(None, on_event)?;
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.text.push_str(delta);
                    on_event(ModelStreamEvent::TextDelta {
                        text: delta.to_owned(),
                    })?;
                }
            }
            "response.reasoning_summary_text.delta" => {
                self.ensure_started(None, on_event)?;
                if self.include_reasoning_summary {
                    if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                        on_event(ModelStreamEvent::ReasoningDelta {
                            text: delta.to_owned(),
                        })?;
                    }
                }
            }
            "response.output_item.added" => {
                let index = output_index(&event)?;
                let Some(item) = event.get("item") else {
                    return Err(invalid_response("streamed output item was missing"));
                };
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    self.ensure_started(None, on_event)?;
                    let accumulator = self.tools.entry(index).or_default();
                    accumulator.id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    accumulator.name = item.get("name").and_then(Value::as_str).map(str::to_owned);
                    on_event(ModelStreamEvent::ToolCallStarted {
                        index,
                        id: accumulator.id.clone(),
                        name: accumulator.name.clone(),
                    })?;
                }
            }
            "response.function_call_arguments.delta" => {
                let index = output_index(&event)?;
                self.ensure_started(None, on_event)?;
                let accumulator = self.tools.entry(index).or_default();
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    accumulator.arguments.push_str(delta);
                    on_event(ModelStreamEvent::ToolCallArgumentsDelta {
                        index,
                        delta: delta.to_owned(),
                    })?;
                }
            }
            "response.function_call_arguments.done" => {
                let index = output_index(&event)?;
                let accumulator = self.tools.entry(index).or_default();
                if let Some(arguments) = event.get("arguments").and_then(Value::as_str) {
                    accumulator.arguments = arguments.to_owned();
                }
            }
            "response.output_item.done" => {
                let index = output_index(&event)?;
                if let Some(item) = event.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        self.ensure_started(None, on_event)?;
                        let accumulator = self.tools.entry(index).or_default();
                        accumulator.id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .or(accumulator.id.take());
                        accumulator.name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .or(accumulator.name.take());
                        let arguments = item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or(&accumulator.arguments);
                        let call = accumulator.call_from_fields(arguments)?;
                        accumulator.completed = Some(call.clone());
                        on_event(ModelStreamEvent::ToolCallCompleted { index, call })?;
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                let response_value = event.get("response").ok_or_else(|| {
                    invalid_response("terminal stream event was missing response")
                })?;
                self.ensure_started(response_value.get("id").and_then(Value::as_str), on_event)?;
                let parsed =
                    parse_response(response_value, self.model, self.include_reasoning_summary)?;
                if self.text.is_empty() && !parsed.text().is_empty() {
                    on_event(ModelStreamEvent::TextDelta {
                        text: parsed.text(),
                    })?;
                }
                for (index, call) in parsed.tool_calls.iter().enumerate() {
                    let index = output_call_index(response_value, call, index);
                    if !self
                        .tools
                        .get(&index)
                        .is_some_and(|accumulator| accumulator.completed.is_some())
                    {
                        on_event(ModelStreamEvent::ToolCallStarted {
                            index,
                            id: Some(call.id.clone()),
                            name: Some(call.name.clone()),
                        })?;
                        on_event(ModelStreamEvent::ToolCallArgumentsDelta {
                            index,
                            delta: call.arguments.to_string(),
                        })?;
                        on_event(ModelStreamEvent::ToolCallCompleted {
                            index,
                            call: call.clone(),
                        })?;
                    }
                }
                if let Some(usage) = &parsed.usage {
                    on_event(ModelStreamEvent::UsageUpdated {
                        usage: usage.clone(),
                    })?;
                }
                on_event(ModelStreamEvent::ResponseCompleted {
                    finish_reason: parsed.finish_reason.clone(),
                })?;
                self.response = Some(parsed);
                return Ok(false);
            }
            "response.failed" | "error" => {
                return Err(invalid_response("provider reported a failed response"));
            }
            "response.cancelled" => return Err(ProviderError::Cancelled),
            // Future Responses event types are intentionally ignored. The
            // normalized contract only forwards events understood by harness.
            _ => {}
        }
        Ok(true)
    }

    fn ensure_started(
        &mut self,
        id: Option<&str>,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<(), ProviderError> {
        if !self.started {
            on_event(ModelStreamEvent::ResponseStarted {
                id: id.map(str::to_owned),
                model: self.model.to_owned(),
            })?;
            self.started = true;
        }
        Ok(())
    }
}

fn output_index(event: &Value) -> Result<u32, ProviderError> {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .ok_or_else(|| invalid_response("stream event was missing its output index"))
}

fn output_call_index(response: &Value, call: &ToolCall, fallback: usize) -> u32 {
    response
        .get("output")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().enumerate().find_map(|(index, item)| {
                (item.get("type").and_then(Value::as_str) == Some("function_call")
                    && item.get("call_id").and_then(Value::as_str) == Some(call.id.as_str()))
                .then_some(index as u32)
            })
        })
        .unwrap_or(fallback as u32)
}

/// Conservative local metadata for discovery. The OpenAI model-list endpoint
/// reports availability and ownership, not coding suitability or capabilities.
fn catalog_capabilities(model: &str) -> Option<ModelCapabilities> {
    let model = model.to_ascii_lowercase();
    let excluded = [
        "audio",
        "realtime",
        "transcribe",
        "tts",
        "image",
        "embedding",
        "moderation",
        "search",
        "deep-research",
        "computer-use",
    ];
    if excluded.iter().any(|part| model.contains(part)) {
        return None;
    }
    let gpt_4o = model.starts_with("gpt-4o");
    let gpt_4_1 = model.starts_with("gpt-4.1");
    let gpt_5 = model.starts_with("gpt-5");
    let gpt_6 = model.starts_with("gpt-6");
    let reasoning = gpt_5
        || gpt_6
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4");
    if !(gpt_4o || gpt_4_1 || reasoning) {
        return None;
    }
    Some(ModelCapabilities {
        text_input: true,
        image_input: gpt_4o || gpt_4_1 || gpt_5,
        streaming: true,
        tool_calling: true,
        parallel_tool_calls: true,
        reasoning,
        configurable_reasoning_effort: reasoning,
        system_instructions: true,
        developer_instructions: true,
        prompt_caching: gpt_4_1 || gpt_5,
        structured_output: gpt_4o || gpt_4_1 || gpt_5,
        ..ModelCapabilities::default()
    })
}

fn configured_capabilities(model: &str) -> ModelCapabilities {
    // Explicit configuration always wins over local discovery metadata. An
    // unknown model gets the common Responses features but no speculative
    // image/reasoning/context claims.
    catalog_capabilities(model).unwrap_or(ModelCapabilities {
        text_input: true,
        streaming: true,
        tool_calling: true,
        parallel_tool_calls: true,
        system_instructions: true,
        developer_instructions: true,
        ..ModelCapabilities::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ModelRequest, ToolDefinition};

    #[test]
    fn responses_request_uses_current_wire_shape_and_never_contains_the_api_key() {
        let provider = OpenAIProvider::with_api_key(
            "https://example.invalid/v1",
            "gpt-customer-model",
            "TEST_KEY",
            "super-secret-key",
        );
        let mut request = ModelRequest::new("ignored", vec![Message::user_text("hello")]);
        request.tools.push(ToolDefinition {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            input_schema: json!({ "type": "object" }),
        });
        request.reasoning = Some(crate::ReasoningConfig {
            effort: Some(ReasoningEffort::High),
            ..crate::ReasoningConfig::default()
        });
        let body = request_body(&request, &provider.model, true).unwrap();
        let serialized = body.to_string();

        assert_eq!(body["model"], "gpt-customer-model");
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["strict"], false);
        assert!(!serialized.contains("super-secret-key"));
        assert!(!format!("{:?}", provider.transport).contains("super-secret-key"));
    }

    #[test]
    fn maps_canonical_tool_results_and_tool_calls_to_responses_input_items() {
        let request = ModelRequest::new(
            "gpt-test",
            vec![
                Message::assistant_tool_calls(vec![ToolCall {
                    id: "call-1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: json!({ "path": "README.md" }),
                }]),
                Message::tool_result(crate::ToolResult {
                    tool_call_id: "call-1".to_owned(),
                    content: "contents".to_owned(),
                    is_error: false,
                }),
            ],
        );

        let body = request_body(&request, "gpt-test", false).unwrap();

        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["arguments"], "{\"path\":\"README.md\"}");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["output"], "contents");
    }

    #[test]
    fn response_parsing_normalizes_tools_usage_and_finish_reason() {
        let response = json!({
            "id": "resp-1",
            "model": "gpt-test-snapshot",
            "status": "completed",
            "output": [
                { "type": "message", "content": [{ "type": "output_text", "text": "done" }] },
                { "type": "function_call", "call_id": "call-1", "name": "read_file", "arguments": "{\"path\":\"x\"}" }
            ],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "total_tokens": 13,
                "input_tokens_details": { "cached_tokens": 2 }
            }
        });

        let parsed = parse_response(&response, "fallback", false).unwrap();

        assert_eq!(parsed.text(), "done");
        assert_eq!(parsed.finish_reason, FinishReason::ToolCalls);
        assert_eq!(parsed.tool_calls[0].arguments, json!({ "path": "x" }));
        let usage = parsed.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(3));
        assert_eq!(usage.total_tokens, Some(13));
        assert_eq!(usage.cache_read_tokens, Some(2));
    }

    #[test]
    fn discovery_metadata_filters_unsupported_model_families_but_unknown_config_is_allowed() {
        assert!(catalog_capabilities("gpt-4.1-mini").is_some());
        assert!(catalog_capabilities("gpt-6-astra").is_some());
        assert!(catalog_capabilities("o3-mini").is_some());
        assert!(catalog_capabilities("text-embedding-3-large").is_none());
        let explicit = configured_capabilities("custom-deployment-abc");
        assert!(explicit.text_input);
        assert!(explicit.tool_calling);
        assert!(!explicit.image_input);
        assert_eq!(explicit.context_window, None);
    }

    #[test]
    fn configured_provider_does_not_claim_the_mock_default_context_window() {
        let provider = OpenAIProvider::from_config(&ModelConfig {
            provider: crate::ProviderKind::OpenAi,
            model: "custom-deployment".to_owned(),
            reasoning_effort: None,
            ..ModelConfig::default()
        })
        .unwrap();

        assert_eq!(provider.descriptor().capabilities.context_window, None);
    }
}
