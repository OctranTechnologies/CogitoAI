use std::path::{Path, PathBuf};
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
    /// Files changed by the current task, used only to attribute diagnostics.
    #[serde(default)]
    pub changed_files: Vec<PathBuf>,
    pub max_output_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureOrigin {
    Introduced,
    Unrelated,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationFailure {
    pub category: VerificationCategory,
    pub command: String,
    pub exit_code: Option<i32>,
    pub diagnostics: Vec<String>,
    pub relevant_output: String,
    pub affected_files: Vec<PathBuf>,
    pub origin: FailureOrigin,
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
    #[serde(default)]
    pub failure: Option<VerificationFailure>,
}

pub trait Verifier: Send + Sync {
    fn verify(&self, request: &VerificationRequest) -> Result<Vec<VerificationReport>, Error>;

    fn verify_cancellable(
        &self,
        request: &VerificationRequest,
        _cancellation: &CancellationToken,
    ) -> Result<Vec<VerificationReport>, Error> {
        self.verify(request)
    }
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
        self.verify_with_cancellation(request, &self.cancellation)
    }

    fn verify_cancellable(
        &self,
        request: &VerificationRequest,
        cancellation: &CancellationToken,
    ) -> Result<Vec<VerificationReport>, Error> {
        self.verify_with_cancellation(request, cancellation)
    }
}

impl CommandVerifier {
    fn verify_with_cancellation(
        &self,
        request: &VerificationRequest,
        cancellation: &CancellationToken,
    ) -> Result<Vec<VerificationReport>, Error> {
        let mut reports = Vec::new();
        for step in &request.plan.steps {
            if cancellation.is_cancelled() {
                return Err(Error::Verification {
                    message: "verification cancelled".to_owned(),
                });
            }
            let started_at = Instant::now();
            let process_output_limit = self.max_output_bytes.min(request.max_output_bytes);
            let process_request = ProcessRequest {
                program: step.command.program.clone(),
                args: step.command.args.clone(),
                working_directory: request.working_directory.clone(),
                timeout: Duration::from_secs(600),
                max_output_bytes: process_output_limit,
            };
            let mut stdout = String::new();
            let mut stderr = String::new();
            let mut output_truncated = false;
            let result =
                self.runner
                    .execute(process_request, cancellation, &mut |event| match event {
                        ProcessEvent::Stdout { chunk } => {
                            output_truncated |=
                                append_bounded(&mut stdout, &chunk, self.max_output_bytes);
                            Ok(())
                        }
                        ProcessEvent::Stderr { chunk } => {
                            output_truncated |=
                                append_bounded(&mut stderr, &chunk, self.max_output_bytes);
                            Ok(())
                        }
                        ProcessEvent::Started { .. } | ProcessEvent::Exited { .. } => Ok(()),
                    });
            if cancellation.is_cancelled() {
                return Err(Error::Verification {
                    message: "verification cancelled".to_owned(),
                });
            }
            let (success, exit_code, output) = match result {
                Ok(result) => {
                    let mut output =
                        combine_output(&stdout, &stderr, process_output_limit, output_truncated);
                    let timed_out = result.timed_out;
                    let cancelled = result.cancelled;
                    if timed_out || cancelled {
                        let status = if timed_out {
                            "verification command timed out"
                        } else {
                            "verification command was cancelled"
                        };
                        output =
                            bounded_string(&format!("{output}\n{status}"), process_output_limit);
                    }
                    (
                        result.success && !timed_out && !cancelled,
                        result.exit_code,
                        output,
                    )
                }
                Err(error) => (
                    false,
                    None,
                    bounded_string(
                        &error.to_string(),
                        self.max_output_bytes.min(request.max_output_bytes),
                    ),
                ),
            };
            let command = format_command(&step.command);
            let diagnostic_lines = diagnostics(&output);
            let failure = (!success).then(|| VerificationFailure {
                category: step.category,
                command: command.clone(),
                exit_code,
                relevant_output: relevant_output(&output, &diagnostic_lines, 8 * 1024),
                affected_files: affected_files(
                    &output,
                    &request.working_directory,
                    &request.changed_files,
                ),
                origin: failure_origin(&output, &request.working_directory, &request.changed_files),
                diagnostics: diagnostic_lines.clone(),
            });
            reports.push(VerificationReport {
                category: step.category,
                command,
                duration_ms: started_at
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
                passed: success,
                exit_code,
                diagnostics: diagnostic_lines,
                output,
                failure,
            });
            // Give the agent the earliest useful failure so it can repair that
            // issue before spending time on checks that depend on it.
            if !success {
                break;
            }
        }
        Ok(reports)
    }
}

