use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use harness_models::{
    AnthropicProvider, FinishReason, Message, ModelProvider, ModelRequest, ModelStreamEvent,
    ProviderError, ReasoningConfig, ReasoningEffort, ToolDefinition, ToolResult,
};
use serde_json::{json, Value};

const TEST_KEY: &str = "anthropic-test-do-not-leak";

struct MockResponse {
    status: u16,
    body: String,
    hold_open: bool,
}

impl MockResponse {
    fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            body: value.to_string(),
            hold_open: false,
        }
    }

    fn sse(events: &[Value]) -> Self {
        Self {
            status: 200,
            body: events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect(),
            hold_open: false,
        }
    }
}

struct CapturedRequest {
    headers: String,
    body: String,
}

struct MockServer {
    base_url: String,
    captures: Arc<Mutex<Vec<CapturedRequest>>>,
    response_sent: mpsc::Receiver<()>,
    join: Option<JoinHandle<()>>,
}

impl MockServer {
    fn start(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock server address");
        let captures = Arc::new(Mutex::new(Vec::new()));
        let thread_captures = Arc::clone(&captures);
        let (sent_tx, sent_rx) = mpsc::channel();
        let join = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept mock request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("set mock read timeout");
                let (headers, body) = read_request(&mut stream);
                thread_captures
                    .lock()
                    .expect("capture lock")
                    .push(CapturedRequest { headers, body });
                let reason = match response.status {
                    200 => "OK",
                    401 => "Unauthorized",
                    403 => "Forbidden",
                    404 => "Not Found",
                    429 => "Too Many Requests",
                    500 => "Internal Server Error",
                    529 => "Overloaded",
                    _ => "Mock Status",
                };
                if response.hold_open {
                    write!(
                        stream,
                        "HTTP/1.1 {} {}\r\nContent-Type: text/event-stream\r\nContent-Length: 100000\r\nConnection: keep-alive\r\n\r\n",
                        response.status, reason
                    )
                    .expect("write held response headers");
                    stream
                        .write_all(response.body.as_bytes())
                        .expect("write held response event");
                    stream.flush().expect("flush held response event");
                    sent_tx.send(()).expect("notify held response started");
                    let mut byte = [0_u8; 1];
                    loop {
                        match stream.read(&mut byte) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                    continue;
                }

                write!(
                    stream,
                    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.status,
                    reason,
                    if response.body.starts_with("data:") {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                    response.body.len()
                )
                .expect("write mock response headers");
                stream
                    .write_all(response.body.as_bytes())
                    .expect("write mock response body");
                stream.flush().expect("flush mock response");
                let _ = sent_tx.send(());
            }
        });
        Self {
            base_url: format!("http://{address}/v1"),
            captures,
            response_sent: sent_rx,
            join: Some(join),
        }
    }

    fn provider(&self, model: &str) -> AnthropicProvider {
        AnthropicProvider::with_api_key(&self.base_url, model, "MOCK_ANTHROPIC_KEY", TEST_KEY)
    }

    fn wait_for_response(&self) {
        self.response_sent
            .recv_timeout(Duration::from_secs(2))
            .expect("mock server sent a response");
    }

    fn request_count(&self) -> usize {
        self.captures.lock().expect("capture lock").len()
    }

    fn request(&self, index: usize) -> CapturedRequestView {
        let captures = self.captures.lock().expect("capture lock");
        let body = if captures[index].body.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&captures[index].body).expect("request JSON")
        };
        CapturedRequestView {
            headers: captures[index].headers.clone(),
            body,
        }
    }
}

struct CapturedRequestView {
    headers: String,
    body: Value,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> (String, String) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut chunk).expect("read mock request");
        assert!(count > 0, "request closed before headers");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = end + 4;
            let header_text = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = header_text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + content_length {
                break (header_end, content_length);
            }
        }
    };
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut chunk).expect("read mock request body");
        assert!(count > 0, "request closed before body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    (
        String::from_utf8_lossy(&bytes[..header_end]).into_owned(),
        String::from_utf8_lossy(&bytes[header_end..header_end + content_length]).into_owned(),
    )
}

