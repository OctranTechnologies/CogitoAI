use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use toml::from_str as parse_toml;

use crate::Error;

const MAX_SCAN_DEPTH: usize = 16;
const MAX_SCAN_FILES: usize = 20_000;

fn executable_available(program: &str) -> bool {
    let Some(path_value) = env::var_os("PATH") else {
        return false;
    };
    let search_paths = env::split_paths(&path_value).collect::<Vec<_>>();
    let extensions = if cfg!(windows) {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    } else {
        vec![String::new()]
    };
    executable_available_in(program, &search_paths, &extensions)
}

fn executable_available_in(program: &str, search_paths: &[PathBuf], extensions: &[String]) -> bool {
    let file_name = Path::new(program)
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new(program));
    let has_extension = Path::new(program).extension().is_some();
    search_paths.iter().any(|directory| {
        if has_extension {
            return directory.join(file_name).is_file();
        }
        extensions.iter().any(|extension| {
            directory
                .join(format!("{}{}", file_name.to_string_lossy(), extension))
                .is_file()
        })
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceDescription {
    pub current_directory: PathBuf,
    pub repository_root: Option<PathBuf>,
    pub git: GitDescription,
    pub languages: Vec<Language>,
    pub manifests: Vec<Manifest>,
    pub monorepo: MonorepoDescription,
    pub configuration: WorkspaceConfiguration,
    pub instructions: Vec<InstructionFile>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct GitDescription {
    pub available: bool,
    pub branch: Option<String>,
    pub working_tree: Option<WorkingTreeState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorkingTreeState {
    Clean,
    Dirty,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum Language {
    C,
    Cpp,
    CSharp,
    Go,
    Java,
    JavaScript,
    Kotlin,
    Python,
    Ruby,
    Rust,
    TypeScript,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum ManifestKind {
    Cargo,
    PackageJson,
    PackageLock,
    PnpmLock,
    YarnLock,
    BunLock,
    Python,
    Go,
    Java,
    Ruby,
    Composer,
    BuildSystem,
    Makefile,
    Justfile,
    Lint,
    Format,
    Typecheck,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub path: PathBuf,
    pub kind: ManifestKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MonorepoDescription {
    pub is_monorepo: bool,
    pub indicators: Vec<MonorepoIndicator>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum MonorepoIndicator {
    CargoWorkspace,
    NpmWorkspaces,
    PnpmWorkspace,
    GoWork,
    Lerna,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
    Cargo,
    Poetry,
    Pipenv,
    Go,
    Maven,
    Gradle,
    Composer,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProjectCommands {
    pub test: Vec<CommandSpec>,
    pub build: Vec<CommandSpec>,
    pub format: Vec<CommandSpec>,
    pub lint: Vec<CommandSpec>,
    pub typecheck: Vec<CommandSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceConfiguration {
    pub package_manager: Option<PackageManager>,
    pub commands: ProjectCommands,
    pub source: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum InstructionKind {
    GlobalAgents,
    Agents,
    Claude,
    Readme,
    Contributing,
    AgentInstructions,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub kind: InstructionKind,
    pub precedence: u8,
    pub content: String,
}

#[derive(Deserialize, Default)]
struct ProjectConfigFile {
    package_manager: Option<PackageManager>,
    #[serde(default)]
    commands: CommandOverrides,
}

#[derive(Deserialize, Default)]
struct CommandOverrides {
    test: Option<Vec<String>>,
    build: Option<Vec<String>>,
    format: Option<Vec<String>>,
    lint: Option<Vec<String>>,
    typecheck: Option<Vec<String>>,
}

impl CommandOverrides {
    fn apply(self, commands: &mut ProjectCommands) -> Result<(), Error> {
        apply_command_override(&mut commands.test, self.test)?;
        apply_command_override(&mut commands.build, self.build)?;
        apply_command_override(&mut commands.format, self.format)?;
        apply_command_override(&mut commands.lint, self.lint)?;
        apply_command_override(&mut commands.typecheck, self.typecheck)?;
        Ok(())
    }
}

fn apply_command_override(
    target: &mut Vec<CommandSpec>,
    values: Option<Vec<String>>,
) -> Result<(), Error> {
    let Some(values) = values else {
        return Ok(());
    };
    if values.is_empty() || values.first().map_or(true, String::is_empty) {
        return Err(Error::InvalidConfig {
            reason: "command overrides must contain a non-empty program".to_owned(),
        });
    }
    *target = vec![CommandSpec::new(
        values.first().expect("validated command program"),
        values.iter().skip(1).cloned(),
    )];
    Ok(())
}

pub fn discover_workspace(start: &Path) -> Result<WorkspaceDescription, Error> {
    let current_directory = fs::canonicalize(start)?;
    let git_available = git_available();
    let filesystem_root = find_git_root(&current_directory);
    let repository_root = filesystem_root.or_else(|| {
        git_available
            .then(|| run_git_output(&current_directory, &["rev-parse", "--show-toplevel"]))
            .flatten()
            .and_then(|value| canonical_path(Path::new(value.trim())))
    });
    let project_root = repository_root
        .clone()
        .or_else(|| find_project_root(&current_directory))
        .unwrap_or_else(|| current_directory.clone());
    let files = collect_files(&project_root);
    let languages = detect_languages(&files);
    let manifests = detect_manifests(&project_root, &files);
    let monorepo = detect_monorepo(&project_root, &manifests);
    let mut package_manager = detect_package_manager(&project_root, &files);
    let mut commands = infer_commands(
        &project_root,
        &files,
        &languages,
        &manifests,
        package_manager,
    );
    let configuration_path = project_root.join(".agent/config.toml");
    let configuration_file = load_project_config(&configuration_path)?;
    if let Some(configuration_file) = configuration_file {
        if configuration_file.package_manager.is_some() {
            package_manager = configuration_file.package_manager;
        }
        configuration_file.commands.apply(&mut commands)?;
    }
    let configuration = WorkspaceConfiguration {
        package_manager,
        commands,
        source: configuration_path.is_file().then_some(configuration_path),
    };
    let git = describe_git(git_available, repository_root.as_deref());
    let instructions = discover_instructions(&project_root, &current_directory)?;

    Ok(WorkspaceDescription {
        current_directory,
        repository_root,
        git,
        languages,
        manifests,
        monorepo,
        configuration,
        instructions,
    })
}

pub fn discover_current_workspace() -> Result<WorkspaceDescription, Error> {
    discover_workspace(&std::env::current_dir()?)
}

fn load_project_config(path: &Path) -> Result<Option<ProjectConfigFile>, Error> {
    if !path.is_file() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)?;
    parse_toml(&contents)
        .map(Some)
        .map_err(|error| Error::InvalidConfig {
            reason: format!("{}: {error}", path.display()),
        })
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return canonical_path(&current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let is_project_root = [
            ".agent",
            "Cargo.toml",
            "package.json",
            "pyproject.toml",
            "setup.py",
            "go.mod",
            "pom.xml",
            "build.gradle",
            "build.gradle.kts",
            "Gemfile",
            "composer.json",
        ]
        .into_iter()
        .any(|name| current.join(name).is_file() || current.join(name).is_dir());
        if is_project_root {
            return canonical_path(&current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn canonical_path(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

fn git_available() -> bool {
    let mut command = Command::new("git");
    command.arg("--version");
    configure_hidden_process(&mut command);
    command.output().is_ok_and(|output| output.status.success())
}

fn run_git_output(directory: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(directory).args(args);
    configure_hidden_process(&mut command);
    let output = command.output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn configure_hidden_process(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn describe_git(available: bool, repository_root: Option<&Path>) -> GitDescription {
    let Some(repository_root) = repository_root else {
        return GitDescription {
            available,
            branch: None,
            working_tree: None,
        };
    };
    let branch = run_git_output(repository_root, &["symbolic-ref", "--short", "-q", "HEAD"])
        .filter(|value| !value.is_empty());
    let working_tree = run_git_output(repository_root, &["status", "--porcelain"]).map(|output| {
        if output.is_empty() {
            WorkingTreeState::Clean
        } else {
            WorkingTreeState::Dirty
        }
    });
    GitDescription {
        available,
        branch,
        working_tree,
    }
}

fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > MAX_SCAN_DEPTH || files.len() >= MAX_SCAN_FILES {
            continue;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if !should_skip_directory(&path) {
                    pending.push((path, depth + 1));
                }
            } else if file_type.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn should_skip_directory(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            ".git"
                | ".hg"
                | ".svn"
                | "node_modules"
                | "target"
                | "dist"
                | "build"
                | "coverage"
                | "vendor"
                | ".venv"
                | "venv"
                | "__pycache__"
        )
    )
}

fn detect_languages(files: &[PathBuf]) -> Vec<Language> {
    let mut languages = BTreeSet::new();
    for path in files {
        let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        let language = match extension {
            "c" | "h" => Some(Language::C),
            "cc" | "cpp" | "cxx" | "hpp" => Some(Language::Cpp),
            "cs" => Some(Language::CSharp),
            "go" => Some(Language::Go),
            "java" => Some(Language::Java),
            "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
            "kt" | "kts" => Some(Language::Kotlin),
            "py" => Some(Language::Python),
            "rb" => Some(Language::Ruby),
            "rs" => Some(Language::Rust),
            "ts" | "tsx" | "mts" | "cts" => Some(Language::TypeScript),
            _ => None,
        };
        if let Some(language) = language {
            languages.insert(language);
        }
    }
    languages.into_iter().collect()
}

fn detect_manifests(root: &Path, files: &[PathBuf]) -> Vec<Manifest> {
    let mut manifests = Vec::new();
    for path in files {
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let kind = match name {
            "Cargo.toml" => Some(ManifestKind::Cargo),
            "package.json" => Some(ManifestKind::PackageJson),
            "package-lock.json" => Some(ManifestKind::PackageLock),
            "pnpm-lock.yaml" => Some(ManifestKind::PnpmLock),
            "yarn.lock" => Some(ManifestKind::YarnLock),
            "bun.lock" | "bun.lockb" => Some(ManifestKind::BunLock),
            "pyproject.toml" | "setup.py" | "setup.cfg" | "requirements.txt" => {
                Some(ManifestKind::Python)
            }
            "go.mod" => Some(ManifestKind::Go),
            "pom.xml" | "build.gradle" | "build.gradle.kts" => Some(ManifestKind::Java),
            "Gemfile" => Some(ManifestKind::Ruby),
            "composer.json" => Some(ManifestKind::Composer),
            "CMakeLists.txt" => Some(ManifestKind::BuildSystem),
            "Makefile" => Some(ManifestKind::Makefile),
            "justfile" | "Justfile" => Some(ManifestKind::Justfile),
            "eslint.config.js" | ".eslintrc" | ".eslintrc.json" | "ruff.toml" => {
                Some(ManifestKind::Lint)
            }
            "rustfmt.toml" | ".prettierrc" | ".prettierrc.json" => Some(ManifestKind::Format),
            "tsconfig.json" | "jsconfig.json" => Some(ManifestKind::Typecheck),
            _ => None,
        };
        if let Some(kind) = kind {
            manifests.push(Manifest {
                path: path.clone(),
                kind,
            });
        }
    }
    manifests.sort_by(|left, right| left.path.cmp(&right.path));
    let _ = root;
    manifests
}

fn detect_monorepo(root: &Path, manifests: &[Manifest]) -> MonorepoDescription {
    let mut indicators = BTreeSet::new();
    if let Some(cargo) = manifests.iter().find(|manifest| {
        manifest.kind == ManifestKind::Cargo && manifest.path.parent() == Some(root)
    }) {
        if fs::read_to_string(&cargo.path)
            .ok()
            .is_some_and(|contents| contents.contains("[workspace]"))
        {
            indicators.insert(MonorepoIndicator::CargoWorkspace);
        }
    }
    if let Some(package) = manifests.iter().find(|manifest| {
        manifest.kind == ManifestKind::PackageJson && manifest.path.parent() == Some(root)
    }) {
        if package_json_has_workspaces(&package.path) {
            indicators.insert(MonorepoIndicator::NpmWorkspaces);
        }
    }
    if root.join("pnpm-workspace.yaml").is_file() {
        indicators.insert(MonorepoIndicator::PnpmWorkspace);
    }
    if root.join("go.work").is_file() {
        indicators.insert(MonorepoIndicator::GoWork);
    }
    if root.join("lerna.json").is_file() {
        indicators.insert(MonorepoIndicator::Lerna);
    }
    let indicators = indicators.into_iter().collect::<Vec<_>>();
    MonorepoDescription {
        is_monorepo: !indicators.is_empty(),
        indicators,
    }
}

fn package_json_has_workspaces(path: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    value.get("workspaces").is_some()
}

fn detect_package_manager(root: &Path, files: &[PathBuf]) -> Option<PackageManager> {
    let node_package_manager = [
        ("pnpm-lock.yaml", PackageManager::Pnpm),
        ("yarn.lock", PackageManager::Yarn),
        ("bun.lock", PackageManager::Bun),
        ("bun.lockb", PackageManager::Bun),
        ("package-lock.json", PackageManager::Npm),
    ]
    .into_iter()
    .find(|(name, _)| root.join(name).is_file())
    .map(|(_, manager)| manager)
    .or_else(|| {
        files.iter().find_map(
            |path| match path.file_name().and_then(|name| name.to_str()) {
                Some("pnpm-lock.yaml") => Some(PackageManager::Pnpm),
                Some("yarn.lock") => Some(PackageManager::Yarn),
                Some("bun.lock" | "bun.lockb") => Some(PackageManager::Bun),
                Some("package-lock.json") => Some(PackageManager::Npm),
                _ => None,
            },
        )
    })
    .or_else(|| {
        root.join("package.json")
            .is_file()
            .then_some(PackageManager::Npm)
    });
    if node_package_manager.is_some() {
        return node_package_manager;
    }
    if root.join("Cargo.toml").is_file() {
        return Some(PackageManager::Cargo);
    }
    if root.join("go.mod").is_file() {
        return Some(PackageManager::Go);
    }
    if root.join("pom.xml").is_file() {
        return Some(PackageManager::Maven);
    }
    if root.join("build.gradle").is_file() || root.join("build.gradle.kts").is_file() {
        return Some(PackageManager::Gradle);
    }
    if root.join("composer.json").is_file() {
        return Some(PackageManager::Composer);
    }
    if root.join("Pipfile").is_file() {
        return Some(PackageManager::Pipenv);
    }
    if root.join("pyproject.toml").is_file()
        && fs::read_to_string(root.join("pyproject.toml"))
            .ok()
            .is_some_and(|contents| contents.contains("[tool.poetry]"))
    {
        return Some(PackageManager::Poetry);
    }
    None
}

fn infer_commands(
    root: &Path,
    files: &[PathBuf],
    languages: &[Language],
    manifests: &[Manifest],
    package_manager: Option<PackageManager>,
) -> ProjectCommands {
    let mut commands = ProjectCommands::default();
    if manifests
        .iter()
        .any(|manifest| manifest.kind == ManifestKind::Cargo)
    {
        commands
            .test
            .push(CommandSpec::new("cargo", ["test", "--workspace"]));
        commands
            .build
            .push(CommandSpec::new("cargo", ["build", "--workspace"]));
        commands
            .typecheck
            .push(CommandSpec::new("cargo", ["check", "--workspace"]));
        commands
            .format
            .push(CommandSpec::new("cargo", ["fmt", "--all", "--check"]));
        commands.lint.push(CommandSpec::new(
            "cargo",
            [
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ],
        ));
    }
    if languages.contains(&Language::Python) {
        if executable_available("pytest") {
            commands
                .test
                .push(CommandSpec::new("pytest", Vec::<String>::new()));
        }
        if executable_available("ruff") {
            commands.lint.push(CommandSpec::new("ruff", ["check", "."]));
        }
        if executable_available("mypy") {
            commands.typecheck.push(CommandSpec::new("mypy", ["."]));
        }
    }
    if manifests
        .iter()
        .any(|manifest| manifest.kind == ManifestKind::Go)
    {
        commands
            .test
            .push(CommandSpec::new("go", ["test", "./..."]));
        commands
            .build
            .push(CommandSpec::new("go", ["build", "./..."]));
        commands.format.push(CommandSpec::new("gofmt", ["-l", "."]));
        commands.lint.push(CommandSpec::new("go", ["vet", "./..."]));
    }
    if let Some(package_json) = find_manifest(manifests, ManifestKind::PackageJson) {
        if let Some(package_manager) = package_manager {
            add_node_commands(&mut commands, package_json, package_manager);
        }
    }
    if manifests
        .iter()
        .any(|manifest| manifest.kind == ManifestKind::Java)
    {
        commands.test.push(CommandSpec::new("mvn", ["test"]));
        commands.build.push(CommandSpec::new("mvn", ["package"]));
    }
    if manifests
        .iter()
        .any(|manifest| manifest.kind == ManifestKind::Composer)
    {
        commands.test.push(CommandSpec::new("composer", ["test"]));
    }
    let _ = (root, files);
    commands
}

fn find_manifest(manifests: &[Manifest], kind: ManifestKind) -> Option<&Path> {
    manifests
        .iter()
        .find(|manifest| manifest.kind == kind)
        .map(|manifest| manifest.path.as_path())
}

fn add_node_commands(
    commands: &mut ProjectCommands,
    package_json: &Path,
    package_manager: PackageManager,
) {
    let scripts = package_json_scripts(package_json);
    let runner = package_manager_command(package_manager);
    for (script, target) in [
        ("test", &mut commands.test),
        ("build", &mut commands.build),
        ("format", &mut commands.format),
        ("lint", &mut commands.lint),
        ("typecheck", &mut commands.typecheck),
    ] {
        if scripts.contains_key(script) {
            target.push(CommandSpec::new(runner, ["run", script]));
        }
    }
}

fn package_json_scripts(path: &Path) -> std::collections::BTreeMap<String, String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return std::collections::BTreeMap::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return std::collections::BTreeMap::new();
    };
    value
        .get("scripts")
        .and_then(serde_json::Value::as_object)
        .map(|scripts| {
            scripts
                .iter()
                .filter_map(|(name, value)| {
                    value.as_str().map(|value| (name.clone(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn package_manager_command(package_manager: PackageManager) -> &'static str {
    match package_manager {
        PackageManager::Npm => "npm",
        PackageManager::Pnpm => "pnpm",
        PackageManager::Yarn => "yarn",
        PackageManager::Bun => "bun",
        PackageManager::Cargo => "cargo",
        PackageManager::Poetry => "poetry",
        PackageManager::Pipenv => "pipenv",
        PackageManager::Go => "go",
        PackageManager::Maven => "mvn",
        PackageManager::Gradle => "gradle",
        PackageManager::Composer => "composer",
    }
}

/// Loads instructions that apply to `target` in parent-to-child order.
///
/// User instructions are lowest precedence, followed by repository and then
/// subdirectory instructions. `AGENTS.md` is primary; `CLAUDE.md` and the
/// existing `.agent/instructions.md` format remain compatible.
pub fn discover_instructions(
    workspace_root: &Path,
    target: &Path,
) -> Result<Vec<InstructionFile>, Error> {
    discover_instructions_with_global(workspace_root, target, user_instructions_path().as_deref())
}

fn discover_instructions_with_global(
    workspace_root: &Path,
    target: &Path,
    global_path: Option<&Path>,
) -> Result<Vec<InstructionFile>, Error> {
    let root = fs::canonicalize(workspace_root)?;
    let target = if target.is_absolute() {
        fs::canonicalize(target)?
    } else {
        fs::canonicalize(root.join(target))?
    };
    let target_directory = if target.is_dir() {
        target
    } else {
        target.parent().unwrap_or(&root).to_path_buf()
    };
    if !target_directory.starts_with(&root) {
        return Err(Error::InvalidConfig {
            reason: "instruction target is outside the workspace".to_owned(),
        });
    }

    let mut instructions = Vec::new();

    if let Some(path) = global_path.filter(|path| path.is_file()) {
        append_instruction(
            &mut instructions,
            path.to_path_buf(),
            InstructionKind::GlobalAgents,
            0,
        )?;
    }

    let mut directories = Vec::new();
    let mut current = target_directory;
    loop {
        directories.push(current.clone());
        if current == root || !current.pop() {
            break;
        }
    }
    directories.reverse();

    for (depth, directory) in directories.iter().enumerate() {
        let base = u8::try_from(depth.saturating_mul(8).saturating_add(16)).unwrap_or(u8::MAX);
        let local = if directory == &root {
            vec![
                ("README.md", InstructionKind::Readme),
                ("CONTRIBUTING.md", InstructionKind::Contributing),
                (".agent/instructions.md", InstructionKind::AgentInstructions),
                ("CLAUDE.md", InstructionKind::Claude),
                ("AGENTS.md", InstructionKind::Agents),
            ]
        } else {
            vec![
                (".agent/instructions.md", InstructionKind::AgentInstructions),
                ("CLAUDE.md", InstructionKind::Claude),
                ("AGENTS.md", InstructionKind::Agents),
            ]
        };
        for (index, (relative_path, kind)) in local.into_iter().enumerate() {
            append_instruction(
                &mut instructions,
                directory.join(relative_path),
                kind,
                base.saturating_add(u8::try_from(index).unwrap_or_default()),
            )?;
        }
    }
    Ok(instructions)
}

fn append_instruction(
    instructions: &mut Vec<InstructionFile>,
    path: PathBuf,
    kind: InstructionKind,
    precedence: u8,
) -> Result<(), Error> {
    if path.is_file() {
        instructions.push(InstructionFile {
            content: fs::read_to_string(&path)?,
            path,
            kind,
            precedence,
        });
    }
    Ok(())
}

/// Per-user repository instructions. `COGITO_CONFIG_DIR` is honored for
/// portable installs, matching the location used by other user preferences.
pub fn user_instructions_path() -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os("COGITO_CONFIG_DIR") {
        return Some(PathBuf::from(directory).join("AGENTS.md"));
    }
    #[cfg(windows)]
    {
        return std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|directory| directory.join("CogitoAI").join("AGENTS.md"));
    }
    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME").map(PathBuf::from).map(|home| {
            home.join("Library")
                .join("Application Support")
                .join("CogitoAI")
                .join("AGENTS.md")
        });
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(directory) = std::env::var_os("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(directory).join("cogitoai").join("AGENTS.md"));
        }
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".config").join("cogitoai").join("AGENTS.md"));
    }
    #[allow(unreachable_code)]
    None
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn optional_executable_discovery_checks_path_and_windows_extensions() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("mypy.exe"), "test executable").unwrap();
        let paths = vec![directory.path().to_path_buf()];
        assert!(executable_available_in(
            "mypy",
            &paths,
            &[".EXE".to_owned(), ".CMD".to_owned()]
        ));
        assert!(!executable_available_in(
            "ruff",
            &paths,
            &[".EXE".to_owned(), ".CMD".to_owned()]
        ));
        assert!(executable_available_in("mypy.exe", &paths, &[]));
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("test path has parent")).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn prepare_git_root(root: &Path) {
        if git_available() {
            let status = Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(root)
                .status()
                .unwrap();
            assert!(status.success());
        } else {
            fs::create_dir_all(root.join(".git")).unwrap();
        }
    }

    #[test]
    fn discovers_rust_workspace_git_and_instruction_precedence() {
        let temporary = tempdir().unwrap();
        let root = temporary.path();
        prepare_git_root(root);
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\"]\n",
        );
        write(&root.join("crates/app/src/main.rs"), "fn main() {}\n");
        write(&root.join("AGENTS.md"), "agents");
        write(&root.join("CLAUDE.md"), "claude");
        write(&root.join("README.md"), "readme");
        write(&root.join("CONTRIBUTING.md"), "contributing");
        write(&root.join(".agent/instructions.md"), "agent instructions");
        write(
            &root.join(".agent/config.toml"),
            "package_manager = \"pnpm\"\n\n[commands]\ntest = [\"pnpm\", \"test\"]\n",
        );

        let description = discover_workspace(&root.join("crates/app")).unwrap();

        assert_eq!(
            description
                .repository_root
                .as_deref()
                .and_then(canonical_path),
            canonical_path(root)
        );
        assert_eq!(description.languages, vec![Language::Rust]);
        assert!(description.monorepo.is_monorepo);
        assert_eq!(
            description.monorepo.indicators,
            vec![MonorepoIndicator::CargoWorkspace]
        );
        assert_eq!(
            description.configuration.package_manager,
            Some(PackageManager::Pnpm)
        );
        assert_eq!(
            description.configuration.commands.test,
            vec![CommandSpec::new("pnpm", ["test"])]
        );
        assert_eq!(
            description
                .instructions
                .iter()
                .map(|instruction| instruction.kind)
                .collect::<Vec<_>>(),
            vec![
                InstructionKind::Readme,
                InstructionKind::Contributing,
                InstructionKind::AgentInstructions,
                InstructionKind::Claude,
                InstructionKind::Agents,
            ]
        );
        assert!(description.git.available);
    }

    #[test]
    fn instructions_follow_global_repository_and_nested_precedence() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repo");
        let global = temporary.path().join("user/AGENTS.md");
        let nested = root.join("packages/app/src");
        write(&root.join("AGENTS.md"), "repository rules");
        write(&root.join("CLAUDE.md"), "repository compatibility rules");
        write(
            &root.join(".agent/instructions.md"),
            "repository legacy rules",
        );
        write(
            &root.join("packages/CLAUDE.md"),
            "package compatibility rules",
        );
        write(&root.join("packages/AGENTS.md"), "package rules");
        write(&root.join("packages/app/src/AGENTS.md"), "source rules");
        write(&global, "global rules");

        let instructions =
            discover_instructions_with_global(&root, &nested, Some(&global)).unwrap();

        assert_eq!(
            instructions
                .iter()
                .map(|instruction| instruction.content.as_str())
                .collect::<Vec<_>>(),
            [
                "global rules",
                "repository legacy rules",
                "repository compatibility rules",
                "repository rules",
                "package compatibility rules",
                "package rules",
                "source rules"
            ]
        );
        assert_eq!(
            instructions
                .iter()
                .filter(|instruction| instruction.path == root.join("AGENTS.md"))
                .map(|instruction| instruction.precedence)
                .next(),
            instructions
                .iter()
                .filter(|instruction| instruction.path == root.join("CLAUDE.md"))
                .map(|instruction| instruction.precedence)
                .next()
                .map(|precedence| precedence + 1)
        );
        assert!(instructions
            .windows(2)
            .all(|pair| pair[0].precedence < pair[1].precedence));
    }

    #[test]
    fn discovers_node_project_without_running_package_scripts() {
        let temporary = tempdir().unwrap();
        let root = temporary.path();
        let marker = root.join("script-ran");
        write(
            &root.join("package.json"),
            r#"{
                "name": "example",
                "workspaces": ["packages/*"],
                "scripts": {
                    "build": "node build.js",
                    "test": "node test.js",
                    "lint": "node lint.js",
                    "typecheck": "node typecheck.js"
                }
            }"#,
        );
        write(&root.join("pnpm-lock.yaml"), "lockfileVersion: 9\n");
        write(&root.join("src/index.ts"), "export const value = 1;\n");
        write(&root.join("tsconfig.json"), "{}\n");
        write(
            &root.join("build.js"),
            "require('fs').writeFileSync('script-ran', 'ran');\n",
        );

        let description = discover_workspace(root).unwrap();

        assert_eq!(
            description.languages,
            vec![Language::JavaScript, Language::TypeScript]
        );
        assert_eq!(
            description.configuration.package_manager,
            Some(PackageManager::Pnpm)
        );
        assert!(description.monorepo.is_monorepo);
        assert_eq!(
            description.configuration.commands.test,
            vec![CommandSpec::new("pnpm", ["run", "test"])]
        );
        assert_eq!(
            description.configuration.commands.build,
            vec![CommandSpec::new("pnpm", ["run", "build"])]
        );
        assert!(!marker.exists());
    }

    #[test]
    fn discovers_python_and_go_ecosystems() {
        let temporary = tempdir().unwrap();
        let root = temporary.path();
        write(
            &root.join("pyproject.toml"),
            "[project]\nname = \"example\"\n",
        );
        write(&root.join("src/main.py"), "print('hello')\n");
        write(&root.join("go.mod"), "module example\n");
        write(&root.join("main.go"), "package main\n");
        write(&root.join("Makefile"), "all:\n\techo all\n");

        let description = discover_workspace(root).unwrap();

        assert_eq!(
            description.configuration.package_manager,
            Some(PackageManager::Go)
        );
        assert_eq!(description.languages, vec![Language::Go, Language::Python]);
        assert!(description
            .manifests
            .iter()
            .any(|manifest| manifest.kind == ManifestKind::Python));
        assert!(description
            .manifests
            .iter()
            .any(|manifest| manifest.kind == ManifestKind::Go));
        assert!(description
            .configuration
            .commands
            .test
            .iter()
            .any(|command| command.program == "pytest"));
        assert!(description
            .configuration
            .commands
            .build
            .iter()
            .any(|command| command.program == "go"));
    }

    #[test]
    fn bare_python_scripts_do_not_assume_a_packaging_build_tool() {
        let temporary = tempdir().unwrap();
        let root = temporary.path();
        write(&root.join("main.py"), "print('hello')\n");

        let description = discover_workspace(root).unwrap();

        assert!(description
            .configuration
            .commands
            .build
            .iter()
            .all(|command| command.program != "python"));
    }

    #[test]
    fn malformed_project_configuration_returns_error() {
        let temporary = tempdir().unwrap();
        let root = temporary.path();
        write(&root.join(".agent/config.toml"), "package_manager = [\n");

        let error = discover_workspace(root).unwrap_err();

        assert!(matches!(error, Error::InvalidConfig { .. }));
    }
}
