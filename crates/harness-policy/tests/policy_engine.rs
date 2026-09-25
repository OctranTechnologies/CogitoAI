use std::fs;
use std::path::Path;

use harness_policy::{
    ExecutionMode, OperationKind, PolicyDecision, PolicyEngine, PolicyRequest, PolicyRule,
};
use tempfile::tempdir;

fn request(
    root: &Path,
    mode: ExecutionMode,
    operation: OperationKind,
    path: Option<&str>,
    command: Option<&str>,
) -> PolicyRequest {
    PolicyRequest {
        tool_name: "test".to_owned(),
        operation,
        workspace_root: root.to_path_buf(),
        path: path.map(|path| root.join(path)),
        command: command.map(str::to_owned),
        mode,
    }
}

fn evaluate(engine: &PolicyEngine, request: &PolicyRequest) -> harness_policy::PolicyEvaluation {
    engine.evaluate_request(request).unwrap()
}

fn rule(name: &str, action: PolicyDecision) -> PolicyRule {
    PolicyRule {
        name: name.to_owned(),
        action,
        priority: 0,
        tools: None,
        operations: None,
        modes: None,
        paths: None,
        command_patterns: None,
    }
}

#[test]
fn read_only_allows_reads_and_denies_mutations() {
    let temporary = tempdir().unwrap();
    let engine = PolicyEngine::new(ExecutionMode::ReadOnly, temporary.path());

    assert_eq!(
        evaluate(
            &engine,
            &request(
                temporary.path(),
                ExecutionMode::ReadOnly,
                OperationKind::Read,
                Some("README.md"),
                None
            )
        )
        .decision,
        PolicyDecision::Allow
    );
    assert_eq!(
        evaluate(
            &engine,
            &request(
                temporary.path(),
                ExecutionMode::ReadOnly,
                OperationKind::Write,
                Some("README.md"),
                None
            )
        )
        .decision,
        PolicyDecision::Deny
    );
    assert_eq!(
        evaluate(
            &engine,
            &request(
                temporary.path(),
                ExecutionMode::ReadOnly,
                OperationKind::Command,
                None,
                Some("pwd")
            )
        )
        .decision,
        PolicyDecision::Deny
    );
}

#[test]
fn safe_requires_approval_for_mutations_and_commands() {
    let temporary = tempdir().unwrap();
    let engine = PolicyEngine::new(ExecutionMode::Safe, temporary.path());

    assert_eq!(
        evaluate(
            &engine,
            &request(
                temporary.path(),
                ExecutionMode::Safe,
                OperationKind::Read,
                Some("file.txt"),
                None
            )
        )
        .decision,
        PolicyDecision::Allow
    );
    assert_eq!(
        evaluate(
            &engine,
            &request(
                temporary.path(),
                ExecutionMode::Safe,
                OperationKind::Write,
                Some("file.txt"),
                None
            )
        )
        .decision,
        PolicyDecision::Ask
    );
    assert_eq!(
        evaluate(
            &engine,
            &request(
                temporary.path(),
                ExecutionMode::Safe,
                OperationKind::Command,
                None,
                Some("echo hello")
            )
        )
        .decision,
        PolicyDecision::Ask
    );
}

#[test]
fn normal_allows_project_edits_and_safe_commands_but_asks_dangerous_commands() {
    let temporary = tempdir().unwrap();
    let engine = PolicyEngine::new(ExecutionMode::Normal, temporary.path());

    assert_eq!(
        evaluate(
            &engine,
            &request(
                temporary.path(),
                ExecutionMode::Normal,
                OperationKind::Write,
                Some("src/main.rs"),
                None
            )
        )
        .decision,
        PolicyDecision::Allow
    );
    assert_eq!(
        evaluate(
            &engine,
            &request(
                temporary.path(),
                ExecutionMode::Normal,
                OperationKind::Command,
                None,
                Some("cargo test")
            )
        )
        .decision,
        PolicyDecision::Allow
    );
    for command in [
        "rm -rf build",
        "git push origin main",
        "curl https://example.com",
    ] {
        assert_eq!(
            evaluate(
                &engine,
                &request(
                    temporary.path(),
                    ExecutionMode::Normal,
                    OperationKind::Command,
                    None,
                    Some(command)
                )
            )
            .decision,
            PolicyDecision::Ask,
            "{command}"
        );
    }
}