fn message_start(id: &str) -> Value {
    json!({
        "type": "message_start",
        "message": {
            "id": id,
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [],
            "stop_reason": null,
            "usage": {
                "input_tokens": 21,
                "output_tokens": 1,
                "cache_read_input_tokens": 3,
                "cache_creation_input_tokens": 2
            }
        }
    })
}

fn text_stream(text: &str) -> MockResponse {
    MockResponse::sse(&[
        message_start("msg-text"),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 8 } }),
        json!({ "type": "message_stop" }),
    ])
}

fn request(model: &str) -> ModelRequest {
    ModelRequest::new(model, vec![Message::user_text("hello")])
}

#[test]
fn complete_normalizes_text_usage_and_required_headers() {
    let server = MockServer::start(vec![MockResponse::json(
        200,
        json!({
            "id": "msg-normal",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [
                { "type": "text", "text": "Hello from Claude." },
                { "type": "thinking", "thinking": "private internals", "signature": "signature-secret" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 32,
                "output_tokens": 9,
                "cache_read_input_tokens": 7,
                "cache_creation_input_tokens": 4
            }
        }),
    )]);
    let provider = server.provider("claude-sonnet-4-6");
    let mut model_request = request("ignored");
    model_request.messages.insert(
        0,
        Message {
            role: harness_models::Role::System,
            content: vec![harness_models::ContentBlock::Text {
                text: "You are concise.".to_owned(),
            }],
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            is_error: false,
        },
    );
    model_request.max_output_tokens = Some(1_024);
    let response = provider.complete(&model_request).unwrap();
    server.wait_for_response();

    assert_eq!(response.text(), "Hello from Claude.");
    assert_eq!(response.finish_reason, FinishReason::Stop);
    let usage = response.usage.as_ref().unwrap();
    assert_eq!(usage.input_tokens, Some(32));
    assert_eq!(usage.output_tokens, Some(9));
    assert_eq!(usage.total_tokens, Some(41));
    assert_eq!(usage.cache_read_tokens, Some(7));
    assert_eq!(usage.cache_creation_tokens, Some(4));
    let captured = server.request(0);
    assert!(captured
        .headers
        .to_ascii_lowercase()
        .contains(&format!("x-api-key: {TEST_KEY}")));
    assert!(captured
        .headers
        .to_ascii_lowercase()
        .contains("anthropic-version: 2023-06-01"));
    assert!(captured
        .headers
        .to_ascii_lowercase()
        .contains("post /v1/messages"));
    assert_eq!(captured.body["model"], "claude-sonnet-4-6");
    assert_eq!(captured.body["max_tokens"], 1_024);
    assert_eq!(captured.body["system"], "You are concise.");
    assert_eq!(captured.body["messages"][0]["role"], "user");
    assert!(!captured.body.to_string().contains(TEST_KEY));
    assert!(!serde_json::to_string(&response)
        .unwrap()
        .contains("signature-secret"));
}

#[test]
fn stream_emits_text_and_cumulative_usage_events() {
    let server = MockServer::start(vec![text_stream("streamed response")]);
    let provider = server.provider("claude-sonnet-4-6");
    let mut events = Vec::new();
    let response = provider
        .generate(&request("claude-sonnet-4-6"), &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
    server.wait_for_response();

    assert_eq!(response.text(), "streamed response");
    assert!(events.iter().any(|event| matches!(
        event,
        ModelStreamEvent::TextDelta { text } if text == "streamed response"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ModelStreamEvent::UsageUpdated { usage }
            if usage.input_tokens == Some(21) && usage.output_tokens == Some(8)
    )));
    assert!(matches!(
        events.last(),
        Some(ModelStreamEvent::ResponseCompleted {
            finish_reason: FinishReason::Stop
        })
    ));
}

#[test]
fn single_tool_use_stream_maps_schema_and_canonical_call() {
    let server = MockServer::start(vec![MockResponse::sse(&[
        message_start("msg-tool"),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "toolu-read", "name": "read_file", "input": {} } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "\"README.md\"}" } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 6 } }),
        json!({ "type": "message_stop" }),
    ])]);
    let provider = server.provider("claude-sonnet-4-6");
    let mut model_request = request("claude-sonnet-4-6");
    model_request.tools.push(ToolDefinition {
        name: "read_file".to_owned(),
        description: "Read a file from the workspace".to_owned(),
        input_schema: json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
    });
    let mut events = Vec::new();
    let response = provider
        .generate(&model_request, &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
    server.wait_for_response();

    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].id, "toolu-read");
    assert_eq!(response.tool_calls[0].name, "read_file");
    assert_eq!(
        response.tool_calls[0].arguments,
        json!({ "path": "README.md" })
    );
    assert!(events.iter().any(|event| matches!(
        event,
        ModelStreamEvent::ToolCallArgumentsDelta { index: 0, delta } if delta.contains("README")
    )));
    assert!(!serde_json::to_string(&events).unwrap().contains("tool_use"));
    let captured = server.request(0);
    assert_eq!(captured.body["tools"][0]["name"], "read_file");
    assert_eq!(captured.body["tools"][0]["input_schema"]["type"], "object");
}

