use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use harness_models::{
    FinishReason, Message, ModelProvider, ModelRequest, ModelStreamEvent, OpenAIProvider,
    ProviderError, ToolDefinition,
};
use serde_json::{json, Value};

const TEST_KEY: &str = "sk-test-do-not-leak";

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
        let body = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
        Self {
            status: 200,
            body,
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
                    429 => "Too Many Requests",
                    500 => "Internal Server Error",
                    _ => "Mock Status",
                };
                if response.hold_open {
                    let first_event = response.body.as_bytes();
                    write!(
                        stream,
                        "HTTP/1.1 {} {}\r\nContent-Type: text/event-stream\r\nContent-Length: 100000\r\nConnection: keep-alive\r\n\r\n",
                        response.status, reason
                    )
                    .expect("write held response headers");
                    stream
                        .write_all(first_event)
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

    fn provider(&self, model: &str) -> OpenAIProvider {
        OpenAIProvider::with_api_key(&self.base_url, model, "MOCK_OPENAI_KEY", TEST_KEY)
    }

    fn wait_for_response(&self) {
        self.response_sent
            .recv_timeout(Duration::from_secs(2))
            .expect("mock server sent a response");
    }

    fn request_count(&self) -> usize {
        self.captures.lock().expect("capture lock").len()
    }

    fn request(&self, index: usize) -> String {
        let captures = self.captures.lock().expect("capture lock");
        let capture = &captures[index];
        format!("{}\n{}", capture.headers, capture.body)
    }
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

fn completed_response(output: Value, usage: Value) -> Value {
    json!({
        "id": "resp-test",
        "model": "gpt-test",
        "status": "completed",
        "output": output,
        "usage": usage,
    })
}

#[test]
fn complete_uses_non_streaming_responses_and_normalizes_text() {
    let server = MockServer::start(vec![MockResponse::json(
        200,
        completed_response(
            json!([{ "type": "message", "content": [{ "type": "output_text", "text": "complete" }] }]),
            json!({ "input_tokens": 2, "output_tokens": 1, "total_tokens": 3 }),
        ),
    )]);
    let provider = server.provider("gpt-test");
    let response = provider
        .complete(&ModelRequest::new(
            "ignored",
            vec![Message::user_text("hello")],
        ))
        .unwrap();
    server.wait_for_response();

    assert_eq!(response.text(), "complete");
    assert_eq!(response.usage.unwrap().total_tokens, Some(3));
    let request: Value = serde_json::from_str(
        server
            .request(0)
            .split("\r\n\r\n")
            .nth(1)
            .expect("request body"),
    )
    .unwrap();
    assert_eq!(request["model"], "gpt-test");
    assert_eq!(request["stream"], false);
}

fn streamed_text() -> MockResponse {
    MockResponse::sse(&[
        json!({ "type": "response.created", "response": { "id": "resp-test" } }),
        json!({ "type": "response.output_text.delta", "output_index": 0, "delta": "hello" }),
        json!({
            "type": "response.completed",
            "response": completed_response(
                json!([{ "type": "message", "content": [{ "type": "output_text", "text": "hello" }] }]),
                json!({ "input_tokens": 12, "output_tokens": 3, "total_tokens": 15, "input_tokens_details": { "cached_tokens": 4 } })
            )
        }),
    ])
}

fn function_call_item(id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "type": "function_call",
        "id": format!("fc_{id}"),
        "call_id": id,
        "name": name,
        "arguments": arguments,
    })
}

fn stream_tool_call(id: &str, name: &str, arguments: &str, index: u32) -> Vec<Value> {
    vec![
        json!({ "type": "response.output_item.added", "output_index": index, "item": function_call_item(id, name, "") }),
        json!({ "type": "response.function_call_arguments.delta", "output_index": index, "delta": arguments }),
        json!({ "type": "response.output_item.done", "output_index": index, "item": function_call_item(id, name, arguments) }),
    ]
}

