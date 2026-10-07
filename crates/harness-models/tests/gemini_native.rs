use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use harness_models::{
    ContentBlock, FinishReason, GeminiProvider, Message, ModelProvider, ModelRequest,
    ModelStreamEvent, ProviderError, ReasoningConfig, ReasoningEffort, ToolDefinition, ToolResult,
};
use serde_json::{json, Value};

const TEST_KEY: &str = "gemini-test-secret-do-not-leak";

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

    fn malformed_sse(body: &str) -> Self {
        Self {
            status: 200,
            body: format!("data: {body}\n\n"),
            hold_open: false,
        }
    }

    fn held_sse(event: &Value) -> Self {
        Self {
            status: 200,
            body: format!("data: {event}\n\n"),
            hold_open: true,
        }
    }
}

struct CapturedRequest {
    headers: String,
    body: String,
    path: String,
}

struct MockServer {
    base_url: String,
    captures: Arc<Mutex<Vec<CapturedRequest>>>,
    response_sent: mpsc::Receiver<usize>,
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
            for (index, response) in responses.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().expect("accept mock request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("set mock read timeout");
                let (headers, path, body) = read_request(&mut stream);
                thread_captures
                    .lock()
                    .expect("capture lock")
                    .push(CapturedRequest {
                        headers,
                        body,
                        path,
                    });
                let reason = match response.status {
                    200 => "OK",
                    400 => "Bad Request",
                    401 => "Unauthorized",
                    403 => "Forbidden",
                    404 => "Not Found",
                    429 => "Too Many Requests",
                    500 => "Internal Server Error",
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
                    sent_tx.send(index).expect("notify held response started");
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
                .expect("write response headers");
                stream
                    .write_all(response.body.as_bytes())
                    .expect("write response body");
                stream.flush().expect("flush response");
                let _ = sent_tx.send(index);
            }
        });
        Self {
            base_url: format!("http://{address}/v1beta"),
            captures,
            response_sent: sent_rx,
            join: Some(join),
        }
    }

    fn provider(&self, model: &str) -> GeminiProvider {
        GeminiProvider::with_api_key(&self.base_url, model, "MOCK_GEMINI_API_KEY", TEST_KEY)
    }

    fn wait_for_response(&self) -> usize {
        self.response_sent
            .recv_timeout(Duration::from_secs(3))
            .expect("mock server sent a response")
    }

    fn request_count(&self) -> usize {
        self.captures.lock().expect("capture lock").len()
    }

    fn request(&self, index: usize) -> CapturedRequestView {
        let captures = self.captures.lock().expect("capture lock");
        CapturedRequestView {
            headers: captures[index].headers.clone(),
            path: captures[index].path.clone(),
            body: if captures[index].body.is_empty() {
                json!({})
            } else {
                serde_json::from_str(&captures[index].body).expect("request JSON")
            },
        }
    }
}

struct CapturedRequestView {
    headers: String,
    path: String,
    body: Value,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> (String, String, String) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut chunk).expect("read request");
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
        let count = stream.read(&mut chunk).expect("read request body");
        assert!(count > 0, "request closed before body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let path = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    let body =
        String::from_utf8_lossy(&bytes[header_end..header_end + content_length]).into_owned();
    (headers, path, body)
}

fn request(model: &str) -> ModelRequest {
    ModelRequest::new(model, vec![Message::user_text("inspect the project")])
}

