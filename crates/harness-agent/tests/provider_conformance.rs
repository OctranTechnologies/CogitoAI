use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use harness_agent::{AgentLimits, AgentRunner, AgentTask, ApprovalHandler};
use harness_context::{ContextBuilder, WorkspaceMetadata};
use harness_git::{CheckpointStore, ShadowCheckpointStore};
use harness_models::{
    AnthropicProvider, GeminiProvider, Message, ModelProvider, ModelRequest, ModelStreamEvent,
    OpenAIProvider, OpenCodeProduct, OpenCodeProvider, ProviderError,
};
use harness_policy::{ExecutionMode, PolicyEngine};
use harness_session::{EventPayload, JsonlSessionStore, Session, SessionStatus, SessionStore};
use harness_tools::{
    ApplyPatchTool, CancellationToken, GrepTool, ListDirectoryTool, ProcessError, ProcessEvent,
    ProcessRequest, ProcessResult, ProcessRunner, ReadFileTool, ShellTool, ToolRegistry,
};
use serde_json::{json, Value};
use tempfile::tempdir;

const API_KEY: &str = "conformance-test-key-never-persist";
const SYSTEM_INSTRUCTION: &str = "CONFORMANCE: preserve the public score API.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WireProtocol {
    Responses,
    Messages,
    Gemini,
    ChatCompletions,
}

#[derive(Clone, Copy)]
struct AdapterCase {
    name: &'static str,
    provider: &'static str,
    model: &'static str,
    protocol: WireProtocol,
    product: Option<OpenCodeProduct>,
}

const CASES: &[AdapterCase] = &[
    AdapterCase {
        name: "OpenAI Responses",
        provider: "openai",
        model: "gpt-conformance",
        protocol: WireProtocol::Responses,
        product: None,
    },
    AdapterCase {
        name: "Anthropic Messages",
        provider: "anthropic",
        model: "claude-sonnet-4-6",
        protocol: WireProtocol::Messages,
        product: None,
    },
    AdapterCase {
        name: "Gemini native",
        provider: "gemini",
        model: "gemini-2.5-flash",
        protocol: WireProtocol::Gemini,
        product: None,
    },
    AdapterCase {
        name: "OpenCode Zen Responses",
        provider: "opencode-zen",
        model: "opencode-zen/conformance-responses",
        protocol: WireProtocol::Responses,
        product: Some(OpenCodeProduct::Zen),
    },
    AdapterCase {
        name: "OpenCode Zen Chat Completions",
        provider: "opencode-zen",
        model: "opencode-zen/conformance-chat",
        protocol: WireProtocol::ChatCompletions,
        product: Some(OpenCodeProduct::Zen),
    },
    AdapterCase {
        name: "OpenCode Zen Messages",
        provider: "opencode-zen",
        model: "opencode-zen/conformance-messages",
        protocol: WireProtocol::Messages,
        product: Some(OpenCodeProduct::Zen),
    },
    AdapterCase {
        name: "OpenCode Go Responses",
        provider: "opencode-go",
        model: "opencode-go/conformance-responses",
        protocol: WireProtocol::Responses,
        product: Some(OpenCodeProduct::Go),
    },
    AdapterCase {
        name: "OpenCode Go Chat Completions",
        provider: "opencode-go",
        model: "opencode-go/conformance-chat",
        protocol: WireProtocol::ChatCompletions,
        product: Some(OpenCodeProduct::Go),
    },
    AdapterCase {
        name: "OpenCode Go Messages",
        provider: "opencode-go",
        model: "opencode-go/conformance-messages",
        protocol: WireProtocol::Messages,
        product: Some(OpenCodeProduct::Go),
    },
];

