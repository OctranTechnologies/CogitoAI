use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use harness_core::{CommandSpec, Error, WorkspaceDescription};
use harness_tools::{CancellationToken, ProcessEvent, ProcessRequest, ProcessRunner};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum VerificationCategory {
    Formatter,
    Lint,
    Typecheck,
    Build,
    TargetedTest,
    GeneralTest,
    GitDiff,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationStep {
    pub category: VerificationCategory,
    pub command: CommandSpec,
    pub source: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationPlan {
    pub steps: Vec<VerificationStep>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationRequest {
    pub working_directory: PathBuf,
    pub plan: VerificationPlan,
    pub max_output_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationReport {
    pub category: VerificationCategory,
    pub command: String,
    pub duration_ms: u64,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub output: String,
    pub diagnostics: Vec<String>,
}

pub trait Verifier: Send + Sync {
    fn verify(&self, request: &VerificationRequest) -> Result<Vec<VerificationReport>, Error>;
}

#[derive(Clone)]
pub struct CommandVerifier {
    runner: Arc<dyn ProcessRunner>,
    cancellation: CancellationToken,
    max_output_bytes: usize,
}

impl CommandVerifier {
    pub fn new(runner: Arc<dyn ProcessRunner>) -> Self {
        Self {
            runner,
            cancellation: CancellationToken::new(),
            max_output_bytes: 64 * 1024,
        }
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn with_output_limit(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }
}

impl Verifier for CommandVerifier {
    fn verify(&self, request: &VerificationRequest) -> Result<Vec<VerificationReport>, Error> {
        let mut reports = Vec::new();
        for step in &request.plan.steps {
            let started_at = Instant::now();
            let process_request = ProcessRequest {
                program: step.command.program.clone(),
                args: step.command.args.clone(),
                working_directory: request.working_directory.clone(),
                timeout: Duration::from_secs(600),
                max_output_bytes: self.max_output_bytes.min(request.max_output_bytes),
            };
            let mut stdout = String::new();
            let mut stderr = String::new();
            let result = self
                .runner
                .execute(
                    process_request,
                    &self.cancellation,
                    &mut |event| match event {
                        ProcessEvent::Stdout { chunk } => {
                            append_bounded(&mut stdout, &chunk, self.max_output_bytes);
                            Ok(())
                        }
                        ProcessEvent::Stderr { chunk } => {
                            append_bounded(&mut stderr, &chunk, self.max_output_bytes);
                            Ok(())
                        }
                        ProcessEvent::Started { .. } | ProcessEvent::Exited { .. } => Ok(()),
                    },
                )
                .map_err(|error| Error::Verification {
                    message: error.to_string(),
                })?;
            let output = combine_output(&stdout, &stderr, self.max_output_bytes);
            reports.push(VerificationReport {
                category: step.category,
                command: format_command(&step.command),
                duration_ms: started_at
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
                passed: result.success,
                exit_code: result.exit_code,
                diagnostics: diagnostics(&output),
                output,
            });
        }
        Ok(reports)
    }
}

impl VerificationPlan {
    pub fn all(workspace: &WorkspaceDescription) -> Self {
        Self::from_workspace(workspace, &[], true)
    }

    pub fn for_changes(workspace: &WorkspaceDescription, changed_paths: &[PathBuf]) -> Self {
        Self::from_workspace(workspace, changed_paths, false)
    }

    pub fn targeted(&self) -> Self {
        Self {
            steps: self
                .steps
                .iter()
                .filter_map(|step| match step.category {
                    VerificationCategory::GeneralTest => Some(VerificationStep {
                        category: VerificationCategory::TargetedTest,
                        command: step.command.clone(),
                        source: "targeted-after-change".to_owned(),
                    }),
                    VerificationCategory::TargetedTest => None,
                    _ => Some(step.clone()),
                })
                .collect(),
        }
    }

    fn from_workspace(
        workspace: &WorkspaceDescription,
        changed_paths: &[PathBuf],
        include_general_tests: bool,
    ) -> Self {
        let commands = &workspace.configuration.commands;
        let mut steps = Vec::new();
        add_steps(
            &mut steps,
            VerificationCategory::Formatter,
            "detected",
            &commands.format,
        );
        add_steps(
            &mut steps,
            VerificationCategory::Lint,
            "detected",
            &commands.lint,
        );
        add_steps(
            &mut steps,
            VerificationCategory::Typecheck,
            "detected",
            &commands.typecheck,
        );
        add_steps(
            &mut steps,
            VerificationCategory::Build,
            "detected",
            &commands.build,
        );
        if !changed_paths.is_empty() {
            add_steps(
                &mut steps,
                VerificationCategory::TargetedTest,
                "targeted-after-change",
                &commands.test,
            );
        } else if include_general_tests {
            add_steps(
                &mut steps,
                VerificationCategory::GeneralTest,
                "detected",
                &commands.test,
            );
        }
        if workspace.git.available {
            steps.push(VerificationStep {
                category: VerificationCategory::GitDiff,
                command: CommandSpec::new("git", ["diff", "--no-ext-diff", "--no-color"]),
                source: "final-git-diff".to_owned(),
            });
        }
        Self { steps }
    }
}

fn add_steps(
    steps: &mut Vec<VerificationStep>,
    category: VerificationCategory,
    source: &str,
    commands: &[CommandSpec],
) {
    steps.extend(commands.iter().map(|command| VerificationStep {
        category,
        command: command.clone(),
        source: source.to_owned(),
    }));
}

fn append_bounded(target: &mut String, chunk: &str, limit: usize) {
    if target.len() >= limit {
        return;
    }
    let remaining = limit - target.len();
    if chunk.len() <= remaining {
        target.push_str(chunk);
    } else {
        let mut end = remaining;
        while !chunk.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        target.push_str(&chunk[..end]);
    }
}

fn combine_output(stdout: &str, stderr: &str, limit: usize) -> String {
    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str("stdout:\n");
        combined.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str("stderr:\n");
        combined.push_str(stderr);
    }
    if combined.len() > limit {
        let mut end = limit;
        while !combined.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        combined.truncate(end);
    }
    combined
}

fn diagnostics(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("error")
                || lower.contains("warning")
                || lower.contains("failed")
                || lower.contains("failure")
        })
        .take(20)
        .map(|line| line.trim().to_owned())
        .collect()
}

fn format_command(command: &CommandSpec) -> String {
    std::iter::once(&command.program)
        .chain(command.args.iter())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}
