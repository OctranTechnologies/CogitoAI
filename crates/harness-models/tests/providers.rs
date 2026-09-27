use std::collections::BTreeMap;

use harness_models::{
    AnthropicProvider, ContentBlock, DeterministicMockProvider, FinishReason, Message,
    MockProvider, MockScenario, ModelConfig, ModelProvider, ModelRequest, ModelStreamEvent,
    OpenAiProvider, ProviderError, ProviderKind, Role, ToolCall, ToolDefinition,
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
    let mut events = Vec::new();

    let response = provider
        .stream(&request, &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();

    assert_eq!(response.text(), "mock response: stream this");
    assert!(matches!(
        events.first(),
        Some(ModelStreamEvent::ResponseStarted { .. })
    ));
    assert!(
        events
            .iter()
            .filter(|event| matches!(event, ModelStreamEvent::TextDelta { .. }))
            .count()
            > 1
    );
    assert!(matches!(
        events.last(),
        Some(ModelStreamEvent::ResponseCompleted {
            finish_reason: FinishReason::Stop
        })
    ));
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
    let mut events = Vec::new();

    let response = provider
        .stream(&request, &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();

    assert_eq!(response.tool_calls[0].name, "read_file");
    assert_eq!(
        response.tool_calls[0].arguments,
        json!({ "input": "read it" })
    );
    assert!(events
        .iter()
        .any(|event| matches!(event, ModelStreamEvent::ToolCallStarted { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, ModelStreamEvent::ToolCallArgumentsDelta { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, ModelStreamEvent::ToolCallCompleted { .. })));
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
        tool_calls: Vec::new(),
        is_error: false,
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
fn anthropic_model_metadata_and_provider_defaults_are_model_aware() {
    let known = AnthropicProvider::with_api_key(
        "https://api.anthropic.com/v1",
        "claude-sonnet-4-6",
        "ANTHROPIC_API_KEY",
        "test-key",
    );
    let descriptor = known.descriptor();
    assert_eq!(descriptor.provider, "anthropic");
    assert!(descriptor.capabilities.streaming);
    assert!(descriptor.capabilities.tool_calling);
    assert!(descriptor.capabilities.parallel_tool_calls);
    assert!(descriptor.capabilities.reasoning);
    assert!(descriptor.capabilities.configurable_reasoning_effort);

    let unknown = AnthropicProvider::with_api_key(
        "https://api.anthropic.com/v1",
        "claude-private-experiment",
        "ANTHROPIC_API_KEY",
        "test-key",
    );
    assert!(!unknown.descriptor().capabilities.reasoning);
    assert!(!unknown.descriptor().capabilities.image_input);
    assert!(unknown.descriptor().capabilities.tool_calling);

    let anthropic_defaults = ModelConfig::for_provider(ProviderKind::Anthropic);
    assert_eq!(anthropic_defaults.model, "claude-sonnet-4-6");
    assert_eq!(anthropic_defaults.api_key_env, "ANTHROPIC_API_KEY");
    assert_eq!(anthropic_defaults.base_url, "https://api.anthropic.com/v1");
    assert_eq!(anthropic_defaults.context_window, None);

    let mut config = ModelConfig::default();
    config.select_provider(ProviderKind::Anthropic);
    assert_eq!(config.model, anthropic_defaults.model);
    assert_eq!(config.api_key_env, anthropic_defaults.api_key_env);
    assert_eq!(config.base_url, anthropic_defaults.base_url);

    let mut customized = anthropic_defaults.clone();
    customized.model = "claude-custom-model-id".to_owned();
    customized.base_url = "https://gateway.example/v1".to_owned();
    customized.api_key_env = "CUSTOM_ANTHROPIC_KEY".to_owned();
    customized.select_provider(ProviderKind::OpenAi);
    assert_eq!(customized.model, "claude-custom-model-id");
    assert_eq!(customized.base_url, "https://gateway.example/v1");
    assert_eq!(customized.api_key_env, "CUSTOM_ANTHROPIC_KEY");
}

#[test]
fn provider_descriptors_report_known_capabilities_without_claiming_unknown_ones() {
    let capabilities = MockProvider::new("test-model").descriptor().capabilities;

    assert!(capabilities.text_input);
    assert!(capabilities.streaming);
    assert!(capabilities.tool_calling);
    assert!(!capabilities.image_input);
    assert!(!capabilities.reasoning);
    assert_eq!(capabilities.context_window, Some(8_192));
    let serialized = serde_json::to_value(capabilities).unwrap();
    assert_eq!(serialized["vision"], false);
    assert!(serialized.get("image_input").is_none());
    let _metadata: BTreeMap<String, String> = BTreeMap::new();
}

#[test]
fn normalized_stream_event_names_are_stable_and_provider_neutral() {
    let events = [
        (
            ModelStreamEvent::ResponseStarted {
                id: None,
                model: "fixture".to_owned(),
            },
            "response.started",
        ),
        (
            ModelStreamEvent::TextDelta {
                text: "hello".to_owned(),
            },
            "text.delta",
        ),
        (
            ModelStreamEvent::ReasoningDelta {
                text: "summary".to_owned(),
            },
            "reasoning.delta",
        ),
        (
            ModelStreamEvent::ToolCallStarted {
                index: 0,
                id: None,
                name: None,
            },
            "tool_call.started",
        ),
        (
            ModelStreamEvent::ToolCallArgumentsDelta {
                index: 0,
                delta: "{}".to_owned(),
            },
            "tool_call.arguments.delta",
        ),
        (
            ModelStreamEvent::ToolCallCompleted {
                index: 0,
                call: ToolCall {
                    id: "call".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: json!({}),
                },
            },
            "tool_call.completed",
        ),
        (
            ModelStreamEvent::UsageUpdated {
                usage: harness_models::Usage::default(),
            },
            "usage.updated",
        ),
        (
            ModelStreamEvent::ResponseCompleted {
                finish_reason: FinishReason::Stop,
            },
            "response.completed",
        ),
        (
            ModelStreamEvent::ResponseFailed {
                error: "failed".to_owned(),
            },
            "response.failed",
        ),
    ];
    for (event, expected) in events {
        assert_eq!(serde_json::to_value(event).unwrap()["type"], expected);
    }
}

#[test]
fn deterministic_text_only_model_streams_normalized_text() {
    let provider = DeterministicMockProvider::text_only("text-fixture", "hello world");
    let request = ModelRequest::new("text-fixture", vec![Message::user_text("prompt")]);
    let mut events = Vec::new();
    let response = provider
        .generate(&request, &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();

    assert_eq!(response.text(), "hello world");
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match event {
                ModelStreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>(),
        "hello world"
    );
}

#[test]
fn deterministic_model_streams_one_tool_call_in_order() {
    let call = ToolCall {
        id: "call-1".to_owned(),
        name: "read_file".to_owned(),
        arguments: json!({ "path": "src/lib.rs" }),
    };
    let provider = DeterministicMockProvider::streaming_tool_call("tool-fixture", call.clone());
    let request = ModelRequest::new("tool-fixture", vec![Message::user_text("read the crate")]);
    let mut events = Vec::new();
    let response = provider
        .generate(&request, &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();

    assert_eq!(response.tool_calls, [call]);
    let completed_index = events
        .iter()
        .position(|event| matches!(event, ModelStreamEvent::ToolCallCompleted { index: 0, .. }))
        .unwrap();
    let finished_index = events
        .iter()
        .position(|event| matches!(event, ModelStreamEvent::ResponseCompleted { .. }));
    assert!(completed_index < finished_index.unwrap());
}

#[test]
fn deterministic_model_streams_tool_lifecycle_and_parallel_calls() {
    let call = |id: &str, name: &str| ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments: json!({ "id": id }),
    };
    let calls = vec![call("one", "read_file"), call("two", "search")];
    let provider = DeterministicMockProvider::multiple_tool_calls("tools-fixture", calls.clone());
    let request = ModelRequest::new("tools-fixture", vec![Message::user_text("inspect")]);
    let mut events = Vec::new();
    let response = provider
        .generate(&request, &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();

    assert_eq!(response.tool_calls, calls);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ModelStreamEvent::ToolCallCompleted { .. }))
            .count(),
        2
    );
}

#[test]
fn deterministic_midstream_error_emits_failure_after_partial_content() {
    let provider = DeterministicMockProvider::new(
        "error-fixture",
        MockScenario::ErrorMidStream {
            partial_text: "partial".to_owned(),
            error: "fixture failure".to_owned(),
        },
    );
    let request = ModelRequest::new("error-fixture", vec![Message::user_text("prompt")]);
    let mut events = Vec::new();
    assert!(provider
        .generate(&request, &mut |event| {
            events.push(event);
            Ok(())
        })
        .is_err());
    assert!(matches!(events[1], ModelStreamEvent::TextDelta { .. }));
    assert!(
        matches!(events.last(), Some(ModelStreamEvent::ResponseFailed { error }) if error.contains("fixture failure"))
    );
}

#[test]
fn capability_validation_rejects_tools_for_text_only_model() {
    let provider = DeterministicMockProvider::text_only("text-fixture", "hello");
    let request = ModelRequest {
        tools: vec![ToolDefinition {
            name: "read_file".to_owned(),
            description: "Read a file".to_owned(),
            input_schema: json!({ "type": "object" }),
        }],
        ..ModelRequest::new("text-fixture", vec![Message::user_text("prompt")])
    };
    assert!(matches!(
        provider.generate(&request, &mut |_| Ok(())),
        Err(ProviderError::UnsupportedCapability {
            capability: "tool calling"
        })
    ));
}