#[derive(Clone)]
enum ModelStep {
    Tool {
        name: &'static str,
        arguments: Value,
    },
    MultipleTools {
        calls: Vec<FixtureToolCall>,
    },
    Text(&'static str),
}

#[derive(Clone)]
struct FixtureToolCall {
    name: &'static str,
    arguments: Value,
}

fn tool(name: &'static str, arguments: Value) -> FixtureToolCall {
    FixtureToolCall { name, arguments }
}

fn coding_steps() -> Vec<ModelStep> {
    vec![
        ModelStep::MultipleTools {
            calls: vec![
                tool("list_directory", json!({ "path": "." })),
                tool("list_directory", json!({ "path": "src" })),
            ],
        },
        ModelStep::Tool {
            name: "grep",
            arguments: json!({ "pattern": "score", "path": ".", "max_results": 1 }),
        },
        ModelStep::Tool {
            name: "read_file",
            arguments: json!({ "path": "src/missing.rs" }),
        },
        ModelStep::Tool {
            name: "read_file",
            arguments: json!({ "path": "src/lib.rs" }),
        },
        ModelStep::Tool {
            name: "read_file",
            arguments: json!({ "path": "tests/score.rs" }),
        },
        ModelStep::Tool {
            name: "apply_patch",
            arguments: json!({ "path": "src/lib.rs", "old_text": "value + 1", "new_text": "value + 2" }),
        },
        ModelStep::Tool {
            name: "shell",
            arguments: json!({ "command": "cargo test --quiet" }),
        },
        ModelStep::Tool {
            name: "apply_patch",
            arguments: json!({ "path": "src/lib.rs", "old_text": "value + 2", "new_text": "value" }),
        },
        ModelStep::Tool {
            name: "shell",
            arguments: json!({ "command": "cargo test --quiet" }),
        },
        ModelStep::Text(
            "Fixed score's off-by-one bug. The test command passed after the correction.",
        ),
    ]
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    headers: String,
    path: String,
    body: Value,
}

struct MockServer {
    root: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    join: Option<JoinHandle<()>>,
}

impl MockServer {
    fn start(case: AdapterCase) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind conformance server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("conformance server address");
        let root = format!("http://{address}");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let steps = coding_steps();
        let setup_requests = usize::from(case.product.is_some()) * 2;
        let expected_requests = setup_requests + steps.len();
        let join = thread::spawn(move || {
            let mut model_turn = 0;
            let deadline = Instant::now() + Duration::from_secs(8);
            while model_turn + setup_requests < expected_requests && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept conformance request: {error}"),
                };
                stream
                    .set_nonblocking(false)
                    .expect("restore blocking accepted socket");
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("set conformance read timeout");
                let request = read_request(&mut stream);
                let setup = case.product.is_some() && request.path == "/api.json"
                    || case.product.is_some() && request.path.ends_with("/models");
                if case.product.is_some() && request.path == "/api.json" {
                    write_json(&mut stream, &catalog(case));
                    captured.lock().unwrap().push(request);
                    continue;
                }
                if case.product.is_some() && request.path.ends_with("/models") {
                    write_json(&mut stream, &listing(case));
                    captured.lock().unwrap().push(request);
                    continue;
                }
                assert!(!setup, "unexpected catalog request {}", request.path);
                assert!(model_turn < steps.len(), "unexpected extra model request");
                write_sse(
                    &mut stream,
                    &stream_events(case, model_turn, &steps[model_turn]),
                );
                captured.lock().unwrap().push(request);
                model_turn += 1;
            }
        });
        Self {
            root,
            requests,
            join: Some(join),
        }
    }

    fn provider(&self, case: AdapterCase) -> Arc<dyn ModelProvider> {
        let base_url = match case.product {
            Some(OpenCodeProduct::Zen) => format!("{}/zen/v1", self.root),
            Some(OpenCodeProduct::Go) => format!("{}/zen/go/v1", self.root),
            None if case.protocol == WireProtocol::Gemini => format!("{}/v1beta", self.root),
            None => format!("{}/v1", self.root),
        };
        match case.product {
            Some(product) => Arc::new(OpenCodeProvider::with_api_key_and_catalog_url(
                product,
                base_url,
                case.model,
                "OPENCODE_API_KEY",
                API_KEY,
                format!("{}/api.json", self.root),
            )),
            None if case.protocol == WireProtocol::Responses => Arc::new(
                OpenAIProvider::with_api_key(base_url, case.model, "OPENAI_API_KEY", API_KEY),
            ),
            None if case.protocol == WireProtocol::Messages => Arc::new(
                AnthropicProvider::with_api_key(base_url, case.model, "ANTHROPIC_API_KEY", API_KEY),
            ),
            None => Arc::new(GeminiProvider::with_api_key(
                base_url,
                case.model,
                "GEMINI_API_KEY",
                API_KEY,
            )),
        }
    }

    fn finish(&mut self) -> Vec<CapturedRequest> {
        if let Some(join) = self.join.take() {
            join.join().expect("mock provider server");
        }
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

struct ScenarioMockServer {
    root: String,
    shutdown: Arc<AtomicBool>,
    model_request: Receiver<()>,
    join: Option<JoinHandle<()>>,
}

impl ScenarioMockServer {
    fn start(case: AdapterCase, status: Option<u16>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scenario server");
        listener
            .set_nonblocking(true)
            .expect("make scenario listener nonblocking");
        let root = format!("http://{}", listener.local_addr().expect("server address"));
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = Arc::clone(&shutdown);
        let (request_tx, model_request) = mpsc::channel();
        let join = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !server_shutdown.load(Ordering::Acquire) && Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept scenario request: {error}"),
                };
                stream
                    .set_nonblocking(false)
                    .expect("restore blocking scenario socket");
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("set scenario read timeout");
                let request = read_request(&mut stream);
                if case.product.is_some() && request.path == "/api.json" {
                    write_json(&mut stream, &catalog(case));
                    continue;
                }
                if case.product.is_some() && request.path.ends_with("/models") {
                    write_json(&mut stream, &listing(case));
                    continue;
                }
                request_tx.send(()).expect("signal model request");
                if let Some(status) = status {
                    write_status(
                        &mut stream,
                        status,
                        &json!({ "error": { "message": API_KEY } }).to_string(),
                    );
                    continue;
                }

                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 4096\r\nConnection: keep-alive\r\n\r\n"
                )
                .expect("write hanging stream headers");
                stream.flush().expect("flush hanging stream headers");
                while !server_shutdown.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(5));
                }
            }
        });
        Self {
            root,
            shutdown,
            model_request,
            join: Some(join),
        }
    }

    fn provider(&self, case: AdapterCase) -> Arc<dyn ModelProvider> {
        let base_url = match case.product {
            Some(OpenCodeProduct::Zen) => format!("{}/zen/v1", self.root),
            Some(OpenCodeProduct::Go) => format!("{}/zen/go/v1", self.root),
            None if case.protocol == WireProtocol::Gemini => format!("{}/v1beta", self.root),
            None => format!("{}/v1", self.root),
        };
        match case.product {
            Some(product) => Arc::new(OpenCodeProvider::with_api_key_and_catalog_url(
                product,
                base_url,
                case.model,
                "OPENCODE_API_KEY",
                API_KEY,
                format!("{}/api.json", self.root),
            )),
            None if case.protocol == WireProtocol::Responses => Arc::new(
                OpenAIProvider::with_api_key(base_url, case.model, "OPENAI_API_KEY", API_KEY),
            ),
            None if case.protocol == WireProtocol::Messages => Arc::new(
                AnthropicProvider::with_api_key(base_url, case.model, "ANTHROPIC_API_KEY", API_KEY),
            ),
            None => Arc::new(GeminiProvider::with_api_key(
                base_url,
                case.model,
                "GEMINI_API_KEY",
                API_KEY,
            )),
        }
    }

    fn wait_for_model_request(&self) {
        self.model_request
            .recv_timeout(Duration::from_secs(3))
            .expect("provider sent its mock model request");
    }

    fn finish(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            join.join().expect("scenario mock server");
        }
    }
}