fn text_response(id: &str, text: &str) -> Value {
    json!({
        "responseId": id,
        "modelVersion": "gemini-3.8-flash",
        "candidates": [{
            "content": { "role": "model", "parts": [{ "text": text }] },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 7,
            "candidatesTokenCount": 3,
            "totalTokenCount": 10,
            "cachedContentTokenCount": 2
        }
    })
}

#[test]
fn streams_text_and_usage_using_native_endpoint_and_auth_header() {
    let server = MockServer::start(vec![MockResponse::sse(&[
        json!({
            "responseId": "response-text",
            "candidates": [{ "content": { "parts": [{ "text": "Hello " }] } }]
        }),
        json!({
            "responseId": "response-text",
            "candidates": [{ "content": { "parts": [{ "text": "Gemini" }] }, "finishReason": "STOP" }],
            "usageMetadata": { "promptTokenCount": 7, "candidatesTokenCount": 3, "totalTokenCount": 10 }
        }),
    ])]);
    let provider = server.provider("gemini-3.8-flash");
    let mut events = Vec::new();
    let response = provider
        .generate(&request("gemini-3.8-flash"), &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
    server.wait_for_response();

    assert_eq!(response.text(), "Hello Gemini");
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(response.usage.as_ref().unwrap().input_tokens, Some(7));
    assert_eq!(response.usage.as_ref().unwrap().output_tokens, Some(3));
    assert!(events
        .iter()
        .any(|event| matches!(event, ModelStreamEvent::TextDelta { text } if text == "Hello ")));
    assert!(events.iter().any(|event| matches!(event, ModelStreamEvent::UsageUpdated { usage } if usage.total_tokens == Some(10))));
    let captured = server.request(0);
    assert!(captured
        .path
        .contains("/models/gemini-3.8-flash:streamGenerateContent?alt=sse"));
    assert!(captured
        .headers
        .to_ascii_lowercase()
        .contains(&format!("x-goog-api-key: {TEST_KEY}")));
    assert!(!captured.body.to_string().contains(TEST_KEY));
    assert!(!format!("{provider:?}").contains(TEST_KEY));
}

#[test]
fn forwards_normalized_image_input_to_an_image_capable_gemini_model() {
    let server = MockServer::start(vec![MockResponse::sse(&[json!({
        "responseId": "image-response",
        "candidates": [{ "content": { "parts": [{ "text": "The screenshot shows a settings panel." }] }, "finishReason": "STOP" }]
    })])]);
    let provider = server.provider("gemini-3.8-flash");
    let mut model_request = request("gemini-3.8-flash");
    model_request.messages[0].content.push(ContentBlock::Image {
        media_type: "image/png".to_owned(),
        data: "iVBORw0KGgo=".to_owned(),
    });
    let response = provider.generate(&model_request, &mut |_| Ok(())).unwrap();
    server.wait_for_response();
    assert!(response.text().contains("settings panel"));
    let body = server.request(0).body;
    let parts = body["contents"][0]["parts"].as_array().unwrap();
    assert!(parts.iter().any(|part| {
        part["inlineData"]["mimeType"] == "image/png"
            && part["inlineData"]["data"] == "iVBORw0KGgo="
    }));
}

#[test]
fn streams_multiple_native_function_calls_without_exposing_signatures() {
    let server = MockServer::start(vec![MockResponse::sse(&[json!({
        "responseId": "parallel-response",
        "candidates": [{ "content": { "parts": [
            { "functionCall": { "id": "call-read", "name": "read_file", "args": { "path": "src/main.rs" } }, "thoughtSignature": "private-signature-a" },
            { "functionCall": { "id": "call-grep", "name": "grep", "args": { "pattern": "main", "path": "src" } } }
        ] }, "finishReason": "STOP" }],
        "usageMetadata": { "promptTokenCount": 11, "candidatesTokenCount": 9, "totalTokenCount": 20 }
    })])]);
    let provider = server.provider("gemini-3.8-flash");
    let mut model_request = request("gemini-3.8-flash");
    model_request.tools = vec![
        ToolDefinition {
            name: "read_file".to_owned(),
            description: "Read a workspace file".to_owned(),
            input_schema: json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
        },
        ToolDefinition {
            name: "grep".to_owned(),
            description: "Search workspace files".to_owned(),
            input_schema: json!({ "type": "object", "properties": { "pattern": { "type": "string" }, "path": { "type": "string" } }, "required": ["pattern"] }),
        },
    ];
    let mut events = Vec::new();
    let response = provider
        .generate(&model_request, &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
    server.wait_for_response();

    assert_eq!(response.tool_calls.len(), 2);
    assert_eq!(response.tool_calls[0].name, "read_file");
    assert_eq!(response.tool_calls[1].arguments["pattern"], "main");
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    let tool_events = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                ModelStreamEvent::ToolCallStarted { .. }
                    | ModelStreamEvent::ToolCallCompleted { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_events.len(), 4);
    assert!(!format!("{events:?}").contains("private-signature-a"));
    let body = server.request(0).body;
    assert_eq!(
        body["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        "read_file"
    );
}

#[test]
fn native_function_result_continues_with_private_thought_signature() {
    let server = MockServer::start(vec![
        MockResponse::json(
            200,
            json!({
                "responseId": "tool-response",
                "modelVersion": "gemini-3.8-flash",
                "candidates": [{ "content": { "role": "model", "parts": [
                    { "functionCall": { "id": "native-call-1", "name": "read_file", "args": { "path": "src/main.rs" } }, "thoughtSignature": "private-signature" }
                ] }, "finishReason": "STOP" }]
            }),
        ),
        MockResponse::json(200, text_response("continued", "File updated.")),
    ]);
    let provider = server.provider("gemini-3.8-flash");
    let first = provider.complete(&request("gemini-3.8-flash")).unwrap();
    let second_request = ModelRequest {
        messages: vec![
            Message::user_text("Read the file and apply the fix"),
            Message {
                role: harness_models::Role::Assistant,
                content: first.content.clone(),
                name: None,
                tool_call_id: None,
                tool_calls: first.tool_calls.clone(),
                is_error: false,
            },
            Message::tool_result(ToolResult {
                tool_call_id: first.tool_calls[0].id.clone(),
                content: "source contents".to_owned(),
                is_error: false,
            }),
        ],
        ..ModelRequest::new("gemini-3.8-flash", Vec::new())
    };
    let second = provider.complete(&second_request).unwrap();
    server.wait_for_response();
    server.wait_for_response();

    assert_eq!(second.text(), "File updated.");
    assert_eq!(second_request.messages[1].tool_calls[0].id, "native-call-1");
    assert!(!serde_json::to_string(&first)
        .unwrap()
        .contains("private-signature"));
    let continuation = server.request(1).body;
    assert_eq!(continuation["contents"][1]["role"], "model");
    assert_eq!(
        continuation["contents"][1]["parts"][0]["thoughtSignature"],
        "private-signature"
    );
    assert_eq!(continuation["contents"][2]["role"], "function");
    assert_eq!(
        continuation["contents"][2]["parts"][0]["functionResponse"]["name"],
        "read_file"
    );
    assert_eq!(
        continuation["contents"][2]["parts"][0]["functionResponse"]["id"],
        "native-call-1"
    );
    assert_eq!(
        continuation["contents"][2]["parts"][0]["functionResponse"]["response"]["result"],
        "source contents"
    );
}

#[test]
fn maps_system_instruction_output_limit_and_gemini_thinking_controls() {
    let server = MockServer::start(vec![MockResponse::json(
        200,
        text_response("reasoned", "done"),
    )]);
    let provider = server.provider("gemini-3.8-flash");
    let model_request = ModelRequest {
        messages: vec![
            Message {
                role: harness_models::Role::System,
                content: vec![harness_models::ContentBlock::Text {
                    text: "Be careful".to_owned(),
                }],
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                is_error: false,
            },
            Message::user_text("hello"),
        ],
        max_output_tokens: Some(1234),
        reasoning: Some(ReasoningConfig {
            effort: Some(ReasoningEffort::High),
            include_summary: true,
            ..ReasoningConfig::default()
        }),
        ..request("gemini-3.8-flash")
    };
    provider.complete(&model_request).unwrap();
    server.wait_for_response();
    let body = server.request(0).body;
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "Be careful");
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 1234);
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );
}

#[test]
fn model_discovery_filters_methods_maps_limits_caches_and_refreshes() {
    let server = MockServer::start(vec![
        MockResponse::json(
            200,
            json!({
                "models": [
                    { "name": "models/gemini-3.8-flash-001", "baseModelId": "gemini-3.8-flash", "displayName": "Gemini Flash", "inputTokenLimit": 1000000, "outputTokenLimit": 64000, "supportedGenerationMethods": ["generateContent"], "thinking": true },
                    { "name": "models/gemini-3.8-flash-preview-001", "baseModelId": "gemini-3.8-flash-preview", "displayName": "Gemini Flash Preview", "inputTokenLimit": 1000000, "outputTokenLimit": 64000, "supportedGenerationMethods": ["generateContent"] },
                    { "name": "models/text-embedding-latest", "baseModelId": "text-embedding-latest", "supportedGenerationMethods": ["embedContent"] }
                ],
                "nextPageToken": "next/token"
            }),
        ),
        MockResponse::json(
            200,
            json!({
                "models": [
                    { "name": "models/gemini-2.5-flash", "baseModelId": "gemini-2.5-flash", "displayName": "Gemini 2.5 Flash", "inputTokenLimit": 1048576, "outputTokenLimit": 8192, "supportedGenerationMethods": ["generateContent"], "thinking": true }
                ]
            }),
        ),
        MockResponse::json(
            200,
            json!({
                "models": [
                    { "name": "models/gemini-next", "baseModelId": "gemini-next", "displayName": "Gemini Next", "inputTokenLimit": 2000000, "outputTokenLimit": 100000, "supportedGenerationMethods": ["generateContent"] }
                ]
            }),
        ),
    ]);
    let provider = server.provider("gemini-3.8-flash");
    let discovered = provider.discover_models().unwrap();
    assert_eq!(discovered.len(), 3);
    assert_eq!(discovered[0].id, "gemini-2.5-flash");
    assert_eq!(discovered[1].display_name, "Gemini Flash");
    assert_eq!(discovered[2].id, "gemini-3.8-flash-preview");
    assert!(discovered[2].capabilities.reasoning);
    assert_eq!(discovered[1].capabilities.context_window, Some(1_000_000));
    assert_eq!(discovered[1].capabilities.max_output_tokens, Some(64_000));
    assert!(discovered[1].capabilities.reasoning);
    assert_eq!(server.request_count(), 2);
    assert!(server.request(1).path.contains("pageToken=next%2Ftoken"));

    assert_eq!(provider.discover_models().unwrap(), discovered);
    assert_eq!(
        server.request_count(),
        2,
        "fresh cached listing should avoid HTTP"
    );
    let refreshed = provider.refresh_models().unwrap();
    assert_eq!(refreshed[0].id, "gemini-next");
    assert_eq!(refreshed[0].capabilities.context_window, Some(2_000_000));
    assert_eq!(server.request_count(), 3);
}

#[test]
fn api_errors_are_normalized_without_server_details_or_credentials() {
    let server = MockServer::start(vec![
        MockResponse::json(
            429,
            json!({ "error": { "code": 429, "message": TEST_KEY } }),
        ),
        MockResponse::json(200, text_response("after-retry", "ok")),
        MockResponse::json(
            401,
            json!({ "error": { "code": 401, "message": TEST_KEY } }),
        ),
    ]);
    let provider = server.provider("gemini-3.8-flash");
    let response = provider.complete(&request("gemini-3.8-flash")).unwrap();
    assert_eq!(response.text(), "ok");
    let error = provider.complete(&request("gemini-3.8-flash")).unwrap_err();
    server.wait_for_response();
    server.wait_for_response();
    server.wait_for_response();
    assert!(matches!(
        error,
        ProviderError::Request {
            provider: "gemini",
            status: Some(401)
        }
    ));
    assert!(!error.to_string().contains(TEST_KEY));
    assert_eq!(server.request_count(), 3);
}

#[test]
fn malformed_stream_event_is_rejected_and_cancellation_interrupts_read() {
    let malformed = MockServer::start(vec![MockResponse::malformed_sse("not-json")]);
    let provider = malformed.provider("gemini-3.8-flash");
    let error = provider
        .stream(&request("gemini-3.8-flash"), &mut |_| Ok(()))
        .unwrap_err();
    malformed.wait_for_response();
    assert!(matches!(error, ProviderError::InvalidResponse { .. }));

    let held = MockServer::start(vec![MockResponse::held_sse(&json!({
        "responseId": "held",
        "candidates": [{ "content": { "parts": [{ "text": "partial" }] } }]
    }))]);
    let provider = Arc::new(held.provider("gemini-3.8-flash"));
    let cancelled = Arc::new(AtomicBool::new(false));
    let thread_cancelled = Arc::clone(&cancelled);
    let (result_tx, result_rx) = mpsc::channel();
    let thread_provider = Arc::clone(&provider);
    let join = thread::spawn(move || {
        let result = thread_provider.stream_cancellable(
            &request("gemini-3.8-flash"),
            &mut |_| Ok(()),
            &|| thread_cancelled.load(Ordering::SeqCst),
        );
        result_tx.send(result.map(|_| ())).unwrap();
    });
    held.wait_for_response();
    cancelled.store(true, Ordering::SeqCst);
    assert!(matches!(
        result_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        Err(ProviderError::Cancelled)
    ));
    join.join().unwrap();
}

#[test]
fn explicit_unknown_model_is_accepted_without_discovery() {
    let server = MockServer::start(vec![MockResponse::json(
        200,
        text_response("explicit", "works"),
    )]);
    let provider = server.provider("custom-gemini-compatible-model");
    let response = provider
        .complete(&request("custom-gemini-compatible-model"))
        .unwrap();
    server.wait_for_response();
    assert_eq!(response.text(), "works");
    assert_eq!(provider.descriptor().id, "custom-gemini-compatible-model");
    assert!(!provider.descriptor().capabilities.reasoning);
}
