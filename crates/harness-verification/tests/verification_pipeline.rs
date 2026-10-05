use std::path::PathBuf;
use std::sync::Arc;

use harness_core::{
    CommandSpec, GitDescription, Manifest, MonorepoDescription, PackageManager, ProjectCommands,
    WorkingTreeState, WorkspaceConfiguration, WorkspaceDescription,
};
use harness_tools::LocalProcessRunner;
use harness_verification::{
    CommandVerifier, VerificationCategory, VerificationPlan, VerificationPlanner,
    VerificationRequest, Verifier,
};
use tempfile::tempdir;

fn workspace(commands: ProjectCommands, git_available: bool) -> WorkspaceDescription {
    WorkspaceDescription {
        current_directory: PathBuf::from("/workspace"),
        repository_root: Some(PathBuf::from("/workspace")),
        git: GitDescription {
            available: git_available,
            branch: Some("main".to_owned()),
            working_tree: Some(WorkingTreeState::Clean),
        },
        languages: vec![harness_core::Language::Rust],
        manifests: vec![Manifest {
            path: PathBuf::from("/workspace/Cargo.toml"),
            kind: harness_core::ManifestKind::Cargo,
        }],
        monorepo: MonorepoDescription {
            is_monorepo: false,
            indicators: Vec::new(),
        },
        configuration: WorkspaceConfiguration {
            package_manager: Some(PackageManager::Cargo),
            commands,
            source: None,
        },
        instructions: Vec::new(),
    }
}

fn shell(command: &str) -> CommandSpec {
    #[cfg(windows)]
    {
        CommandSpec::new("cmd", ["/C", command])
    }
    #[cfg(not(windows))]
    {
        CommandSpec::new("sh", ["-c", command])
    }
}

#[test]
fn plans_use_configured_commands_and_targeted_tests_after_changes() {
    let commands = ProjectCommands {
        test: vec![shell("cargo test")],
        build: vec![shell("cargo build")],
        format: vec![shell("cargo fmt --check")],
        lint: vec![shell("cargo clippy")],
        typecheck: vec![],
    };
    let workspace = workspace(commands, true);

    let all = VerificationPlan::all(&workspace);
    let targeted = VerificationPlan::for_changes(&workspace, &[PathBuf::from("src/lib.rs")]);

    assert!(all
        .steps
        .iter()
        .any(|step| step.category == VerificationCategory::GeneralTest));
    assert!(!all
        .steps
        .iter()
        .any(|step| step.category == VerificationCategory::TargetedTest));
    assert!(targeted
        .steps
        .iter()
        .any(|step| step.category == VerificationCategory::GeneralTest));
    assert!(targeted
        .steps
        .iter()
        .any(|step| step.category == VerificationCategory::GitDiff));
    assert!(targeted
        .steps
        .iter()
        .find(|step| step.category == VerificationCategory::Build)
        .is_some_and(|step| step.command.program == "cmd" || step.command.program == "sh"));
}

#[test]
fn targets_tests_to_the_changed_cargo_package_and_runs_cheap_checks_first() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    std::fs::create_dir_all(root.join("crates/widget/src")).unwrap();
    std::fs::create_dir_all(root.join("crates/widget/tests")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/widget\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("crates/widget/Cargo.toml"),
        "[package]\nname = \"widget\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let mut discovered = workspace(
        ProjectCommands {
            test: vec![CommandSpec::new("cargo", ["test", "--workspace"])],
            format: vec![CommandSpec::new("cargo", ["fmt", "--all", "--check"])],
            typecheck: vec![CommandSpec::new("cargo", ["check", "--workspace"])],
            lint: vec![CommandSpec::new("cargo", ["clippy", "--workspace"])],
            build: vec![CommandSpec::new("cargo", ["build", "--workspace"])],
        },
        true,
    );
    discovered.current_directory = root.to_path_buf();
    discovered.repository_root = Some(root.to_path_buf());

    let plan =
        VerificationPlan::for_changes(&discovered, &[PathBuf::from("crates/widget/src/lib.rs")]);

    let categories = plan
        .steps
        .iter()
        .map(|step| step.category)
        .collect::<Vec<_>>();
    assert_eq!(
        categories,
        vec![
            VerificationCategory::Formatter,
            VerificationCategory::TargetedTest,
            VerificationCategory::Typecheck,
            VerificationCategory::Lint,
            VerificationCategory::Build,
            VerificationCategory::GitDiff,
        ]
    );
    let targeted = plan
        .steps
        .iter()
        .find(|step| step.category == VerificationCategory::TargetedTest)
        .unwrap();
    assert_eq!(targeted.command.program, "cargo");
    assert_eq!(targeted.command.args, ["test", "--package", "widget"]);
    let formatter = plan
        .steps
        .iter()
        .find(|step| step.category == VerificationCategory::Formatter)
        .unwrap();
    assert_eq!(
        formatter.command.args,
        ["fmt", "--package", "widget", "--check"]
    );
    let typecheck = plan
        .steps
        .iter()
        .find(|step| step.category == VerificationCategory::Typecheck)
        .unwrap();
    assert_eq!(typecheck.command.args, ["check", "--package", "widget"]);
}