impl Drop for ScenarioMockServer {
    fn drop(&mut self) {
        self.finish();
    }
}

fn write_status(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write scenario error response");
    stream.flush().expect("flush scenario error response");
}

fn read_request(stream: &mut TcpStream) -> CapturedRequest {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut buffer).expect("read request headers");
        assert_ne!(count, 0, "request ended before headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let end = index + 4;
            let headers = String::from_utf8_lossy(&bytes[..end]).into_owned();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= end + content_length {
                break (end, content_length);
            }
        }
    };
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut buffer).expect("read request body");
        assert_ne!(count, 0, "request ended before body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let path = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    let body = if content_length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[header_end..header_end + content_length])
            .expect("provider request JSON")
    };
    CapturedRequest {
        headers,
        path,
        body,
    }
}

fn write_json(stream: &mut TcpStream, value: &Value) {
    write_http(stream, "application/json", value.to_string().as_bytes());
}

fn write_sse(stream: &mut TcpStream, events: &[Value]) {
    let body = events
        .iter()
        .map(|event| {
            let data = if event.as_str() == Some("[DONE]") {
                "[DONE]".to_owned()
            } else {
                event.to_string()
            };
            format!("data: {data}\n\n")
        })
        .collect::<String>();
    write_http(stream, "text/event-stream", body.as_bytes());
}