impl VerificationPlan {
    pub fn all(workspace: &WorkspaceDescription) -> Self {
        VerificationPlanner::all(workspace)
    }

    pub fn for_changes(workspace: &WorkspaceDescription, changed_paths: &[PathBuf]) -> Self {
        VerificationPlanner::for_changes(workspace, changed_paths)
    }

    #[deprecated(note = "Use VerificationPlanner::after_changes with the changed paths")]
    pub fn targeted(&self) -> Self {
        self.clone()
    }
}

/// Builds a low-cost-first validation plan from workspace discovery and the
/// repository's explicit verification instructions. Test commands are narrowed
/// to the edited package/file when the framework makes that safe to infer.
pub struct VerificationPlanner;

impl VerificationPlanner {
    pub fn all(workspace: &WorkspaceDescription) -> VerificationPlan {
        let instructions = validation_instructions(workspace);
        let configured = configured_categories(workspace);
        let commands = &workspace.configuration.commands;
        let mut steps = Vec::new();
        for (category, key, discovered) in [
            (VerificationCategory::Formatter, "format", &commands.format),
            (
                VerificationCategory::Typecheck,
                "typecheck",
                &commands.typecheck,
            ),
            (VerificationCategory::Lint, "lint", &commands.lint),
            (VerificationCategory::Build, "build", &commands.build),
        ] {
            let selected = select_commands(key, discovered, &instructions, &configured);
            add_steps(&mut steps, category, "workspace-discovery", &selected);
        }
        if !instructions.targeted_tests.is_empty() && !configured.contains("test") {
            add_steps(
                &mut steps,
                VerificationCategory::TargetedTest,
                "AGENTS targeted-test",
                &instructions.targeted_tests,
            );
        } else {
            let tests = select_commands("test", &commands.test, &instructions, &configured);
            add_steps(
                &mut steps,
                VerificationCategory::GeneralTest,
                "workspace-discovery",
                &tests,
            );
        }
        if workspace.git.available && workspace.repository_root.is_some() {
            steps.push(VerificationStep {
                category: VerificationCategory::GitDiff,
                command: CommandSpec::new("git", ["diff", "--no-ext-diff", "--no-color"]),
                source: "final-git-diff".to_owned(),
            });
        }
        VerificationPlan { steps }
    }

    pub fn for_changes(
        workspace: &WorkspaceDescription,
        changed_paths: &[PathBuf],
    ) -> VerificationPlan {
        let root = workspace
            .repository_root
            .as_deref()
            .unwrap_or(&workspace.current_directory);
        Self::after_changes(&Self::all(workspace), root, changed_paths)
    }