#[test]
fn streams_text_and_reports_usage_without_leaking_credentials() {
    let server = MockServer::start(vec![streamed_text()]);
    let provider = server.provider("gpt-test");
    let mut events = Vec::new();
    let response = provider
        .generate(
            &ModelRequest::new("ignored", vec![Message::user_text("say hello")]),
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )
        .unwrap();
    server.wait_for_response();

    assert_eq!(response.text(), "hello");
    assert_eq!(response.usage.as_ref().unwrap().input_tokens, Some(12));
    assert_eq!(response.usage.as_ref().unwrap().total_tokens, Some(15));
    assert!(events
        .iter()
        .any(|event| matches!(event, ModelStreamEvent::TextDelta { text } if text == "hello")));
    assert!(events.iter().any(|event| matches!(event, ModelStreamEvent::UsageUpdated { usage } if usage.cache_read_tokens == Some(4))));
    let request = server.request(0);
    assert!(request.contains("Authorization: Bearer sk-test-do-not-leak"));
    assert!(request.contains("\"stream\":true"));
    assert!(!serde_json::to_string(&events).unwrap().contains(TEST_KEY));
}

#[test]
fn streams_function_call_arguments_and_maps_call_to_canonical_tool_call() {
    let mut events = vec![json!({ "type": "response.created", "response": { "id": "resp-tool" } })];
    events.extend(stream_tool_call(
        "call-1",
        "read_file",
        "{\"path\":\"README.md\"}",
        0,
    ));
    events.push(json!({
        "type": "response.completed",
        "response": completed_response(
            json!([function_call_item("call-1", "read_file", "{\"path\":\"README.md\"}")]),
            json!({ "input_tokens": 5, "output_tokens": 4, "total_tokens": 9 })
        )
    }));
    let server = MockServer::start(vec![MockResponse::sse(&events)]);
    let provider = server.provider("gpt-test");
    let mut request = ModelRequest::new("ignored", vec![Message::user_text("read the readme")]);
    request.tools.push(ToolDefinition {
        name: "read_file".to_owned(),
        description: "Read a file".to_owned(),
        input_schema: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
    });
    let mut normalized = Vec::new();
    let response = provider
        .generate(&request, &mut |event| {
            normalized.push(event);
            Ok(())
        })
        .unwrap();
    server.wait_for_response();

    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.tool_calls[0].id, "call-1");
    assert_eq!(
        response.tool_calls[0].arguments,
        json!({ "path": "README.md" })
    );
    assert!(normalized.iter().any(|event| matches!(event, ModelStreamEvent::ToolCallArgumentsDelta { delta, .. } if delta.contains("README.md"))));
    assert!(normalized.iter().any(|event| matches!(event, ModelStreamEvent::ToolCallCompleted { call, .. } if call.name == "read_file")));
    let request = server.request(0);
    let request: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(request["tools"][0]["type"], "function");
    assert_eq!(request["tools"][0]["name"], "read_file");
}

#[test]
fn streams_multiple_tool_calls_in_output_order() {
    let mut events =
        vec![json!({ "type": "response.created", "response": { "id": "resp-multi" } })];
    events.extend(stream_tool_call(
        "call-a",
        "read_file",
        "{\"path\":\"a\"}",
        0,
    ));
    events.extend(stream_tool_call(
        "call-b",
        "read_file",
        "{\"path\":\"b\"}",
        1,
    ));
    events.push(json!({
        "type": "response.completed",
        "response": completed_response(
            json!([
                function_call_item("call-a", "read_file", "{\"path\":\"a\"}"),
                function_call_item("call-b", "read_file", "{\"path\":\"b\"}")
            ]),
            json!({ "input_tokens": 8, "output_tokens": 6, "total_tokens": 14 })
        )
    }));
    let server = MockServer::start(vec![MockResponse::sse(&events)]);
    let provider = server.provider("gpt-test");
    let response = provider
        .generate(
            &ModelRequest::new("gpt-test", vec![Message::user_text("read both")]),
            &mut |_| Ok(()),
        )
        .unwrap();
    server.wait_for_response();

    assert_eq!(response.tool_calls.len(), 2);
    assert_eq!(response.tool_calls[0].id, "call-a");
    assert_eq!(response.tool_calls[1].arguments, json!({ "path": "b" }));
}