fn write_http(stream: &mut TcpStream, content_type: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("write response headers");
    stream.write_all(body).expect("write response body");
    stream.flush().expect("flush response");
}

fn raw_model_id(case: AdapterCase) -> &'static str {
    case.model
        .split_once('/')
        .map_or(case.model, |(_, model)| model)
}

fn catalog(case: AdapterCase) -> Value {
    let provider_key = match case.product {
        Some(OpenCodeProduct::Zen) => "opencode",
        Some(OpenCodeProduct::Go) => "opencode-go",
        None => unreachable!(),
    };
    let package = match case.protocol {
        WireProtocol::Responses => "@ai-sdk/openai",
        WireProtocol::ChatCompletions => "@ai-sdk/openai-compatible",
        WireProtocol::Messages => "@ai-sdk/anthropic",
        WireProtocol::Gemini => unreachable!(),
    };
    json!({
        (provider_key): {
            "models": {
                (raw_model_id(case)): {
                    "name": case.name,
                    "tool_call": true,
                    "provider": { "npm": package }
                }
            }
        }
    })
}

fn listing(case: AdapterCase) -> Value {
    json!({ "object": "list", "data": [{ "id": raw_model_id(case), "object": "model" }] })
}

fn stream_events(case: AdapterCase, turn: usize, step: &ModelStep) -> Vec<Value> {
    match case.protocol {
        WireProtocol::Responses => responses_events(case, turn, step),
        WireProtocol::Messages => messages_events(case, turn, step),
        WireProtocol::Gemini => gemini_events(case, turn, step),
        WireProtocol::ChatCompletions => chat_events(case, turn, step),
    }
}

fn responses_events(case: AdapterCase, turn: usize, step: &ModelStep) -> Vec<Value> {
    let response_id = format!("responses-{turn}");
    let mut events = vec![json!({ "type": "response.created", "response": { "id": response_id } })];
    let calls = step_calls(step);
    if !calls.is_empty() {
        let mut output = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            let call_id = format!("call-{turn}-{index}");
            let item = json!({ "type": "function_call", "id": format!("fc-{turn}-{index}"), "call_id": call_id, "name": call.name, "arguments": call.arguments.to_string() });
            events.push(json!({ "type": "response.output_item.added", "output_index": index, "item": { "type": "function_call", "id": format!("fc-{turn}-{index}"), "call_id": call_id, "name": call.name, "arguments": "" } }));
            events.push(json!({ "type": "response.function_call_arguments.delta", "output_index": index, "delta": call.arguments.to_string() }));
            events.push(json!({ "type": "response.output_item.done", "output_index": index, "item": item.clone() }));
            output.push(item);
        }
        events.push(json!({ "type": "response.completed", "response": completed_response(case, turn, output) }));
    } else if let ModelStep::Text(text) = step {
        events.push(
            json!({ "type": "response.output_text.delta", "output_index": 0, "delta": text }),
        );
        events.push(json!({ "type": "response.completed", "response": completed_response(case, turn, vec![json!({ "type": "message", "content": [{ "type": "output_text", "text": text }] })]) }));
    }
    events
}

fn completed_response(case: AdapterCase, turn: usize, output: Vec<Value>) -> Value {
    json!({
        "id": format!("responses-{turn}"),
        "model": raw_model_id(case),
        "status": "completed",
        "output": output,
        "usage": { "input_tokens": 12, "output_tokens": 4, "total_tokens": 16 }
    })
}

fn messages_events(case: AdapterCase, turn: usize, step: &ModelStep) -> Vec<Value> {
    let mut events = vec![json!({ "type": "message_start", "message": {
        "id": format!("message-{turn}"), "type": "message", "role": "assistant",
        "model": raw_model_id(case), "content": [], "stop_reason": null,
        "usage": { "input_tokens": 12, "output_tokens": 1 }
    } })];
    let calls = step_calls(step);
    if !calls.is_empty() {
        for (index, call) in calls.iter().enumerate() {
            events.push(json!({ "type": "content_block_start", "index": index, "content_block": { "type": "tool_use", "id": format!("call-{turn}-{index}"), "name": call.name, "input": {} } }));
            events.push(json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": call.arguments.to_string() } }));
            events.push(json!({ "type": "content_block_stop", "index": index }));
        }
        events.push(json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 4 } }));
    } else if let ModelStep::Text(text) = step {
        events.push(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }));
        events.push(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }));
        events.push(json!({ "type": "content_block_stop", "index": 0 }));
        events.push(json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 4 } }));
    }
    events.push(json!({ "type": "message_stop" }));
    events
}