    pub fn after_changes(
        plan: &VerificationPlan,
        working_directory: &Path,
        changed_paths: &[PathBuf],
    ) -> VerificationPlan {
        let mut steps = Vec::new();
        for step in &plan.steps {
            if step.category == VerificationCategory::TargetedTest
                && step.source == "AGENTS targeted-test"
            {
                steps.push(VerificationStep {
                    category: VerificationCategory::TargetedTest,
                    command: substitute_changed_files(
                        &step.command,
                        working_directory,
                        changed_paths,
                    ),
                    source: step.source.clone(),
                });
            } else if matches!(
                step.category,
                VerificationCategory::GeneralTest | VerificationCategory::TargetedTest
            ) {
                let (category, command, source) =
                    target_test_command(&step.command, working_directory, changed_paths)
                        .map(|command| {
                            (
                                VerificationCategory::TargetedTest,
                                command,
                                "targeted-by-changed-files",
                            )
                        })
                        .unwrap_or((
                            VerificationCategory::GeneralTest,
                            step.command.clone(),
                            "full-test-fallback",
                        ));
                steps.push(VerificationStep {
                    category,
                    command,
                    source: source.to_owned(),
                });
            } else if let Some(command) =
                narrow_cargo_command(&step.command, working_directory, changed_paths)
            {
                steps.push(VerificationStep {
                    category: step.category,
                    command,
                    source: "targeted-by-changed-package".to_owned(),
                });
            } else {
                steps.push(step.clone());
            }
        }
        if !steps
            .iter()
            .any(|step| step.category == VerificationCategory::GitDiff)
            && git_root_from(working_directory).is_some()
        {
            steps.push(VerificationStep {
                category: VerificationCategory::GitDiff,
                command: CommandSpec::new("git", ["diff", "--no-ext-diff", "--no-color"]),
                source: "required-final-diff-inspection".to_owned(),
            });
        }
        steps.sort_by_key(|step| category_priority(step.category));
        VerificationPlan { steps }
    }
}

fn git_root_from(start: &Path) -> Option<PathBuf> {
    let mut directory = start.to_path_buf();
    loop {
        if directory.join(".git").exists() {
            return Some(directory);
        }
        if !directory.pop() {
            return None;
        }
    }
}

fn narrow_cargo_command(
    command: &CommandSpec,
    root: &Path,
    changed_paths: &[PathBuf],
) -> Option<CommandSpec> {
    let program = command
        .program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&command.program)
        .trim_end_matches(".exe");
    if !program.eq_ignore_ascii_case("cargo") || changed_paths.is_empty() {
        return None;
    }
    let (package, _) = cargo_target(root, changed_paths)?;
    let (subcommand, flags) = command.args.split_first()?;
    let mut args = vec![subcommand.clone(), "--package".to_owned(), package];
    args.extend(
        flags
            .iter()
            .filter(|argument| argument.as_str() != "--workspace" && argument.as_str() != "--all")
            .cloned(),
    );
    Some(CommandSpec::new(&command.program, args))
}