#[test]
fn explicit_deny_rules_beat_allow_rules_and_auto() {
    let temporary = tempdir().unwrap();
    let mut engine = PolicyEngine::new(ExecutionMode::Auto, temporary.path());
    let mut allow = rule("allow-shell", PolicyDecision::Allow);
    allow.priority = 100;
    allow.tools = Some(vec!["shell".to_owned()]);
    allow.command_patterns = Some(vec!["rm*".to_owned()]);
    let mut deny = rule("deny-destructive", PolicyDecision::Deny);
    deny.priority = -100;
    deny.command_patterns = Some(vec!["rm -rf*".to_owned()]);
    engine.rules = vec![allow, deny];

    let result = evaluate(
        &engine,
        &request(
            temporary.path(),
            ExecutionMode::Auto,
            OperationKind::Command,
            None,
            Some("rm -rf build"),
        ),
    );

    assert_eq!(result.decision, PolicyDecision::Deny);
    assert_eq!(result.rule, "deny-destructive");
}

#[test]
fn path_globs_cover_nested_project_files() {
    let temporary = tempdir().unwrap();
    let mut engine = PolicyEngine::new(ExecutionMode::Auto, temporary.path());
    let mut generated = rule("ask-generated", PolicyDecision::Ask);
    generated.paths = Some(vec!["generated/**".to_owned()]);
    engine.rules = vec![generated];

    let result = evaluate(
        &engine,
        &request(
            temporary.path(),
            ExecutionMode::Auto,
            OperationKind::Write,
            Some("generated/api.rs"),
            None,
        ),
    );
    assert_eq!(result.decision, PolicyDecision::Ask);
}

#[test]
fn high_risk_and_outside_paths_are_denied_by_default() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("workspace");
    fs::create_dir_all(&root).unwrap();
    let engine = PolicyEngine::new(ExecutionMode::Auto, &root);

    for path in [
        ".env",
        ".env.local",
        ".ssh/config",
        "id_rsa",
        "private.key",
        "../outside.txt",
    ] {
        let result = evaluate(
            &engine,
            &PolicyRequest {
                tool_name: "read_file".to_owned(),
                operation: OperationKind::Read,
                workspace_root: root.clone(),
                path: Some(root.join(path)),
                command: None,
                mode: ExecutionMode::Auto,
            },
        );
        assert_eq!(result.decision, PolicyDecision::Deny, "{path}");
    }
}

#[test]
fn loads_policy_rules_from_agent_toml() {
    let temporary = tempdir().unwrap();
    let contents = r#"
[policy]
mode = "safe"

[[policy.rules]]
name = "deny-env"
action = "deny"
paths = [".env*"]
"#;
    let engine = PolicyEngine::from_toml(contents, temporary.path()).unwrap();

    assert_eq!(engine.mode, ExecutionMode::Safe);
    assert_eq!(engine.rules.len(), 1);
    assert_eq!(engine.rules[0].action, PolicyDecision::Deny);
    let result = evaluate(
        &engine,
        &request(
            temporary.path(),
            ExecutionMode::Safe,
            OperationKind::Read,
            Some(".env"),
            None,
        ),
    );
    assert_eq!(result.decision, PolicyDecision::Deny);
}

#[test]
fn invalid_policy_configuration_is_reported() {
    let temporary = tempdir().unwrap();
    let result = PolicyEngine::from_toml("[policy]\nmode = \"unsupported\"", temporary.path());
    assert!(result.is_err());
}
