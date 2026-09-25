use std::collections::BTreeMap;

use harness_models::{
    ContentBlock, FinishReason, Message, MockProvider, ModelProvider, ModelRequest, OpenAiProvider,
    ProviderError, Role, StreamDelta, StreamDeltaKind, ToolDefinition,
};
use serde_json::json;

#[test]
fn mock_provider_returns_deterministic_text_and_usage() {
    let provider = MockProvider::new("test-model");
    let request = ModelRequest::new("ignored", vec![Message::user_text("hello")]);

    let first = provider.complete(&request).unwrap();
    let second = provider.complete(&request).unwrap();

    assert_eq!(first, second);
    assert_eq!(first.text(), "mock response: hello");
    assert_eq!(first.finish_reason, FinishReason::Stop);
    let usage = first.usage.unwrap();
    assert_eq!(usage.input_tokens, Some(5));
    assert_eq!(usage.output_tokens, Some(3));
    assert_eq!(usage.total_tokens, Some(8));
}

#[test]
fn mock_provider_streams_text_deltas_in_order() {
    let provider = MockProvider::new("test-model");
    let request = ModelRequest::new("test-model", vec![Message::user_text("stream this")]);
    let mut deltas = Vec::new();

    let response = provider
        .stream(&request, &mut |delta: StreamDelta| {
            deltas.push(delta);
            Ok(())
        })
        .unwrap();

    assert_eq!(response.text(), "mock response: stream this");
    assert_eq!(
        deltas
            .iter()
            .map(|delta| delta.sequence)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert!(deltas
        .iter()
        .all(|delta| matches!(delta.delta, StreamDeltaKind::Text { .. })));
    assert_eq!(
        deltas.last().unwrap().finish_reason,
        Some(FinishReason::Stop)
    );
    assert_eq!(deltas.last().unwrap().usage, response.usage);
}

#[test]
fn mock_provider_serializes_and_streams_tool_calls() {
    let provider = MockProvider::new("test-model");
    let request = ModelRequest {
        tools: vec![ToolDefinition {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            input_schema: json!({ "type": "object" }),
        }],
        ..ModelRequest::new("test-model", vec![Message::user_text("read it")])
    };
    let mut deltas = Vec::new();

    let response = provider
        .stream(&request, &mut |delta| {
            deltas.push(delta);
            Ok(())
        })
        .unwrap();

    assert_eq!(response.tool_calls[0].name, "read_file");
    assert_eq!(
        response.tool_calls[0].arguments,
        json!({ "input": "read it" })
    );
    assert!(matches!(deltas[0].delta, StreamDeltaKind::ToolCall { .. }));
    let serialized = serde_json::to_value(&response.tool_calls[0]).unwrap();
    assert_eq!(serialized["name"], "read_file");
    assert!(serialized.get("api_key").is_none());
}

#[test]
fn common_message_and_content_types_are_serializable() {
    let message = Message {
        role: Role::User,
        content: vec![
            ContentBlock::Text {
                text: "describe".to_owned(),
            },
            ContentBlock::Image {
                media_type: "image/png".to_owned(),
                data: "encoded".to_owned(),
            },
        ],
        name: Some("user".to_owned()),
        tool_call_id: None,
    };
    let serialized = serde_json::to_value(&message).unwrap();

    assert_eq!(serialized["role"], "user");
    assert_eq!(serialized["content"][1]["type"], "image");
    assert!(serialized.get("tool_call_id").is_none());
}

#[test]
fn missing_real_provider_credentials_fail_without_exposing_a_key() {
    let provider = OpenAiProvider::with_api_key(
        "https://example.invalid/v1",
        "gpt-test",
        "TEST_MODEL_KEY",
        "",
    );
    let error = provider
        .complete(&ModelRequest::new(
            "gpt-test",
            vec![Message::user_text("hello")],
        ))
        .unwrap_err();

    assert!(matches!(
        error,
        ProviderError::MissingApiKey {
            provider: "openai",
            ..
        }
    ));
    assert!(!error.to_string().contains("Bearer"));
}

#[test]
fn provider_capabilities_cover_streaming_tools_vision_reasoning_and_context() {
    let capabilities = MockProvider::new("test-model").capabilities();

    assert!(capabilities.streaming);
    assert!(capabilities.tool_calling);
    assert!(capabilities.vision);
    assert!(capabilities.reasoning);
    assert_eq!(capabilities.context_window, Some(8_192));
    let _metadata: BTreeMap<String, String> = BTreeMap::new();
}