#[test]
fn repository_verification_instructions_fill_in_discovered_commands() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    std::fs::create_dir_all(root.join(".agent")).unwrap();
    let mut discovered = workspace(ProjectCommands::default(), false);
    discovered.current_directory = root.to_path_buf();
    discovered.repository_root = Some(root.to_path_buf());
    discovered.instructions.push(harness_core::InstructionFile {
        path: root.join("AGENTS.md"),
        kind: harness_core::InstructionKind::Agents,
        precedence: 0,
        content: "# Project guidance\n\n## Verification\n- lint: `pnpm lint`\n- targeted-test: `pnpm test -- --run {changed_files}`\n".to_owned(),
    });

    let all = VerificationPlan::all(&discovered);
    let changed =
        VerificationPlanner::after_changes(&all, root, &[PathBuf::from("src/widget.test.ts")]);

    let lint = changed
        .steps
        .iter()
        .find(|step| step.category == VerificationCategory::Lint)
        .unwrap();
    assert_eq!(lint.command.program, "pnpm");
    assert_eq!(lint.command.args, ["lint"]);
    let test = changed
        .steps
        .iter()
        .find(|step| step.category == VerificationCategory::TargetedTest)
        .unwrap();
    assert_eq!(
        test.command.args,
        ["test", "--", "--run", "src/widget.test.ts"]
    );
    assert_eq!(test.source, "AGENTS targeted-test");
}

#[test]
fn explicit_project_config_commands_override_agent_instructions() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    std::fs::create_dir_all(root.join(".agent")).unwrap();
    let config_path = root.join(".agent/config.toml");
    std::fs::write(&config_path, "[commands]\nlint = [\"configured-lint\"]\n").unwrap();
    let mut discovered = workspace(
        ProjectCommands {
            lint: vec![CommandSpec::new("inferred-lint", Vec::<String>::new())],
            ..ProjectCommands::default()
        },
        false,
    );
    discovered.current_directory = root.to_path_buf();
    discovered.repository_root = Some(root.to_path_buf());
    discovered.configuration.source = Some(config_path);
    discovered.configuration.commands.lint =
        vec![CommandSpec::new("configured-lint", Vec::<String>::new())];
    discovered.instructions.push(harness_core::InstructionFile {
        path: root.join("AGENTS.md"),
        kind: harness_core::InstructionKind::Agents,
        precedence: 0,
        content: "## Verification\n- lint: `agent-lint`\n".to_owned(),
    });

    let plan = VerificationPlan::all(&discovered);

    let lint = plan
        .steps
        .iter()
        .find(|step| step.category == VerificationCategory::Lint)
        .unwrap();
    assert_eq!(lint.command.program, "configured-lint");
}