#[test]
fn multiple_tool_use_blocks_are_returned_in_order() {
    let server = MockServer::start(vec![MockResponse::sse(&[
        message_start("msg-tools"),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "toolu-a", "name": "read_file", "input": { "path": "a.txt" } } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "toolu-b", "name": "read_file", "input": { "path": "b.txt" } } }),
        json!({ "type": "content_block_stop", "index": 1 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 7 } }),
        json!({ "type": "message_stop" }),
    ])]);
    let provider = server.provider("claude-sonnet-4-6");
    let response = provider
        .generate(&request("claude-sonnet-4-6"), &mut |_| Ok(()))
        .unwrap();
    server.wait_for_response();

    assert_eq!(
        response
            .tool_calls
            .iter()
            .map(|call| call.id.as_str())
            .collect::<Vec<_>>(),
        ["toolu-a", "toolu-b"]
    );
}

#[test]
fn tool_result_continuation_echoes_signed_blocks_privately() {
    let server = MockServer::start(vec![
        MockResponse::json(
            200,
            json!({
                "id": "msg-tool",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4-6",
                "content": [
                    { "type": "thinking", "thinking": "", "signature": "signed-private-thinking" },
                    { "type": "tool_use", "id": "toolu-roundtrip", "name": "read_file", "input": { "path": "src/lib.rs" } }
                ],
                "stop_reason": "tool_use",
                "usage": { "input_tokens": 21, "output_tokens": 3 }
            }),
        ),
        MockResponse::json(
            200,
            json!({
                "id": "msg-final",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4-6",
                "content": [{ "type": "text", "text": "The file defines the runtime." }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 48, "output_tokens": 8 }
            }),
        ),
    ]);
    let provider = server.provider("claude-sonnet-4-6");
    let first = provider.complete(&request("claude-sonnet-4-6")).unwrap();
    let second_request = ModelRequest {
        messages: vec![
            Message::user_text("Read the file"),
            Message {
                role: harness_models::Role::Assistant,
                content: first.content.clone(),
                name: None,
                tool_call_id: None,
                tool_calls: first.tool_calls.clone(),
                is_error: false,
            },
            Message::tool_result(ToolResult {
                tool_call_id: "toolu-roundtrip".to_owned(),
                content: "pub struct Runtime;".to_owned(),
                is_error: false,
            }),
        ],
        ..ModelRequest::new("claude-sonnet-4-6", Vec::new())
    };
    let second = provider.complete(&second_request).unwrap();
    server.wait_for_response();
    server.wait_for_response();

    assert_eq!(second.text(), "The file defines the runtime.");
    assert!(first.text().is_empty());
    assert!(!serde_json::to_string(&first)
        .unwrap()
        .contains("signed-private-thinking"));
    let follow_up = server.request(1);
    let assistant = &follow_up.body["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"][0]["type"], "thinking");
    assert_eq!(
        assistant["content"][0]["signature"],
        "signed-private-thinking"
    );
    assert_eq!(assistant["content"][1]["type"], "tool_use");
    assert_eq!(assistant["content"][1]["input"]["path"], "src/lib.rs");
    assert_eq!(follow_up.body["messages"][2]["role"], "user");
    assert_eq!(
        follow_up.body["messages"][2]["content"][0]["type"],
        "tool_result"
    );
    assert_eq!(
        follow_up.body["messages"][2]["content"][0]["tool_use_id"],
        "toolu-roundtrip"
    );
}

