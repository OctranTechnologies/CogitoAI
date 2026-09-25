use std::path::PathBuf;
use std::sync::Arc;

use harness_core::{
    CommandSpec, GitDescription, Manifest, MonorepoDescription, PackageManager, ProjectCommands,
    WorkingTreeState, WorkspaceConfiguration, WorkspaceDescription,
};
use harness_tools::LocalProcessRunner;
use harness_verification::{
    CommandVerifier, VerificationCategory, VerificationPlan, VerificationRequest, Verifier,
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
        .any(|step| step.category == VerificationCategory::TargetedTest));
    assert!(!targeted
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
        max_output_bytes: 128,
    };
    let verifier = CommandVerifier::new(Arc::new(LocalProcessRunner)).with_output_limit(128);

    let report = verifier.verify(&request).unwrap().remove(0);

    assert!(report.output.len() <= 128);
    assert!(report.passed);
}