fn gemini_events(case: AdapterCase, turn: usize, step: &ModelStep) -> Vec<Value> {
    let calls = step_calls(step);
    let parts = if calls.is_empty() {
        let ModelStep::Text(text) = step else {
            unreachable!("tool steps contain calls")
        };
        vec![json!({ "text": text })]
    } else {
        calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                json!({ "functionCall": {
                "id": format!("call-{turn}-{index}"), "name": call.name, "args": call.arguments
            } })
            })
            .collect()
    };
    vec![json!({
        "responseId": format!("gemini-{turn}"),
        "modelVersion": case.model,
        "candidates": [{ "content": { "parts": parts }, "finishReason": "STOP" }],
        "usageMetadata": { "promptTokenCount": 12, "candidatesTokenCount": 4, "totalTokenCount": 16 }
    })]
}

fn chat_events(case: AdapterCase, turn: usize, step: &ModelStep) -> Vec<Value> {
    let start = json!({ "id": format!("chat-{turn}"), "model": raw_model_id(case), "choices": [{ "delta": {}, "finish_reason": null }] });
    let calls = step_calls(step);
    let finish_reason = if calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let delta = if calls.is_empty() {
        let ModelStep::Text(text) = step else {
            unreachable!("tool steps contain calls")
        };
        json!({ "content": text })
    } else {
        json!({ "tool_calls": calls.iter().enumerate().map(|(index, call)| json!({
            "index": index, "id": format!("call-{turn}-{index}"), "type": "function",
            "function": { "name": call.name, "arguments": call.arguments.to_string() }
        })).collect::<Vec<_>>() })
    };
    vec![
        start,
        json!({ "id": format!("chat-{turn}"), "model": raw_model_id(case), "choices": [{ "delta": delta, "finish_reason": null }] }),
        json!({ "id": format!("chat-{turn}"), "model": raw_model_id(case), "choices": [{ "delta": {}, "finish_reason": finish_reason }], "usage": { "prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16 } }),
        json!("[DONE]"),
    ]
}

fn step_calls(step: &ModelStep) -> Vec<FixtureToolCall> {
    match step {
        ModelStep::Tool { name, arguments } => vec![tool(name, arguments.clone())],
        ModelStep::MultipleTools { calls } => calls.clone(),
        ModelStep::Text(_) => Vec::new(),
    }
}

#[derive(Default)]
struct DeterministicTestRunner {
    requests: Mutex<Vec<ProcessRequest>>,
}

impl ProcessRunner for DeterministicTestRunner {
    fn execute(
        &self,
        request: ProcessRequest,
        _cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProcessEvent) -> Result<(), ProcessError>,
    ) -> Result<ProcessResult, ProcessError> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(request);
        let is_first = requests.len() == 1;
        drop(requests);
        on_event(ProcessEvent::Started { pid: None })?;
        let result = if is_first {
            ProcessResult {
                exit_code: Some(101),
                success: false,
                timed_out: false,
                cancelled: false,
                stdout: "test failed: score returned 5, expected 3".to_owned(),
                stderr: String::new(),
                duration_ms: 3,
            }
        } else {
            ProcessResult {
                exit_code: Some(0),
                success: true,
                timed_out: false,
                cancelled: false,
                stdout: "test result: ok. 1 passed".to_owned(),
                stderr: String::new(),
                duration_ms: 4,
            }
        };
        on_event(ProcessEvent::Stdout {
            chunk: result.stdout.clone(),
        })?;
        on_event(ProcessEvent::Exited {
            result: result.clone(),
        })?;
        Ok(result)
    }
}

struct ApproveAll;

impl ApprovalHandler for ApproveAll {
    fn request(
        &self,
        _tool: &harness_tools::ToolRequest,
    ) -> Result<bool, harness_agent::AgentError> {
        Ok(true)
    }
}

fn tool_registry(process_runner: Arc<DeterministicTestRunner>) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(ListDirectoryTool));
    tools.register(Box::new(GrepTool));
    tools.register(Box::new(ReadFileTool));
    tools.register(Box::new(ApplyPatchTool));
    tools.register(Box::new(ShellTool::new(
        process_runner,
        CancellationToken::new(),
    )));
    tools
}

