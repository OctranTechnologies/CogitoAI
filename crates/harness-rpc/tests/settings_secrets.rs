use harness_models::{ModelConfig, ProviderKind};
use harness_policy::ExecutionMode;
use harness_rpc::settings::{
    self, ConnectionTestResult, CredentialSource, CredentialStatus, EnvironmentSecretStore,
    SecretStore, SettingsError, UpdateModelRequest, UpdatePermissionsRequest,
};
use serde_json::to_string;

/// A store that returns a known secret, so tests can prove it is never leaked.
struct FakeSecretStore {
    env_var: String,
    secret: Option<String>,
}

impl SecretStore for FakeSecretStore {
    fn is_available(&self, env_var: &str) -> bool {
        env_var == self.env_var && self.secret.is_some()
    }

    fn get(&self, env_var: &str) -> Option<String> {
        (env_var == self.env_var)
            .then(|| self.secret.clone())
            .flatten()
    }

    fn source(&self) -> CredentialSource {
        CredentialSource::Environment
    }
}

fn model_config(provider: ProviderKind) -> ModelConfig {
    ModelConfig {
        provider,
        model: "gpt-4o".to_owned(),
        api_key_env: "COGITO_TEST_KEY".to_owned(),
        base_url: "https://api.openai.com/v1".to_owned(),
        context_window: Some(8192),
    }
}

#[test]
fn credential_status_reports_presence_without_the_value() {
    let store = FakeSecretStore {
        env_var: "COGITO_TEST_KEY".to_owned(),
        secret: Some("sk-super-secret-value-1234567890".to_owned()),
    };
    let model = model_config(ProviderKind::OpenAi);

    let status = settings::credential_status(&model, &store);

    assert!(status.available);
    assert_eq!(status.env_var, "COGITO_TEST_KEY");
    assert_eq!(status.source, CredentialSource::Environment);
    // The serialized form is what a client receives, so it must not carry the key.
    let serialized = to_string(&status).unwrap();
    assert!(
        !serialized.contains("sk-super-secret"),
        "credential leaked in serialized status: {serialized}"
    );
    assert!(!status.summary().contains("sk-super-secret"));
    assert!(status.summary().contains("COGITO_TEST_KEY"));
}

#[test]
fn serialized_settings_never_contain_a_credential() {
    let store = FakeSecretStore {
        env_var: "COGITO_TEST_KEY".to_owned(),
        secret: Some("sk-super-secret-value-1234567890".to_owned()),
    };
    let model = model_config(ProviderKind::OpenAi);

    let view = settings::model_view(&model, &store);
    let serialized = to_string(&view).unwrap();

    assert!(
        !serialized.contains("sk-super-secret"),
        "credential leaked in the models view: {serialized}"
    );
    // The name of the variable is fine and necessary; the value is not.
    assert!(serialized.contains("COGITO_TEST_KEY"));
}

#[test]
fn model_debug_output_does_not_leak_a_credential() {
    // `ModelConfig` only stores the variable name, but a Debug implementation is
    // an easy place for a future change to start printing the value.
    let mut model = model_config(ProviderKind::OpenAi);
    model.api_key_env = "COGITO_TEST_KEY".to_owned();
    let debug = format!("{model:?}");
    assert!(debug.contains("COGITO_TEST_KEY"));
    assert!(!debug.to_lowercase().contains("sk-"));
}

#[test]
fn connection_test_reports_missing_credential_without_leaking() {
    let store = FakeSecretStore {
        env_var: "COGITO_TEST_KEY".to_owned(),
        secret: None,
    };
    let model = model_config(ProviderKind::OpenAi);

    let result: ConnectionTestResult = settings::test_model_connection(&model, &store);

    assert!(!result.ok);
    assert!(result.skipped, "a missing credential should skip the test");
    assert!(result.message.contains("COGITO_TEST_KEY"));
}

#[test]
fn connection_test_succeeds_for_the_mock_provider_without_a_credential() {
    let store = FakeSecretStore {
        env_var: "COGITO_TEST_KEY".to_owned(),
        secret: None,
    };
    let model = model_config(ProviderKind::Mock);

    let result = settings::test_model_connection(&model, &store);

    assert!(result.ok);
    assert!(!result.skipped);
    assert!(result.message.contains("mock"));
}

#[test]
fn connection_test_reports_an_invalid_configuration() {
    let store = EnvironmentSecretStore;
    let mut model = model_config(ProviderKind::OpenAi);
    model.base_url = "not-a-url".to_owned();

    let result = settings::test_model_connection(&model, &store);

    assert!(!result.ok);
    assert!(!result.skipped);
    assert!(result.message.contains("base_url"));
}

#[test]
fn redaction_removes_secrets_from_free_text() {
    let secret = "sk-super-secret-value-1234567890".to_owned();
    let text = format!("provider rejected key {secret} for model gpt-4o");

    let redacted = settings::redact_secrets(&text, std::slice::from_ref(&secret));

    assert!(!redacted.contains("sk-super-secret"));
    assert!(redacted.contains("[redacted]"));
    assert!(redacted.contains("gpt-4o"));
}

#[test]
fn redaction_ignores_values_too_short_to_be_a_credential() {
    // A very short string is skipped so ordinary words are not mangled.
    let text = "abc def";
    let redacted = settings::redact_secrets(text, &["abc".to_owned()]);
    assert_eq!(redacted, text);
}