#[test]
fn malformed_stream_response_is_normalized() {
    let server = MockServer::start(vec![MockResponse {
        status: 200,
        body: "data: definitely not json\n\n".to_owned(),
        hold_open: false,
    }]);
    let provider = server.provider("gpt-test");
    let mut events = Vec::new();
    let error = provider
        .generate(
            &ModelRequest::new("gpt-test", vec![Message::user_text("hello")]),
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )
        .unwrap_err();
    server.wait_for_response();

    assert!(matches!(error, ProviderError::InvalidResponse { .. }));
    assert!(events
        .iter()
        .any(|event| matches!(event, ModelStreamEvent::ResponseFailed { .. })));
    assert!(!error.to_string().contains(TEST_KEY));
    assert!(!serde_json::to_string(&events).unwrap().contains(TEST_KEY));
}

#[test]
fn authentication_failure_does_not_expose_the_key() {
    let server = MockServer::start(vec![MockResponse::json(
        401,
        json!({ "error": { "message": TEST_KEY } }),
    )]);
    let provider = server.provider("gpt-test");
    let mut events = Vec::new();
    let error = provider
        .generate(
            &ModelRequest::new("gpt-test", vec![Message::user_text("hello")]),
            &mut |event| {
                events.push(event);
                Ok(())
            },
        )
        .unwrap_err();
    server.wait_for_response();

    assert!(matches!(
        error,
        ProviderError::Request {
            status: Some(401),
            ..
        }
    ));
    assert!(!error.to_string().contains(TEST_KEY));
    assert!(!serde_json::to_string(&events).unwrap().contains(TEST_KEY));
}

#[test]
fn transient_rate_limit_is_retried_once_before_streaming() {
    let server = MockServer::start(vec![
        MockResponse::json(429, json!({ "error": { "message": "retry" } })),
        streamed_text(),
    ]);
    let provider = server.provider("gpt-test");
    let response = provider
        .generate(
            &ModelRequest::new("gpt-test", vec![Message::user_text("hello")]),
            &mut |_| Ok(()),
        )
        .unwrap();
    server.wait_for_response();
    server.wait_for_response();

    assert_eq!(response.text(), "hello");
    assert_eq!(server.request_count(), 2);
}

#[test]
fn server_failure_retries_are_bounded_and_status_is_normalized() {
    let server = MockServer::start(vec![
        MockResponse::json(500, json!({ "error": { "message": "internal" } })),
        MockResponse::json(500, json!({ "error": { "message": "internal" } })),
        MockResponse::json(500, json!({ "error": { "message": TEST_KEY } })),
    ]);
    let provider = server.provider("gpt-test");
    let error = provider
        .generate(
            &ModelRequest::new("gpt-test", vec![Message::user_text("hello")]),
            &mut |_| Ok(()),
        )
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
fn cancelled_stream_returns_promptly_and_emits_no_secret() {
    let server = MockServer::start(vec![MockResponse {
        status: 200,
        body: format!(
            "data: {}\n\n",
            json!({ "type": "response.created", "response": { "id": "resp-held" } })
        ),
        hold_open: true,
    }]);
    let provider = server.provider("gpt-test");
    let cancelled = Arc::new(AtomicBool::new(false));
    let thread_cancelled = Arc::clone(&cancelled);
    let (result_tx, result_rx) = mpsc::channel();
    let task = thread::spawn(move || {
        let result = provider.generate_cancellable(
            &ModelRequest::new("gpt-test", vec![Message::user_text("wait")]),
            &mut |_| Ok(()),
            &|| thread_cancelled.load(Ordering::Acquire),
        );
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
fn model_discovery_filters_endpoint_results_with_capability_metadata() {
    let server = MockServer::start(vec![MockResponse::json(
        200,
        json!({
            "object": "list",
            "data": [
                { "id": "gpt-4.1-mini" },
                { "id": "o3-mini" },
                { "id": "text-embedding-3-large" },
                { "id": "gpt-4o-realtime-preview" },
                { "id": "custom-finetune" }
            ]
        }),
    )]);
    let provider = server.provider("gpt-4.1-mini");
    let models = provider.discover_models().unwrap();
    server.wait_for_response();

    let ids = models.into_iter().map(|model| model.id).collect::<Vec<_>>();
    assert_eq!(ids, ["gpt-4.1-mini", "o3-mini"]);
    assert!(server.request(0).starts_with("GET /v1/models HTTP/1.1"));
}