#[test]
fn targets_javascript_python_and_go_tests_to_changed_workspaces() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    std::fs::create_dir_all(root.join("packages/ui/src")).unwrap();
    std::fs::write(
        root.join("packages/ui/package.json"),
        r#"{"scripts":{"test":"vitest run"}}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("packages/ui/src/widget.ts"),
        "export const widget = 1;\n",
    )
    .unwrap();
    std::fs::write(
        root.join("packages/ui/src/widget.test.ts"),
        "test('widget', () => {});\n",
    )
    .unwrap();
    let mut discovered = workspace(
        ProjectCommands {
            test: vec![CommandSpec::new("pnpm", ["run", "test"])],
            ..ProjectCommands::default()
        },
        false,
    );
    discovered.current_directory = root.to_path_buf();
    discovered.repository_root = Some(root.to_path_buf());
    let javascript =
        VerificationPlan::for_changes(&discovered, &[PathBuf::from("packages/ui/src/widget.ts")]);
    let js_test = javascript
        .steps
        .iter()
        .find(|step| step.category == VerificationCategory::TargetedTest)
        .unwrap();
    assert_eq!(
        js_test.command.args,
        [
            "--dir",
            "packages/ui",
            "run",
            "test",
            "--",
            "src/widget.test.ts"
        ]
    );

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(root.join("src/service.py"), "def service(): pass\n").unwrap();
    std::fs::write(
        root.join("tests/test_service.py"),
        "def test_service(): pass\n",
    )
    .unwrap();
    let mut python_workspace = discovered.clone();
    python_workspace.configuration.commands.test =
        vec![CommandSpec::new("pytest", Vec::<String>::new())];
    let python =
        VerificationPlan::for_changes(&python_workspace, &[PathBuf::from("src/service.py")]);
    assert_eq!(
        python
            .steps
            .iter()
            .find(|step| step.category == VerificationCategory::TargetedTest)
            .unwrap()
            .command
            .args,
        ["tests/test_service.py"]
    );

    std::fs::create_dir_all(root.join("pkg")).unwrap();
    std::fs::write(root.join("go.mod"), "module example.invalid\n").unwrap();
    std::fs::write(root.join("pkg/service.go"), "package pkg\n").unwrap();
    std::fs::write(root.join("pkg/service_test.go"), "package pkg\n").unwrap();
    let mut go_workspace = discovered;
    go_workspace.configuration.commands.test = vec![CommandSpec::new("go", ["test", "./..."])];
    let go = VerificationPlan::for_changes(&go_workspace, &[PathBuf::from("pkg/service.go")]);
    assert_eq!(
        go.steps
            .iter()
            .find(|step| step.category == VerificationCategory::TargetedTest)
            .unwrap()
            .command
            .args,
        ["test", "./pkg"]
    );
}

#[test]
fn runs_success_and_failure_with_structured_reports() {
    let temporary = tempdir().unwrap();
    let success = shell("echo verified");
    let failure = shell("echo failure & exit /B 7");
    let request = VerificationRequest {
        working_directory: temporary.path().to_path_buf(),
        plan: harness_verification::VerificationPlan {
            steps: vec![
                harness_verification::VerificationStep {
                    category: VerificationCategory::Build,
                    command: success,
                    source: "test".to_owned(),
                },
                harness_verification::VerificationStep {
                    category: VerificationCategory::Lint,
                    command: failure,
                    source: "test".to_owned(),
                },
            ],
        },
        changed_files: Vec::new(),
        max_output_bytes: 1024,
    };
    let verifier = CommandVerifier::new(Arc::new(LocalProcessRunner));

    let reports = verifier.verify(&request).unwrap();

    assert!(reports[0].passed);
    assert_eq!(reports[0].exit_code, Some(0));
    assert!(reports[0].output.contains("verified"));
    assert!(!reports[1].passed);
    assert_eq!(reports[1].exit_code, Some(7));
    assert_eq!(reports[1].diagnostics, vec!["failure"]);
}

#[test]
fn bounds_large_verification_output() {
    let temporary = tempdir().unwrap();
    let large = if cfg!(windows) {
        "for /L %i in (1,1,20000) do @echo 123456789012345678901234567890"
    } else {
        "yes 123456789012345678901234567890 | head -c 2000000"
    };
    let request = VerificationRequest {
        working_directory: temporary.path().to_path_buf(),
        plan: harness_verification::VerificationPlan {
            steps: vec![harness_verification::VerificationStep {
                category: VerificationCategory::GeneralTest,
                command: shell(large),
                source: "test".to_owned(),
            }],
        },
        changed_files: Vec::new(),
        max_output_bytes: 128,
    };
    let verifier = CommandVerifier::new(Arc::new(LocalProcessRunner)).with_output_limit(128);

    let report = verifier.verify(&request).unwrap().remove(0);

    assert!(report.output.len() <= 128);
    assert!(report.output.contains("output truncated"));
    assert!(report.passed);
}