fn create_fixture(workspace: &Path) {
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::create_dir_all(workspace.join("tests")).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"conformance_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        workspace.join("src/lib.rs"),
        "pub fn score(value: i32) -> i32 { value + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        workspace.join("tests/score.rs"),
        "use conformance_fixture::score;\n\n#[test]\nfn score_is_identity() {\n    assert_eq!(score(3), 3);\n    assert_eq!(score(8), 8);\n}\n",
    )
    .unwrap();
    let init = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(workspace)
        .output()
        .expect("initialize checkpoint fixture repository");
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
}

fn run_fixture(case: AdapterCase) {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    create_fixture(workspace);
    let mut server = MockServer::start(case);
    let provider = server.provider(case);
    assert_eq!(provider.descriptor().provider, case.provider);
    let sessions = Arc::new(JsonlSessionStore::new(workspace.join(".cogito/sessions")).unwrap());
    let session_store: Arc<dyn SessionStore> = sessions.clone();
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(ShadowCheckpointStore::new(workspace.join(".cogito/checkpoints")).unwrap());
    let process_runner = Arc::new(DeterministicTestRunner::default());
    let runner = AgentRunner::new(
        provider,
        case.model,
        tool_registry(Arc::clone(&process_runner)),
        Arc::new(PolicyEngine::new(ExecutionMode::Normal, workspace)),
        session_store,
        ContextBuilder::default(),
        AgentLimits {
            max_turns: 12,
            ..AgentLimits::default()
        },
        Arc::new(ApproveAll),
    )
    .with_checkpoints(Arc::clone(&checkpoints));
    let outcome = runner
        .run(
            &AgentTask {
                workspace_root: workspace.to_path_buf(),
                user_task: "Find the bug in this repository and fix it.".to_owned(),
                system_instructions: SYSTEM_INSTRUCTION.to_owned(),
                workspace: WorkspaceMetadata {
                    root: Some(workspace.to_path_buf()),
                    ..WorkspaceMetadata::default()
                },
                ..AgentTask::default()
            },
            &CancellationToken::new(),
        )
        .unwrap_or_else(|error| panic!("{} fixture failed: {error}", case.name));
    let requests = server.finish();
    assert_eq!(
        outcome.final_message,
        "Fixed score's off-by-one bug. The test command passed after the correction."
    );
    assert_eq!(outcome.tool_calls, 10);
    assert_eq!(outcome.turns, 10);
    assert!(
        outcome.model_tokens >= 160,
        "{} usage was not accumulated",
        case.name
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("src/lib.rs")).unwrap(),
        "pub fn score(value: i32) -> i32 { value }\n"
    );

    let session = sessions.load(&outcome.session_id).unwrap();
    assert_eq!(session.state().unwrap().status, SessionStatus::Completed);
    assert_session_is_provider_neutral(&session, case);
    assert!(
        session
            .events
            .iter()
            .any(|event| event.event_type == harness_session::EventType::ToolFailed),
        "{} lost tool errors",
        case.name
    );
    assert!(
        session
            .events
            .iter()
            .any(|event| event.event_type == harness_session::EventType::PolicyDecision),
        "{} bypassed policy",
        case.name
    );
    assert!(
        session
            .events
            .iter()
            .any(|event| event.event_type == harness_session::EventType::FileChanged),
        "{} omitted file changes",
        case.name
    );
    let session_json = serde_json::to_string(&session.events).unwrap();
    assert!(
        !session_json.contains(API_KEY),
        "{} persisted its credential",
        case.name
    );
    assert!(
        session_json.contains("test result: ok"),
        "{} did not record the test result",
        case.name
    );

    let checkpoint_info = checkpoints
        .list()
        .unwrap()
        .into_iter()
        .find(|info| info.session_id == outcome.session_id)
        .expect("session checkpoint");
    let checkpoint = checkpoints.load(&checkpoint_info.id).unwrap();
    assert!(
        !checkpoint.recorded_changes.is_empty(),
        "{} did not checkpoint its patch",
        case.name
    );

    let process_requests = process_runner.requests.lock().unwrap();
    assert_eq!(
        process_requests.len(),
        2,
        "{} did not retry tests after the failure",
        case.name
    );
    assert!(process_requests
        .iter()
        .all(|request| { request.args.join(" ").contains("cargo test --quiet") }));
    drop(process_requests);

    assert_protocol_continuation(&requests, case);
}