fn substitute_changed_files(
    command: &CommandSpec,
    root: &Path,
    changed_paths: &[PathBuf],
) -> CommandSpec {
    let values = changed_paths
        .iter()
        .map(|path| {
            path.strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    let mut args = Vec::new();
    let mut replaced = false;
    for argument in &command.args {
        if argument.contains("{changed_files}") {
            args.extend(values.iter().cloned());
            replaced = true;
        } else if argument.contains("{changed_file}") {
            if let Some(value) = values.first() {
                args.push(value.clone());
            }
            replaced = true;
        } else {
            args.push(argument.clone());
        }
    }
    if !replaced && !values.is_empty() {
        if !args.iter().any(|argument| argument == "--") {
            args.push("--".to_owned());
        }
        args.extend(values);
    }
    CommandSpec::new(&command.program, args)
}

fn category_priority(category: VerificationCategory) -> u8 {
    match category {
        VerificationCategory::Formatter => 0,
        VerificationCategory::TargetedTest => 1,
        VerificationCategory::Typecheck => 2,
        VerificationCategory::Lint => 3,
        VerificationCategory::Build => 4,
        VerificationCategory::GeneralTest => 5,
        VerificationCategory::GitDiff => 6,
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

#[derive(Default)]
struct ValidationInstructions {
    commands: std::collections::HashMap<&'static str, Vec<CommandSpec>>,
    targeted_tests: Vec<CommandSpec>,
}

fn validation_instructions(workspace: &WorkspaceDescription) -> ValidationInstructions {
    let mut result = ValidationInstructions::default();
    for instruction in &workspace.instructions {
        let mut in_validation_section = false;
        for line in instruction.content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                let heading = trimmed.trim_start_matches('#').trim().to_ascii_lowercase();
                in_validation_section = ["verification", "validation", "testing"]
                    .iter()
                    .any(|name| heading == *name || heading.starts_with(&format!("{name} ")));
                continue;
            }
            if !in_validation_section {
                continue;
            }
            let line = trimmed.trim_start_matches(['-', '*', '+', ' ']).trim();
            let Some((label, value)) = line.split_once(':') else {
                continue;
            };
            let label = label.trim().to_ascii_lowercase().replace(' ', "-");
            let command_text = value.trim().trim_matches('`').trim();
            let Some(command) = parse_command(command_text) else {
                continue;
            };
            match label.as_str() {
                "format" | "formatter" => {
                    result.commands.entry("format").or_default().push(command)
                }
                "lint" => result.commands.entry("lint").or_default().push(command),
                "typecheck" | "type-check" | "typechecker" => result
                    .commands
                    .entry("typecheck")
                    .or_default()
                    .push(command),
                "build" | "compile" => result.commands.entry("build").or_default().push(command),
                "test" | "tests" => result.commands.entry("test").or_default().push(command),
                "targeted-test" | "targeted-tests" => result.targeted_tests.push(command),
                _ => {}
            }
        }
    }
    result
}

fn configured_categories(workspace: &WorkspaceDescription) -> std::collections::HashSet<String> {
    let Some(path) = workspace.configuration.source.as_deref() else {
        return std::collections::HashSet::new();
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return std::collections::HashSet::new();
    };
    let Ok(value) = contents.parse::<toml::Value>() else {
        return std::collections::HashSet::new();
    };
    value
        .get("commands")
        .and_then(toml::Value::as_table)
        .map(|table| table.keys().cloned().collect())
        .unwrap_or_default()
}

fn select_commands(
    category: &str,
    discovered: &[CommandSpec],
    instructions: &ValidationInstructions,
    configured: &std::collections::HashSet<String>,
) -> Vec<CommandSpec> {
    if configured.contains(category) {
        return discovered.to_vec();
    }
    instructions
        .commands
        .get(category)
        .filter(|commands| !commands.is_empty())
        .cloned()
        .unwrap_or_else(|| discovered.to_vec())
}

fn parse_command(command: &str) -> Option<CommandSpec> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in command.chars() {
        if escaped {
            word.push(character);
            escaped = false;
        } else if character == '\\' && quote != Some('\'') {
            escaped = true;
        } else if let Some(current_quote) = quote {
            if character == current_quote {
                quote = None;
            } else {
                word.push(character);
            }
        } else if character == '\'' || character == '"' {
            quote = Some(character);
        } else if character.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(character);
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !word.is_empty() {
        words.push(word);
    }
    let (program, args) = words.split_first()?;
    (!program.is_empty()).then(|| CommandSpec::new(program, args.iter().cloned()))
}

fn target_test_command(
    command: &CommandSpec,
    root: &Path,
    changed_paths: &[PathBuf],
) -> Option<CommandSpec> {
    if changed_paths.is_empty() {
        return None;
    }
    let program = command
        .program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&command.program)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    let args = command
        .args
        .iter()
        .map(|arg| arg.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if program == "cargo" && args.iter().any(|arg| arg == "test") {
        if let Some((package, integration_test)) = cargo_target(root, changed_paths) {
            let mut target = vec!["test".to_owned(), "--package".to_owned(), package];
            if let Some(test) = integration_test {
                target.push("--test".to_owned());
                target.push(test);
            }
            return Some(CommandSpec::new("cargo", target));
        }
    }
    if program == "go" && args.iter().any(|arg| arg == "test") {
        if let Some(directory) = go_target(root, changed_paths) {
            return Some(CommandSpec::new("go", ["test", directory.as_str()]));
        }
    }
    if program == "pytest"
        || (program == "python" && args.windows(2).any(|pair| pair == ["-m", "pytest"]))
    {
        let test_paths = related_test_paths(root, changed_paths, &["py"]);
        if !test_paths.is_empty() {
            let program = if program == "pytest" {
                "pytest"
            } else {
                "python"
            };
            let mut target = command.args.clone();
            target.retain(|argument| argument != "-m" && argument != "pytest");
            if program == "python" {
                target.extend(["-m".to_owned(), "pytest".to_owned()]);
            }
            target.extend(test_paths);
            return Some(CommandSpec::new(program, target));
        }
    }
    if ["npm", "pnpm", "yarn", "bun"].contains(&program.as_str())
        && args.iter().any(|arg| arg == "test" || arg == "run")
    {
        if let Some(target) = node_package_test_command(&program, root, changed_paths) {
            return Some(target);
        }
        let test_paths = related_test_paths(
            root,
            changed_paths,
            &["js", "jsx", "ts", "tsx", "mjs", "cjs"],
        );
        if !test_paths.is_empty() {
            let mut target = command.args.clone();
            if !target.iter().any(|argument| argument == "--") {
                target.push("--".to_owned());
            }
            target.extend(test_paths);
            return Some(CommandSpec::new(&command.program, target));
        }
    }
    None
}

fn node_package_test_command(
    package_manager: &str,
    root: &Path,
    changed_paths: &[PathBuf],
) -> Option<CommandSpec> {
    let mut package_directory = None;
    for changed in changed_paths {
        let path = absolute_path(root, changed);
        let mut directory = path.parent()?.to_path_buf();
        let mut found = None;
        while directory.starts_with(root) {
            let package_file = directory.join("package.json");
            if let Ok(contents) = std::fs::read_to_string(package_file) {
                let value = serde_json::from_str::<serde_json::Value>(&contents).ok()?;
                if value
                    .get("scripts")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|scripts| scripts.get("test"))
                    .and_then(serde_json::Value::as_str)
                    .is_some()
                {
                    found = Some(directory.clone());
                    break;
                }
            }
            if !directory.pop() {
                break;
            }
        }
        let found = found?;
        if package_directory
            .as_ref()
            .is_some_and(|existing| existing != &found)
        {
            return None;
        }
        package_directory = Some(found);
    }
    let package_directory = package_directory?;
    let relative_package = package_directory
        .strip_prefix(root)
        .ok()?
        .to_string_lossy()
        .replace('\\', "/");
    let relative_package = if relative_package.is_empty() {
        ".".to_owned()
    } else {
        relative_package
    };
    let extensions = ["js", "jsx", "ts", "tsx", "mjs", "cjs"];
    let test_paths = related_test_paths(root, changed_paths, &extensions)
        .into_iter()
        .filter_map(|path| {
            let absolute = root.join(path);
            absolute
                .strip_prefix(&package_directory)
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        })
        .collect::<Vec<_>>();
    let mut args = match package_manager {
        "pnpm" => vec![
            "--dir".to_owned(),
            relative_package,
            "run".to_owned(),
            "test".to_owned(),
        ],
        "npm" => vec![
            "--prefix".to_owned(),
            relative_package,
            "run".to_owned(),
            "test".to_owned(),
        ],
        "yarn" => vec!["--cwd".to_owned(), relative_package, "test".to_owned()],
        "bun" => vec![
            "--cwd".to_owned(),
            relative_package,
            "run".to_owned(),
            "test".to_owned(),
        ],
        _ => return None,
    };
    if !test_paths.is_empty() {
        args.push("--".to_owned());
        args.extend(test_paths);
    }
    Some(CommandSpec::new(package_manager, args))
}

fn cargo_target(root: &Path, changed_paths: &[PathBuf]) -> Option<(String, Option<String>)> {
    let mut found: Option<(String, Option<String>)> = None;
    for changed in changed_paths {
        let path = absolute_path(root, changed);
        let mut directory = if path.is_dir() {
            path
        } else {
            path.parent()?.to_path_buf()
        };
        let mut package = None;
        while directory.starts_with(root) {
            let manifest = directory.join("Cargo.toml");
            if let Ok(contents) = std::fs::read_to_string(manifest) {
                if let Ok(value) = contents.parse::<toml::Value>() {
                    if let Some(name) = value
                        .get("package")
                        .and_then(|value| value.get("name"))
                        .and_then(toml::Value::as_str)
                    {
                        package = Some((name.to_owned(), changed_test_target(changed)));
                        break;
                    }
                }
            }
            if !directory.pop() {
                break;
            }
        }
        let candidate = package?;
        match &mut found {
            Some(existing) if existing.0 != candidate.0 => return None,
            Some(existing) if existing.1 != candidate.1 => existing.1 = None,
            Some(_) => {}
            None => found = Some(candidate),
        }
    }
    found
}

fn changed_test_target(path: &Path) -> Option<String> {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let parts = normalized.split('/').collect::<Vec<_>>();
    let tests_position = parts.iter().rposition(|part| *part == "tests")?;
    let relative = parts.get(tests_position + 1..)?;
    (relative.len() == 1)
        .then(|| {
            Path::new(relative[0])
                .file_stem()?
                .to_str()
                .map(str::to_owned)
        })
        .flatten()
}

fn go_target(root: &Path, changed_paths: &[PathBuf]) -> Option<String> {
    let mut directories = std::collections::BTreeSet::new();
    for changed in changed_paths {
        let path = absolute_path(root, changed);
        let directory = path.parent()?;
        let has_test = std::fs::read_dir(directory).ok()?.flatten().any(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("_test.go"))
        });
        if !has_test {
            return None;
        }
        let relative = directory
            .strip_prefix(root)
            .ok()?
            .to_string_lossy()
            .replace('\\', "/");
        directories.insert(if relative.is_empty() {
            "./".to_owned()
        } else {
            format!("./{relative}")
        });
    }
    (directories.len() == 1)
        .then(|| directories.into_iter().next())
        .flatten()
}