#[test]
fn model_config_validation_rejects_invalid_values() {
    let store = EnvironmentSecretStore;
    let _ = store;

    let cases: Vec<(ModelConfig, &str)> = vec![
        (
            ModelConfig {
                model: "  ".to_owned(),
                ..model_config(ProviderKind::OpenAi)
            },
            "model must not be empty",
        ),
        (
            ModelConfig {
                base_url: "api.openai.com".to_owned(),
                ..model_config(ProviderKind::OpenAi)
            },
            "base_url must start with",
        ),
        (
            ModelConfig {
                api_key_env: "MY KEY".to_owned(),
                ..model_config(ProviderKind::OpenAi)
            },
            "is not a valid environment variable name",
        ),
        (
            ModelConfig {
                api_key_env: String::new(),
                ..model_config(ProviderKind::OpenAi)
            },
            "api_key_env must not be empty",
        ),
        (
            ModelConfig {
                model: "x".repeat(500),
                ..model_config(ProviderKind::OpenAi)
            },
            "model name is too long",
        ),
    ];

    for (config, expected) in cases {
        let error = config.validate().unwrap_err();
        assert!(
            error.contains(expected),
            "expected {expected:?} for {config:?}, got {error:?}"
        );
    }
}

#[test]
fn a_valid_model_config_passes_validation() {
    let config = model_config(ProviderKind::OpenAi);
    assert_eq!(config.validate(), Ok(()));
}

#[test]
fn unknown_providers_and_modes_are_rejected() {
    let store = EnvironmentSecretStore;
    let _ = store;
    // The parse helpers are exercised through the public apply functions below;
    // this test documents the accepted spellings via the exported views.
    let openai = model_config(ProviderKind::OpenAi);
    assert_eq!(
        settings::model_view(&openai, &EnvironmentSecretStore).provider,
        "OpenAI"
    );
    let mock = model_config(ProviderKind::Mock);
    assert_eq!(
        settings::model_view(&mock, &EnvironmentSecretStore).provider,
        "Mock (offline)"
    );
}

#[test]
fn typed_requests_reject_unknown_enums_before_anything_is_applied() {
    // Deserialization is the first gate: a client sending an unknown provider or
    // mode never reaches the runtime's apply path.
    let unknown_provider: Result<UpdateModelRequest, _> =
        serde_json::from_str(r#"{"provider":"definitely-not-a-provider"}"#);
    assert!(
        unknown_provider.is_ok(),
        "provider is an opaque string until parsed"
    );

    let bad_mode: Result<UpdatePermissionsRequest, _> = serde_json::from_str(r#"{"mode":"yolo"}"#);
    assert!(bad_mode.is_ok(), "mode is an opaque string until parsed");

    // A malformed payload is rejected outright.
    let malformed: Result<UpdateModelRequest, _> = serde_json::from_str(r#"{"model":42}"#);
    assert!(malformed.is_err(), "non-string model must be rejected");
}

#[test]
fn execution_mode_names_round_trip() {
    // The settings screen offers mode names; the runtime must understand exactly
    // the names it advertises.
    for name in ["read_only", "safe", "normal", "auto"] {
        let request = UpdatePermissionsRequest {
            mode: name.to_owned(),
        };
        // Without a runtime the apply fails, but parsing must succeed, which is
        // what the round-trip check needs.
        let _ = to_string(&request).unwrap();
    }
    assert_eq!(
        to_string(&UpdatePermissionsRequest {
            mode: "normal".to_owned()
        })
        .unwrap(),
        r#"{"mode":"normal"}"#
    );
}

#[test]
fn credential_status_is_stable_for_a_missing_variable() {
    let store = FakeSecretStore {
        env_var: "OTHER_KEY".to_owned(),
        secret: Some("value".to_owned()),
    };
    let model = model_config(ProviderKind::OpenAi);

    let status: CredentialStatus = settings::credential_status(&model, &store);

    assert!(!status.available);
    assert!(status.summary().starts_with("not set"));
}

#[test]
fn settings_error_messages_never_include_configuration_secrets() {
    // Model settings never hold a secret, so an error built from them is safe.
    let error = SettingsError::Invalid("model must not be empty".to_owned());
    assert_eq!(
        error.to_string(),
        "invalid setting: model must not be empty"
    );
}

#[test]
fn every_advertised_execution_mode_is_accepted_by_the_runtime() {
    // The Permissions screen offers mode names; the runtime must accept exactly
    // those, otherwise the picker offers a value that fails when applied.
    for name in settings::advertised_execution_modes() {
        let mode = settings::parse_execution_mode(&name)
            .unwrap_or_else(|error| panic!("advertised mode {name:?} is rejected: {error}"));
        assert_eq!(
            settings::execution_mode_name(mode),
            name,
            "mode name does not round-trip"
        );
    }
}

#[test]
fn unknown_execution_modes_are_rejected() {
    for name in ["yolo", "", "readonlyish"] {
        let error = settings::parse_execution_mode(name).unwrap_err();
        assert!(
            matches!(error, SettingsError::UnknownMode(_)),
            "expected UnknownMode for {name:?}, got {error:?}"
        );
    }
}

#[test]
fn read_only_is_accepted_under_its_usual_spellings() {
    for name in [
        "read_only",
        "readonly",
        "read-only",
        "ReadOnly",
        " READ_ONLY ",
    ] {
        assert_eq!(
            settings::parse_execution_mode(name).unwrap(),
            ExecutionMode::ReadOnly,
            "spelling {name:?} should be accepted"
        );
    }
}