#[test]
fn adaptive_thinking_is_enabled_only_when_requested_and_effort_is_model_aware() {
    let server = MockServer::start(vec![
        MockResponse::json(
            200,
            json!({
                "id": "msg-disabled", "type": "message", "role": "assistant", "model": "claude-sonnet-4-6",
                "content": [{ "type": "text", "text": "no thinking" }], "stop_reason": "end_turn", "usage": { "input_tokens": 2, "output_tokens": 2 }
            }),
        ),
        MockResponse::json(
            200,
            json!({
                "id": "msg-enabled", "type": "message", "role": "assistant", "model": "claude-sonnet-4-6",
                "content": [{ "type": "thinking", "thinking": "safe summary", "signature": "signature" }, { "type": "text", "text": "done" }],
                "stop_reason": "end_turn", "usage": { "input_tokens": 2, "output_tokens": 2 }
            }),
        ),
    ]);
    let provider = server.provider("claude-sonnet-4-6");
    provider.complete(&request("claude-sonnet-4-6")).unwrap();
    let mut thinking_request = request("claude-sonnet-4-6");
    thinking_request.reasoning = Some(ReasoningConfig {
        effort: Some(ReasoningEffort::High),
        include_summary: true,
        ..ReasoningConfig::default()
    });
    provider.complete(&thinking_request).unwrap();
    server.wait_for_response();
    server.wait_for_response();

    let disabled = server.request(0);
    assert!(disabled.body.get("thinking").is_none());
    let enabled = server.request(1);
    assert_eq!(enabled.body["thinking"]["type"], "adaptive");
    assert_eq!(enabled.body["thinking"]["display"], "summarized");
    assert_eq!(enabled.body["output_config"]["effort"], "high");
}

#[test]
fn invalid_model_and_authentication_errors_are_normalized_and_redacted() {
    for (status, expected) in [(404, 404), (401, 401)] {
        let server = MockServer::start(vec![MockResponse::json(
            status,
            json!({
                "type": "error",
                "error": { "type": if status == 401 { "authentication_error" } else { "not_found_error" }, "message": TEST_KEY }
            }),
        )]);
        let provider = server.provider("claude-model-does-not-exist");
        let error = provider
            .complete(&request("claude-model-does-not-exist"))
            .unwrap_err();
        server.wait_for_response();
        assert!(
            matches!(error, ProviderError::Request { status: Some(actual), .. } if actual == expected)
        );
        assert!(!error.to_string().contains(TEST_KEY));
        assert!(!format!("{provider:?}").contains(TEST_KEY));
    }
}

#[test]
fn rate_limit_retries_before_streaming_and_hides_error_body() {
    let server = MockServer::start(vec![
        MockResponse::json(
            429,
            json!({ "type": "error", "error": { "type": "rate_limit_error", "message": TEST_KEY } }),
        ),
        text_stream("retried"),
    ]);
    let provider = server.provider("claude-sonnet-4-6");
    let response = provider
        .generate(&request("claude-sonnet-4-6"), &mut |_| Ok(()))
        .unwrap();
    server.wait_for_response();
    server.wait_for_response();

    assert_eq!(response.text(), "retried");
    assert_eq!(server.request_count(), 2);
}

#[test]
fn malformed_response_is_normalized_without_server_details() {
    let server = MockServer::start(vec![MockResponse::json(
        200,
        json!({ "id": "msg-bad", "type": "message", "model": "claude-sonnet-4-6", "message": TEST_KEY }),
    )]);
    let provider = server.provider("claude-sonnet-4-6");
    let error = provider
        .complete(&request("claude-sonnet-4-6"))
        .unwrap_err();
    server.wait_for_response();

    assert!(matches!(error, ProviderError::InvalidResponse { .. }));
    assert!(!error.to_string().contains(TEST_KEY));
}

#[test]
fn server_failures_are_retried_and_normalized_without_body_details() {
    let failure = || {
        MockResponse::json(
            500,
            json!({
                "type": "error",
                "error": { "type": "api_error", "message": TEST_KEY }
            }),
        )
    };
    let server = MockServer::start(vec![failure(), failure(), failure()]);
    let provider = server.provider("claude-sonnet-4-6");
    let error = provider
        .complete(&request("claude-sonnet-4-6"))
        .unwrap_err();
    for _ in 0..3 {
        server.wait_for_response();
    }

    assert!(matches!(
        error,
        ProviderError::Request {
            status: Some(500),
            ..
        }
    ));
    assert_eq!(server.request_count(), 3);
    assert!(!error.to_string().contains(TEST_KEY));
}