#[test]
fn structured_failures_include_attribution_and_relevant_output() {
    let temporary = tempdir().unwrap();
    let request = VerificationRequest {
        working_directory: temporary.path().to_path_buf(),
        plan: VerificationPlan {
            steps: vec![harness_verification::VerificationStep {
                category: VerificationCategory::TargetedTest,
                command: shell("echo error src/lib.rs:2 assertion failed & exit /B 7"),
                source: "fixture".to_owned(),
            }],
        },
        changed_files: vec![temporary.path().join("src/lib.rs")],
        max_output_bytes: 1024,
    };
    let verifier = CommandVerifier::new(Arc::new(LocalProcessRunner));

    let report = verifier.verify(&request).unwrap().remove(0);
    let failure = report.failure.unwrap();

    assert_eq!(failure.category, VerificationCategory::TargetedTest);
    assert!(failure.command.contains("echo"));
    assert_eq!(failure.exit_code, Some(7));
    assert!(failure
        .diagnostics
        .iter()
        .any(|line| line.contains("error")));
    assert!(failure.relevant_output.contains("assertion failed"));
    assert_eq!(
        failure.affected_files,
        vec![temporary.path().join("src/lib.rs")]
    );
    assert_eq!(
        failure.origin,
        harness_verification::FailureOrigin::Introduced
    );
}

#[test]
fn lint_and_compile_failures_retain_distinct_categories() {
    let temporary = tempdir().unwrap();
    let verifier = CommandVerifier::new(Arc::new(LocalProcessRunner));
    for (category, name) in [
        (VerificationCategory::Lint, "lint"),
        (VerificationCategory::Build, "compile"),
    ] {
        let report = verifier
            .verify(&VerificationRequest {
                working_directory: temporary.path().to_path_buf(),
                plan: VerificationPlan {
                    steps: vec![harness_verification::VerificationStep {
                        category,
                        command: shell(&format!("echo {name} error & exit /B 2")),
                        source: "fixture".to_owned(),
                    }],
                },
                changed_files: Vec::new(),
                max_output_bytes: 1024,
            })
            .unwrap()
            .remove(0);

        assert!(!report.passed);
        assert_eq!(report.failure.unwrap().category, category);
    }
}

#[test]
fn unrelated_and_stale_test_failures_are_not_claimed_as_patch_regressions() {
    let temporary = tempdir().unwrap();
    let verifier = CommandVerifier::new(Arc::new(LocalProcessRunner));
    let run = |output: &str| {
        let command = format!("echo {output} & exit /B 1");
        verifier
            .verify(&VerificationRequest {
                working_directory: temporary.path().to_path_buf(),
                plan: VerificationPlan {
                    steps: vec![harness_verification::VerificationStep {
                        category: VerificationCategory::GeneralTest,
                        command: shell(&command),
                        source: "fixture".to_owned(),
                    }],
                },
                changed_files: vec![PathBuf::from("src/new.rs")],
                max_output_bytes: 1024,
            })
            .unwrap()
            .remove(0)
            .failure
            .unwrap()
    };

    assert_eq!(
        run("error tests/legacy.rs assertion failed").origin,
        harness_verification::FailureOrigin::Unrelated
    );
    assert_eq!(
        run("test failed: expected old snapshot").origin,
        harness_verification::FailureOrigin::Unknown
    );
}

#[test]
fn command_verifier_stops_after_the_first_failure() {
    let temporary = tempdir().unwrap();
    let after = temporary.path().join("after.txt");
    let create_after = if cfg!(windows) {
        format!("echo ran > {}", after.display())
    } else {
        format!("touch '{}'", after.display())
    };
    let request = VerificationRequest {
        working_directory: temporary.path().to_path_buf(),
        plan: VerificationPlan {
            steps: vec![
                harness_verification::VerificationStep {
                    category: VerificationCategory::Formatter,
                    command: shell("echo formatting passed"),
                    source: "fixture".to_owned(),
                },
                harness_verification::VerificationStep {
                    category: VerificationCategory::Lint,
                    command: shell("echo lint failed & exit /B 1"),
                    source: "fixture".to_owned(),
                },
                harness_verification::VerificationStep {
                    category: VerificationCategory::GeneralTest,
                    command: shell(&create_after),
                    source: "fixture".to_owned(),
                },
            ],
        },
        changed_files: Vec::new(),
        max_output_bytes: 1024,
    };

    let reports = CommandVerifier::new(Arc::new(LocalProcessRunner))
        .verify(&request)
        .unwrap();

    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].category, VerificationCategory::Formatter);
    assert!(!reports[1].passed);
    assert!(
        !after.exists(),
        "checks after the first failure must not run"
    );
}