fn assert_session_is_provider_neutral(session: &Session, case: AdapterCase) {
    let mut response_count = 0;
    for event in &session.events {
        match &event.payload {
            EventPayload::ModelRequested { provider, .. } => assert_eq!(provider, case.provider),
            EventPayload::ModelResponse {
                provider,
                input_tokens,
                output_tokens,
                ..
            } => {
                response_count += 1;
                assert_eq!(provider, case.provider);
                assert!(input_tokens.is_some() && output_tokens.is_some());
            }
            _ => {}
        }
    }
    assert_eq!(
        response_count,
        coding_steps().len(),
        "{} lost model turns",
        case.name
    );
}

fn assert_protocol_continuation(requests: &[CapturedRequest], case: AdapterCase) {
    let setup_requests = usize::from(case.product.is_some()) * 2;
    let model_requests = &requests[setup_requests..];
    assert_eq!(
        model_requests.len(),
        coding_steps().len(),
        "{} request count",
        case.name
    );
    let rendered = model_requests
        .iter()
        .map(|request| request.body.to_string())
        .collect::<Vec<_>>();
    assert!(
        rendered[0].contains(SYSTEM_INSTRUCTION),
        "{} lost system instructions",
        case.name
    );
    assert!(
        rendered[1].contains("call-0-0") && rendered[1].contains("call-0-1"),
        "{} lost one of the multiple tool results",
        case.name
    );
    assert!(
        rendered[2].contains("[truncated]"),
        "{} did not signal truncated tool output",
        case.name
    );
    assert!(
        rendered[3].contains("Tool read_file failed"),
        "{} did not return tool errors to the model",
        case.name
    );
    assert!(
        rendered[7].contains("test failed"),
        "{} did not continue after a failing test",
        case.name
    );
    assert!(
        rendered[9].contains("test result: ok"),
        "{} did not continue after a passing test",
        case.name
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.path != "/api.json" && !request.path.ends_with("/models"))
            .all(|request| {
                request.headers.contains(API_KEY) && !request.body.to_string().contains(API_KEY)
            }),
        "{} leaked or omitted its request credential",
        case.name
    );
}

#[test]
fn every_provider_protocol_passes_the_shared_coding_agent_fixture() {
    for case in CASES {
        run_fixture(*case);
    }
}

#[test]
fn authentication_rate_limit_and_invalid_model_errors_normalize_on_every_route() {
    for case in CASES {
        for status in [401, 404, 429] {
            let mut server = ScenarioMockServer::start(*case, Some(status));
            let provider = server.provider(*case);
            let request = ModelRequest::new(case.model, vec![Message::user_text("hello")]);
            let mut events = Vec::new();
            let result = provider.generate(&request, &mut |event| {
                events.push(event);
                Ok(())
            });
            assert!(
                matches!(result, Err(ProviderError::Request { status: Some(actual), .. }) if actual == status),
                "{} did not normalize HTTP {status}: {result:?}",
                case.name
            );
            let event_json = serde_json::to_string(&events).unwrap();
            assert!(
                !event_json.contains(API_KEY),
                "{} exposed a credential in HTTP {status} events",
                case.name
            );
            server.finish();
        }
    }
}

#[test]
fn cancellation_is_observed_on_every_protocol_route() {
    for case in CASES {
        let mut server = ScenarioMockServer::start(*case, None);
        let provider = server.provider(*case);
        let request = ModelRequest::new(case.model, vec![Message::user_text("hello")]);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = thread::spawn(move || {
            let mut events = Vec::new();
            let result = provider.generate_cancellable(
                &request,
                &mut |event| {
                    events.push(event);
                    Ok(())
                },
                &|| worker_cancelled.load(Ordering::Acquire),
            );
            (result, events)
        });
        server.wait_for_model_request();
        cancelled.store(true, Ordering::Release);
        let (result, events) = worker.join().expect("cancellation worker");
        assert!(
            matches!(result, Err(ProviderError::Cancelled)),
            "{} did not normalize cancellation: {result:?}",
            case.name
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, ModelStreamEvent::ResponseFailed { .. })));
        assert!(!serde_json::to_string(&events).unwrap().contains(API_KEY));
        server.finish();
    }
}