#[test]
fn streamed_provider_errors_are_normalized_and_redacted() {
    let server = MockServer::start(vec![MockResponse::sse(&[json!({
        "type": "error",
        "error": { "type": "overloaded_error", "message": TEST_KEY }
    })])]);
    let provider = server.provider("claude-sonnet-4-6");
    let mut events = Vec::new();
    let error = provider
        .generate(&request("claude-sonnet-4-6"), &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap_err();
    server.wait_for_response();

    assert!(matches!(
        error,
        ProviderError::Request {
            status: Some(529),
            ..
        }
    ));
    assert!(!serde_json::to_string(&events).unwrap().contains(TEST_KEY));
}

#[test]
fn model_discovery_uses_official_list_and_filters_local_metadata() {
    let server = MockServer::start(vec![MockResponse::json(
        200,
        json!({
            "data": [
                { "id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6" },
                { "id": "claude-opus-4-6" },
                { "id": "claude-private-experiment" },
                { "id": "some-embedding" }
            ],
            "has_more": false,
            "last_id": "some-embedding"
        }),
    )]);
    let provider = server.provider("claude-sonnet-4-6");
    let models = provider.discover_models().unwrap();
    server.wait_for_response();

    assert_eq!(models.len(), 2);
    assert!(models
        .iter()
        .any(|model| model.display_name == "Claude Sonnet 4.6"));
    assert!(models.iter().all(|model| model.capabilities.tool_calling));
    assert!(server
        .request(0)
        .headers
        .to_ascii_lowercase()
        .contains("get /v1/models?limit=1000"));
}

#[test]
fn model_discovery_follows_official_pagination_cursor() {
    let server = MockServer::start(vec![
        MockResponse::json(
            200,
            json!({
                "data": [{ "id": "claude-sonnet-4-6" }],
                "has_more": true,
                "last_id": "claude-sonnet-4-6"
            }),
        ),
        MockResponse::json(
            200,
            json!({
                "data": [{ "id": "claude-opus-4-6" }],
                "has_more": false,
                "last_id": "claude-opus-4-6"
            }),
        ),
    ]);
    let models = server
        .provider("claude-sonnet-4-6")
        .discover_models()
        .unwrap();
    server.wait_for_response();
    server.wait_for_response();

    assert_eq!(models.len(), 2);
    assert!(server
        .request(1)
        .headers
        .to_ascii_lowercase()
        .contains("after_id=claude-sonnet-4-6"));
}

#[test]
fn cancelled_stream_returns_promptly_without_exposing_private_state() {
    let server = MockServer::start(vec![MockResponse {
        status: 200,
        body: format!("data: {}\n\n", message_start("msg-held")),
        hold_open: true,
    }]);
    let provider = server.provider("claude-sonnet-4-6");
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let (result_tx, result_rx) = mpsc::channel();
    let task = thread::spawn(move || {
        let result =
            provider.generate_cancellable(&request("claude-sonnet-4-6"), &mut |_| Ok(()), &|| {
                worker_cancelled.load(Ordering::Acquire)
            });
        result_tx.send(result).expect("send cancellation result");
    });

    server.wait_for_response();
    cancelled.store(true, Ordering::Release);
    let result = result_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("stream cancellation should be prompt");
    task.join().expect("provider cancellation worker");
    assert!(matches!(result, Err(ProviderError::Cancelled)));
}

#[test]
fn credential_never_enters_stream_events_or_serialized_responses() {
    let server = MockServer::start(vec![text_stream("safe output")]);
    let provider = server.provider("claude-sonnet-4-6");
    let mut events = Vec::new();
    let response = provider
        .generate(&request("claude-sonnet-4-6"), &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
    server.wait_for_response();

    assert!(!serde_json::to_string(&events).unwrap().contains(TEST_KEY));
    assert!(!serde_json::to_string(&response).unwrap().contains(TEST_KEY));
    assert!(!format!("{provider:?}").contains(TEST_KEY));
}