fn related_test_paths(root: &Path, changed_paths: &[PathBuf], extensions: &[&str]) -> Vec<String> {
    let mut tests = std::collections::BTreeSet::new();
    for changed in changed_paths {
        let absolute = absolute_path(root, changed);
        if is_test_path(&absolute) {
            if let Ok(relative) = absolute.strip_prefix(root) {
                tests.insert(relative.to_string_lossy().replace('\\', "/"));
            }
            continue;
        }
        let Some(stem) = absolute.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        for extension in extensions {
            let mut candidates = vec![
                absolute.with_file_name(format!("{stem}.test.{extension}")),
                absolute.with_file_name(format!("{stem}.spec.{extension}")),
                absolute.with_file_name(format!("{stem}_test.{extension}")),
                absolute.with_file_name(format!("test_{stem}.{extension}")),
                absolute
                    .parent()
                    .unwrap_or(root)
                    .join("tests")
                    .join(format!("{stem}.test.{extension}")),
                root.join("tests").join(format!("{stem}.test.{extension}")),
                root.join("__tests__")
                    .join(format!("{stem}.test.{extension}")),
            ];
            if *extension == "py" {
                candidates.push(root.join("tests").join(format!("test_{stem}.py")));
                candidates.push(
                    absolute
                        .parent()
                        .unwrap_or(root)
                        .join("tests")
                        .join(format!("test_{stem}.py")),
                );
            }
            for candidate in candidates {
                if candidate.is_file() {
                    if let Ok(relative) = candidate.strip_prefix(root) {
                        tests.insert(relative.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
        }
    }
    tests.into_iter().collect()
}

fn is_test_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name.starts_with("test_")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.go")
        || name.ends_with(".test.js")
        || name.ends_with(".test.jsx")
        || name.ends_with(".test.ts")
        || name.ends_with(".test.tsx")
        || name.ends_with(".spec.js")
        || name.ends_with(".spec.ts")
        || path
            .components()
            .any(|component| matches!(component.as_os_str().to_str(), Some("tests" | "__tests__")))
}

fn absolute_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn append_bounded(target: &mut String, chunk: &str, limit: usize) -> bool {
    if target.len() >= limit {
        return !chunk.is_empty();
    }
    let remaining = limit - target.len();
    if chunk.len() <= remaining {
        target.push_str(chunk);
        false
    } else {
        let mut end = remaining;
        while !chunk.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        target.push_str(&chunk[..end]);
        true
    }
}

fn bounded_string(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let suffix = "\n... output truncated ...";
    let body_limit = limit.saturating_sub(suffix.len());
    let mut end = body_limit;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{suffix}", &value[..end])
}

fn combine_output(stdout: &str, stderr: &str, limit: usize, was_truncated: bool) -> String {
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
    if combined.len() > limit || was_truncated {
        let suffix = "\n... output truncated ...";
        let body_limit = limit.saturating_sub(suffix.len());
        let mut end = body_limit.min(combined.len());
        while !combined.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        combined.truncate(end);
        if limit >= suffix.len() {
            combined.push_str(suffix);
        }
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
        .take(12)
        .map(|line| bounded_string(line.trim(), 512))
        .collect()
}

fn relevant_output(output: &str, diagnostics: &[String], max_bytes: usize) -> String {
    let lines = output.lines().collect::<Vec<_>>();
    let selected = if diagnostics.is_empty() {
        lines
            .iter()
            .rev()
            .take(40)
            .rev()
            .copied()
            .collect::<Vec<_>>()
    } else {
        let mut selected = std::collections::BTreeSet::new();
        for (index, line) in lines.iter().enumerate() {
            if diagnostics
                .iter()
                .any(|diagnostic| line.contains(diagnostic))
            {
                let start = index.saturating_sub(2);
                let end = (index + 2).min(lines.len().saturating_sub(1));
                selected.extend(start..=end);
            }
        }
        selected.into_iter().map(|index| lines[index]).collect()
    };
    bounded_string(&selected.join("\n"), max_bytes)
}

fn affected_files(output: &str, root: &Path, changed_files: &[PathBuf]) -> Vec<PathBuf> {
    let output = output.to_ascii_lowercase().replace('\\', "/");
    changed_files
        .iter()
        .filter(|path| {
            let path_from_root = if path.is_absolute() {
                path.strip_prefix(root).unwrap_or(path).to_path_buf()
            } else {
                path.to_path_buf()
            };
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            let normalized_relative = path_from_root
                .to_string_lossy()
                .to_ascii_lowercase()
                .replace('\\', "/");
            let normalized_absolute = absolute
                .to_string_lossy()
                .to_ascii_lowercase()
                .replace('\\', "/");
            (!normalized_relative.is_empty() && output.contains(&normalized_relative))
                || output.contains(&normalized_absolute)
        })
        .cloned()
        .collect()
}

fn failure_origin(output: &str, root: &Path, changed_files: &[PathBuf]) -> FailureOrigin {
    if !affected_files(output, root, changed_files).is_empty() {
        return FailureOrigin::Introduced;
    }
    let output = output.to_ascii_lowercase().replace('\\', "/");
    let mentions_file = output.split_whitespace().any(|token| {
        let token = token.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, '/' | '_' | '-' | '.')
        });
        [
            ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java", ".cs", ".json", ".toml",
            ".yaml", ".yml", ".md", ".html", ".css", ".sql",
        ]
        .iter()
        .any(|extension| {
            token
                .split(':')
                .next()
                .unwrap_or_default()
                .ends_with(extension)
        })
    });
    if mentions_file {
        FailureOrigin::Unrelated
    } else {
        FailureOrigin::Unknown
    }
}

fn format_command(command: &CommandSpec) -> String {
    std::iter::once(&command.program)
        .chain(command.args.iter())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}
