//! Deterministic, bounded repository metadata for incremental coding-agent queries.
//!
//! The index intentionally stores paths and lightweight declarations/imports,
//! not file contents or embeddings. Text search uses ripgrep when installed and
//! falls back to a bounded native scan for packaged environments without it.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use harness_policy::{OperationKind, Permission};
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};

use super::{CancellationToken, Tool, ToolContext, ToolError, ToolRequest, ToolResult, ToolSpec};

const MAX_INDEX_FILES: usize = 50_000;
const MAX_INDEX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_INDEX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_QUERY_RESULTS: usize = 200;
const MAX_RESULT_BYTES: usize = 24 * 1024;
const MAX_DEPTH: usize = 24;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolKind {
    Function,
    Class,
    Interface,
    Type,
    Module,
}

impl SymbolKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Type => "type",
            Self::Module => "module",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolDefinition {
    pub name: String,
    pub kind: SymbolKind,
    pub path: String,
    pub line: usize,
    pub exported: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedFile {
    pub path: String,
    pub language: Option<String>,
    pub package: Option<String>,
    pub is_test: bool,
    pub is_configuration: bool,
    pub imports: Vec<String>,
    pub exports: Vec<String>,
    pub symbols: Vec<SymbolDefinition>,
    size: u64,
    modified: Option<std::time::SystemTime>,
    indexed_content: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PackageRoot {
    path: PathBuf,
    name: String,
}

/// A queryable repository snapshot. The snapshot is deliberately omitted from
/// model context: callers request small slices through the tools below.
pub struct RepositoryIndex {
    root: PathBuf,
    files: BTreeMap<String, IndexedFile>,
    packages: Vec<PackageRoot>,
    indexed_bytes: u64,
    last_validated: Instant,
}

impl RepositoryIndex {
    pub fn build(root: &Path) -> Result<Self, ToolError> {
        let root =
            fs::canonicalize(root).map_err(|error| index_io_error("resolve workspace", error))?;
        if !root.is_dir() {
            return Err(ToolError::NotDirectory { path: root });
        }
        let paths = enumerate_files(&root);
        let packages = discover_packages(&root, &paths);
        let mut index = Self {
            root,
            files: BTreeMap::new(),
            packages,
            indexed_bytes: 0,
            last_validated: Instant::now(),
        };
        for path in paths.into_iter().take(MAX_INDEX_FILES) {
            index.update_file(&path)?;
        }
        Ok(index)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn files(&self) -> impl Iterator<Item = &IndexedFile> {
        self.files.values()
    }

    pub fn symbols(&self) -> impl Iterator<Item = &SymbolDefinition> {
        self.files.values().flat_map(|file| file.symbols.iter())
    }

    pub fn indexed_bytes(&self) -> u64 {
        self.indexed_bytes
    }

    pub fn repo_map(&self) -> String {
        let mut sections = Vec::new();
        let mut top_level = fs::read_dir(&self.root)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                (!is_ignored_directory(&path))
                    .then(|| entry.file_name().to_string_lossy().into_owned())
            })
            .collect::<Vec<_>>();
        top_level.sort();
        top_level.truncate(20);
        sections.push(format!("Top-level: {}", top_level.join(", ")));

        if !self.packages.is_empty() {
            let mut packages = self
                .packages
                .iter()
                .map(|package| {
                    let files = self
                        .files
                        .values()
                        .filter(|file| file.package.as_deref() == Some(package.name.as_str()))
                        .count();
                    let path = package
                        .path
                        .strip_prefix(&self.root)
                        .unwrap_or(&package.path);
                    format!("{} ({}, {files} files)", path_string(path), package.name)
                })
                .collect::<Vec<_>>();
            packages.truncate(16);
            sections.push(format!("Packages: {}", packages.join("; ")));
        }

        let mut symbols = self
            .symbols()
            .filter(|symbol| symbol.exported)
            .collect::<Vec<_>>();
        if symbols.is_empty() {
            symbols = self.symbols().collect();
        }
        symbols.sort_by(|left, right| left.path.cmp(&right.path).then(left.line.cmp(&right.line)));
        let symbol_lines = symbols
            .into_iter()
            .take(32)
            .map(|symbol| {
                let package = self
                    .files
                    .get(&symbol.path)
                    .and_then(|file| file.package.as_deref())
                    .map(|name| format!(" [{name}]"))
                    .unwrap_or_default();
                format!(
                    "{}:{} {} {}{}",
                    symbol.path,
                    symbol.line,
                    symbol.kind.as_str(),
                    symbol.name,
                    package
                )
            })
            .collect::<Vec<_>>();
        if !symbol_lines.is_empty() {
            sections.push(format!("Key symbols:\n- {}", symbol_lines.join("\n- ")));
        }

        let mut relationships = BTreeMap::<String, Vec<String>>::new();
        for file in self.files.values() {
            let Some(package) = &file.package else {
                continue;
            };
            for import in &file.imports {
                push_unique(
                    relationships.entry(package.clone()).or_default(),
                    import.clone(),
                );
            }
        }
        let relationship_lines = relationships
            .into_iter()
            .take(12)
            .map(|(package, imports)| {
                let imports = imports.into_iter().take(8).collect::<Vec<_>>();
                format!("{package} imports {}", imports.join(", "))
            })
            .collect::<Vec<_>>();
        if !relationship_lines.is_empty() {
            sections.push(format!(
                "Module relationships:\n- {}",
                relationship_lines.join("\n- ")
            ));
        }
        truncate_text(&sections.join("\n"), 4_000)
    }

    fn refresh_if_stale(&mut self) -> Result<(), ToolError> {
        if self.last_validated.elapsed() < Duration::from_secs(2) {
            return Ok(());
        }
        let paths = enumerate_files(&self.root);
        let present = paths
            .iter()
            .filter_map(|path| path.strip_prefix(&self.root).ok())
            .map(path_string)
            .collect::<std::collections::HashSet<_>>();
        let removed = self
            .files
            .keys()
            .filter(|path| !present.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        let mut package_metadata_changed = !removed.is_empty();
        for path in removed {
            package_metadata_changed |= is_package_manifest(Path::new(&path));
            self.remove_file(&path);
        }
        for path in paths.into_iter().take(MAX_INDEX_FILES) {
            let relative = path
                .strip_prefix(&self.root)
                .map(path_string)
                .unwrap_or_default();
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            let stale = self.files.get(&relative).map_or(true, |file| {
                file.size != metadata.len() || file.modified != metadata.modified().ok()
            });
            if stale {
                package_metadata_changed |= is_package_manifest(Path::new(&relative));
                self.update_file(&path)?;
            }
        }
        if package_metadata_changed {
            let current_paths = enumerate_files(&self.root);
            self.packages = discover_packages(&self.root, &current_paths);
            for file in self.files.values_mut() {
                let absolute = self.root.join(&file.path);
                file.package = self
                    .packages
                    .iter()
                    .filter(|package| absolute.starts_with(&package.path))
                    .max_by_key(|package| package.path.components().count())
                    .map(|package| package.name.clone());
            }
        }
        self.last_validated = Instant::now();
        Ok(())
    }

    /// Re-parse a changed file and replace its old declarations/imports.
    pub fn update_file(&mut self, path: &Path) -> Result<(), ToolError> {
        let relative = self.relative_path(path)?;
        let absolute = self.root.join(&relative);
        self.remove_file(&relative);
        let Ok(metadata) = fs::metadata(&absolute) else {
            return Ok(());
        };
        if !metadata.is_file() {
            return Ok(());
        }
        let relative_string = path_string(Path::new(&relative));
        let language = language_for(Path::new(&relative));
        let package = self
            .packages
            .iter()
            .filter(|package| absolute.starts_with(&package.path))
            .max_by_key(|package| package.path.components().count())
            .map(|package| package.name.clone());
        let mut record = IndexedFile {
            path: relative_string.clone(),
            language,
            package,
            is_test: is_test_path(Path::new(&relative)),
            is_configuration: is_configuration_path(Path::new(&relative)),
            imports: Vec::new(),
            exports: Vec::new(),
            symbols: Vec::new(),
            size: metadata.len(),
            modified: metadata.modified().ok(),
            indexed_content: false,
        };
        if record.language.is_none() {
            self.files.insert(relative_string, record);
            return Ok(());
        }
        if self.indexed_bytes >= MAX_INDEX_TOTAL_BYTES || metadata.len() > MAX_INDEX_FILE_BYTES {
            self.files.insert(relative_string, record);
            return Ok(());
        }
        let remaining = MAX_INDEX_TOTAL_BYTES - self.indexed_bytes;
        if metadata.len() > remaining {
            self.files.insert(relative_string, record);
            return Ok(());
        }
        let contents = match fs::read_to_string(&absolute) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                self.files.insert(relative_string, record);
                return Ok(());
            }
            Err(error) => return Err(index_io_error("read indexed file", error)),
        };
        self.indexed_bytes = self.indexed_bytes.saturating_add(metadata.len());
        parse_file(&mut record, &contents);
        record.indexed_content = true;
        self.files.insert(relative_string, record);
        Ok(())
    }

    pub fn search_files(&self, query: &str, path: &str, limit: usize) -> Vec<String> {
        let query = query.to_lowercase();
        let base = normalize_query_path(path);
        self.files
            .keys()
            .filter(|file| file.starts_with(&base) && file.to_lowercase().contains(&query))
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn search_files_truncated(
        &self,
        query: &str,
        path: &str,
        limit: usize,
    ) -> (Vec<String>, bool) {
        let mut results = self.search_files(query, path, limit.saturating_add(1));
        let truncated = results.len() > limit;
        results.truncate(limit);
        (results, truncated)
    }

    pub fn find_symbol(&self, query: &str, limit: usize) -> Vec<SymbolDefinition> {
        let query = query.to_lowercase();
        self.symbols()
            .filter(|symbol| symbol.name.to_lowercase().contains(&query))
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn find_symbol_truncated(
        &self,
        query: &str,
        limit: usize,
    ) -> (Vec<SymbolDefinition>, bool) {
        let mut results = self.find_symbol(query, limit.saturating_add(1));
        let truncated = results.len() > limit;
        results.truncate(limit);
        (results, truncated)
    }

    pub fn goto_definition(&self, name: &str) -> Vec<SymbolDefinition> {
        self.symbols()
            .filter(|symbol| symbol.name == name)
            .take(MAX_QUERY_RESULTS + 1)
            .cloned()
            .collect()
    }

    pub fn file_outline(&self, path: &str) -> Result<Option<&IndexedFile>, ToolError> {
        let path = self.resolve_query_path(path)?;
        Ok(self.files.get(&path))
    }

    pub fn repo_tree(&self, path: &str, depth: usize, limit: usize) -> (Vec<String>, bool) {
        let base = normalize_query_path(path);
        let base_depth = base.split('/').filter(|part| !part.is_empty()).count();
        let mut results = self
            .files
            .keys()
            .filter(|file| file.starts_with(&base))
            .filter(|file| {
                file.split('/').filter(|part| !part.is_empty()).count()
                    <= base_depth.saturating_add(depth.max(1))
            })
            .take(limit.saturating_add(1))
            .cloned()
            .collect::<Vec<_>>();
        let truncated = results.len() > limit;
        results.truncate(limit);
        (results, truncated)
    }

    fn relative_path(&self, path: &Path) -> Result<String, ToolError> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let normalized = if absolute.exists() {
            fs::canonicalize(&absolute)
                .map_err(|error| index_io_error("resolve indexed path", error))?
        } else {
            lexical_normalize(&absolute)
        };
        let relative =
            normalized
                .strip_prefix(&self.root)
                .map_err(|_| ToolError::PathOutsideWorkspace {
                    path: path.to_path_buf(),
                })?;
        Ok(path_string(relative))
    }

    fn resolve_query_path(&self, path: &str) -> Result<String, ToolError> {
        let requested = Path::new(path);
        if requested.is_absolute() {
            return Err(ToolError::PathOutsideWorkspace {
                path: requested.to_path_buf(),
            });
        }
        let absolute = self.root.join(requested);
        if absolute.exists() {
            let resolved = fs::canonicalize(&absolute)
                .map_err(|error| index_io_error("resolve query path", error))?;
            if !resolved.starts_with(&self.root) {
                return Err(ToolError::PathOutsideWorkspace {
                    path: requested.to_path_buf(),
                });
            }
            return Ok(path_string(
                resolved
                    .strip_prefix(&self.root)
                    .expect("checked workspace containment"),
            ));
        }
        self.relative_path(requested)
    }

    fn remove_file(&mut self, relative: &str) {
        if let Some(old) = self.files.remove(relative) {
            if old.indexed_content {
                self.indexed_bytes = self.indexed_bytes.saturating_sub(old.size);
            }
        }
    }
}

/// Shared lazy cache for the standard workspace tool registry.
#[derive(Default)]
pub struct RepositoryIndexService {
    indexes: Mutex<HashMap<PathBuf, RepositoryIndex>>,
}

impl RepositoryIndexService {
    pub fn repo_map(&self, root: &Path) -> Result<String, ToolError> {
        self.with_index(root, |index| Ok(index.repo_map()))
    }

    fn with_index<T>(
        &self,
        root: &Path,
        operation: impl FnOnce(&RepositoryIndex) -> Result<T, ToolError>,
    ) -> Result<T, ToolError> {
        let root =
            fs::canonicalize(root).map_err(|error| index_io_error("resolve workspace", error))?;
        let mut indexes = self.indexes.lock().expect("repository index lock poisoned");
        if !indexes.contains_key(&root) {
            indexes.insert(root.clone(), RepositoryIndex::build(&root)?);
        }
        let index = indexes.get_mut(&root).expect("index inserted");
        index.refresh_if_stale()?;
        operation(index)
    }

    pub fn update_changed(&self, root: &Path, changed: &[PathBuf]) -> Result<(), ToolError> {
        let root =
            fs::canonicalize(root).map_err(|error| index_io_error("resolve workspace", error))?;
        let mut indexes = self.indexes.lock().expect("repository index lock poisoned");
        let Some(index) = indexes.get_mut(&root) else {
            return Ok(());
        };
        let packages_changed = changed.iter().any(|path| is_package_manifest(path));
        for path in changed {
            index.update_file(path)?;
        }
        if packages_changed {
            let paths = enumerate_files(&root);
            index.packages = discover_packages(&root, &paths);
            for file in index.files.values_mut() {
                let absolute = root.join(&file.path);
                file.package = index
                    .packages
                    .iter()
                    .filter(|package| absolute.starts_with(&package.path))
                    .max_by_key(|package| package.path.components().count())
                    .map(|package| package.name.clone());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryAction {
    SearchFiles,
    SearchText,
    FindSymbol,
    FindReferences,
    GotoDefinition,
    GetDiagnostics,
    GetFileOutline,
    GetRepoTree,
}

pub struct RepositoryTool {
    action: RepositoryAction,
    index: std::sync::Arc<RepositoryIndexService>,
}

impl RepositoryTool {
    pub fn new(action: RepositoryAction, index: std::sync::Arc<RepositoryIndexService>) -> Self {
        Self { action, index }
    }
}

impl Tool for RepositoryTool {
    fn spec(&self) -> ToolSpec {
        let (name, description, arguments_schema) = match self.action {
            RepositoryAction::SearchFiles => (
                "search_files",
                "Find repository files by path substring",
                json!({"type":"object","properties":{"query":{"type":"string","minLength":1},"path":{"type":"string"},"max_results":{"type":"integer","minimum":1,"maximum":200}},"required":["query"],"additionalProperties":false}),
            ),
            RepositoryAction::SearchText => (
                "search_text",
                "Search repository text using ripgrep when available",
                json!({"type":"object","properties":{"query":{"type":"string","minLength":1},"path":{"type":"string"},"glob":{"type":"string"},"max_results":{"type":"integer","minimum":1,"maximum":200},"case_sensitive":{"type":"boolean"}},"required":["query"],"additionalProperties":false}),
            ),
            RepositoryAction::FindSymbol => (
                "find_symbol",
                "Find indexed functions, classes, interfaces, types, and modules",
                json!({"type":"object","properties":{"name":{"type":"string","minLength":1},"max_results":{"type":"integer","minimum":1,"maximum":200}},"required":["name"],"additionalProperties":false}),
            ),
            RepositoryAction::FindReferences => (
                "find_references",
                "Find textual references to a symbol by exact identifier",
                json!({"type":"object","properties":{"name":{"type":"string","minLength":1},"path":{"type":"string"},"max_results":{"type":"integer","minimum":1,"maximum":200}},"required":["name"],"additionalProperties":false}),
            ),
            RepositoryAction::GotoDefinition => (
                "goto_definition",
                "Locate indexed definitions for an exact symbol name",
                json!({"type":"object","properties":{"name":{"type":"string","minLength":1}},"required":["name"],"additionalProperties":false}),
            ),
            RepositoryAction::GetDiagnostics => (
                "get_diagnostics",
                "Request file diagnostics from a supported language-server executable when available",
                json!({"type":"object","properties":{"path":{"type":"string"},"quick":{"type":"boolean"}},"additionalProperties":false}),
            ),
            RepositoryAction::GetFileOutline => (
                "get_file_outline",
                "Show a concise file outline with symbols, imports, and exports",
                json!({"type":"object","properties":{"path":{"type":"string","minLength":1}},"required":["path"],"additionalProperties":false}),
            ),
            RepositoryAction::GetRepoTree => (
                "get_repo_tree",
                "Show a bounded repository tree, optionally scoped to a directory",
                json!({"type":"object","properties":{"path":{"type":"string"},"depth":{"type":"integer","minimum":1,"maximum":12},"max_entries":{"type":"integer","minimum":1,"maximum":200}},"additionalProperties":false}),
            ),
        };
        ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            arguments_schema,
        }
    }

    fn required_permission(&self) -> Permission {
        Permission::ReadWorkspace
    }

    fn operation(&self) -> OperationKind {
        OperationKind::Search
    }

    fn execute(
        &self,
        context: &ToolContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        match self.action {
            RepositoryAction::SearchText => self.search_text(context, &request),
            RepositoryAction::FindReferences => self.find_references(context, &request),
            RepositoryAction::GetDiagnostics => self.get_diagnostics(context, &request),
            _ => self.with_index(context, &request),
        }
    }
}

impl RepositoryTool {
    fn get_diagnostics(
        &self,
        context: &ToolContext<'_>,
        request: &ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let root = canonical_workspace(context.working_directory)?;
        let relative = optional_string(request, "path", "")?;
        if relative.is_empty() || relative == "." {
            return Ok(ToolResult::new(
                "get_diagnostics needs a source file path. No project-wide diagnostics were requested.",
            ));
        }
        let source = Path::new(&relative);
        if source.is_absolute() {
            return Err(ToolError::PathOutsideWorkspace {
                path: source.to_path_buf(),
            });
        }
        let source = fs::canonicalize(root.join(source))
            .map_err(|error| index_io_error("resolve diagnostic file", error))?;
        if !source.starts_with(&root) {
            return Err(ToolError::PathOutsideWorkspace { path: source });
        }
        if !source.is_file() {
            return Err(ToolError::NotFile { path: source });
        }
        let metadata = fs::metadata(&source)
            .map_err(|error| index_io_error("inspect diagnostic file", error))?;
        if metadata.len() > MAX_INDEX_FILE_BYTES {
            return Err(ToolError::FileTooLarge {
                path: source,
                limit: MAX_INDEX_FILE_BYTES,
            });
        }
        let contents = fs::read_to_string(&source)
            .map_err(|error| index_io_error("read diagnostic file", error))?;
        let language = language_for(&source).unwrap_or_default();
        let quick = request
            .arguments
            .get("quick")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let diagnostics = query_language_server(
            &root,
            &source,
            &language,
            &contents,
            context.cancellation,
            quick,
        )?;
        Ok(ToolResult::new(diagnostics))
    }

    fn with_index(
        &self,
        context: &ToolContext<'_>,
        request: &ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let path = optional_string(request, "path", ".")?;
        let limit = optional_limit(request, "max_results", MAX_QUERY_RESULTS)?;
        let result =
            self.index
                .with_index(context.working_directory, |index| match self.action {
                    RepositoryAction::SearchFiles => {
                        let query = required_string(request, "query")?;
                        let (matches, truncated) =
                            index.search_files_truncated(&query, &path, limit);
                        let mut result = ToolResult::new(matches.join("\n"));
                        result
                            .metadata
                            .insert("count".to_owned(), json!(matches.len()));
                        result.truncated = truncated;
                        Ok(result)
                    }
                    RepositoryAction::FindSymbol => {
                        let name = required_string(request, "name")?;
                        let (matches, truncated) = index.find_symbol_truncated(&name, limit);
                        let output = matches
                            .iter()
                            .map(format_symbol)
                            .collect::<Vec<_>>()
                            .join("\n");
                        let mut result = ToolResult::new(output);
                        result
                            .metadata
                            .insert("count".to_owned(), json!(matches.len()));
                        result.truncated = truncated;
                        Ok(result)
                    }
                    RepositoryAction::GotoDefinition => {
                        let name = required_string(request, "name")?;
                        let mut matches = index.goto_definition(&name);
                        let truncated = matches.len() > limit;
                        matches.truncate(limit);
                        let output = matches
                            .iter()
                            .map(format_symbol)
                            .collect::<Vec<_>>()
                            .join("\n");
                        let mut result = ToolResult::new(output);
                        result
                            .metadata
                            .insert("count".to_owned(), json!(matches.len()));
                        result.truncated = truncated;
                        Ok(result)
                    }
                    RepositoryAction::GetFileOutline => {
                        let file_path = required_string(request, "path")?;
                        let Some(file) = index.file_outline(&file_path)? else {
                            return Err(ToolError::NotFound {
                                path: PathBuf::from(file_path),
                            });
                        };
                        let mut result = ToolResult::new(format_file_outline(file));
                        result.truncated = file.symbols.len() > 80;
                        Ok(result)
                    }
                    RepositoryAction::GetRepoTree => {
                        let depth = optional_usize(request, "depth", 4, 1, 12)?;
                        let max_entries = optional_limit(request, "max_entries", 100)?;
                        let (entries, truncated) = index.repo_tree(&path, depth, max_entries);
                        let mut result = ToolResult::new(entries.join("\n"));
                        result
                            .metadata
                            .insert("count".to_owned(), json!(entries.len()));
                        result.truncated = truncated;
                        Ok(result)
                    }
                    _ => unreachable!("action is handled outside the index query"),
                })?;
        Ok(bound_result(result))
    }

    fn search_text(
        &self,
        context: &ToolContext<'_>,
        request: &ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let query = required_string(request, "query")?;
        let path = optional_string(request, "path", ".")?;
        let glob = optional_string(request, "glob", "**")?;
        let limit = optional_limit(request, "max_results", 100)?;
        let case_sensitive = optional_bool(request, "case_sensitive", false)?;
        let root = canonical_workspace(context.working_directory)?;
        let search_root = resolve_search_root(&root, &path)?;
        let (lines, truncated) = ripgrep_search(
            &root,
            &search_root,
            &query,
            &glob,
            case_sensitive,
            false,
            limit,
        )?;
        let mut result = ToolResult::new(lines.join("\n"));
        result
            .metadata
            .insert("count".to_owned(), json!(lines.len()));
        result.metadata.insert(
            "engine".to_owned(),
            json!(if has_ripgrep() {
                "ripgrep"
            } else {
                "native-fallback"
            }),
        );
        result.truncated = truncated;
        Ok(bound_result(result))
    }

    fn find_references(
        &self,
        context: &ToolContext<'_>,
        request: &ToolRequest,
    ) -> Result<ToolResult, ToolError> {
        let name = required_string(request, "name")?;
        let path = optional_string(request, "path", ".")?;
        let limit = optional_limit(request, "max_results", 100)?;
        let root = canonical_workspace(context.working_directory)?;
        let search_root = resolve_search_root(&root, &path)?;
        let (lines, truncated) =
            ripgrep_search(&root, &search_root, &name, "**", true, true, limit)?;
        let mut result = ToolResult::new(lines.join("\n"));
        result
            .metadata
            .insert("count".to_owned(), json!(lines.len()));
        result.truncated = truncated;
        Ok(bound_result(result))
    }
}

fn query_language_server(
    root: &Path,
    source: &Path,
    language: &str,
    contents: &str,
    cancellation: Option<&CancellationToken>,
    quick: bool,
) -> Result<String, ToolError> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(ToolError::Process {
            message: "diagnostics request cancelled".to_owned(),
        });
    }
    let (program, arguments) = match language {
        "rust" => ("rust-analyzer", vec!["--stdio"]),
        "typescript" | "javascript" => ("typescript-language-server", vec!["--stdio"]),
        "python" => ("pyright-langserver", vec!["--stdio"]),
        _ => {
            return Ok(format!(
                "No supported language server is configured for {}.",
                source.display()
            ))
        }
    };
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::process::configure_process_group(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return Ok(format!(
                "No {program} executable is available; diagnostics are unavailable for {}.",
                source.display()
            ))
        }
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return Ok(format!("Could not read diagnostics from {program}."));
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        return Ok(format!(
            "Could not send a diagnostics request to {program}."
        ));
    };
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        while let Ok(message) = read_lsp_message(&mut reader) {
            if sender.send(message).is_err() {
                break;
            }
        }
    });

    let uri = file_uri(source);
    let document = LspDocument {
        root,
        source,
        uri: &uri,
        language,
        contents,
        cancellation,
    };
    let startup_timeout = if quick {
        Duration::from_millis(300)
    } else {
        Duration::from_secs(5)
    };
    let diagnostics_timeout = if quick {
        Duration::from_millis(300)
    } else {
        Duration::from_secs(4)
    };
    let startup_result = initialize_lsp(
        program,
        &mut stdin,
        &receiver,
        &document,
        startup_timeout,
        diagnostics_timeout,
    );
    let response = match startup_result {
        Ok(response) => response,
        Err(message) => {
            let _ = child.kill();
            let _ = child.wait();
            if message == "diagnostics request cancelled" {
                return Err(ToolError::Process { message });
            }
            return Ok(format!("{program} diagnostics failed: {message}"));
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    Ok(response)
}

struct LspDocument<'a> {
    root: &'a Path,
    source: &'a Path,
    uri: &'a str,
    language: &'a str,
    contents: &'a str,
    cancellation: Option<&'a CancellationToken>,
}

fn initialize_lsp(
    program: &str,
    stdin: &mut ChildStdin,
    receiver: &Receiver<Value>,
    document: &LspDocument<'_>,
    startup_timeout: Duration,
    diagnostics_timeout: Duration,
) -> Result<String, String> {
    let root_uri = file_uri(document.root);
    send_lsp_message(
        stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{
                "processId":std::process::id(),
                "rootUri":root_uri,
                "workspaceFolders":[{"uri":root_uri,"name":"workspace"}],
                "capabilities":{
                    "general":{"positionEncodings":["utf-16"]},
                    "textDocument":{"diagnostic":{"dynamicRegistration":false}}
                }
            }
        }),
    )?;
    let init_deadline = Instant::now() + startup_timeout;
    loop {
        let message = receive_lsp(receiver, init_deadline, document.cancellation)?;
        if message.get("id").and_then(Value::as_i64) == Some(1) {
            if message.get("error").is_some() {
                return Err("server rejected initialization".to_owned());
            }
            break;
        }
        answer_lsp_request(stdin, &message)?;
    }
    send_lsp_message(
        stdin,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )?;
    send_lsp_message(
        stdin,
        &json!({
            "jsonrpc":"2.0",
            "method":"textDocument/didOpen",
            "params":{"textDocument":{"uri":document.uri,"languageId":lsp_language_id(document.language),"version":1,"text":document.contents}}
        }),
    )?;
    send_lsp_message(
        stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"textDocument/diagnostic",
            "params":{"textDocument":{"uri":document.uri}}
        }),
    )?;
    let deadline = Instant::now() + diagnostics_timeout;
    loop {
        let message = match receive_lsp(receiver, deadline, document.cancellation) {
            Ok(message) => message,
            Err(error) if error == "diagnostics request cancelled" => return Err(error),
            Err(_) => {
                return Ok(format!(
                    "{program} did not return diagnostics for {} within the time limit.",
                    document.source.display()
                ))
            }
        };
        answer_lsp_request(stdin, &message)?;
        if message.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
        {
            let params = &message["params"];
            if params.get("uri").and_then(Value::as_str) == Some(document.uri) {
                return Ok(render_diagnostics(
                    params.get("diagnostics").and_then(Value::as_array),
                ));
            }
        }
        if message.get("id").and_then(Value::as_i64) == Some(2) {
            if let Some(items) = message
                .get("result")
                .and_then(|result| result.get("items"))
                .and_then(Value::as_array)
            {
                return Ok(render_diagnostics(Some(items)));
            }
        }
    }
}

fn receive_lsp(
    receiver: &Receiver<Value>,
    deadline: Instant,
    cancellation: Option<&CancellationToken>,
) -> Result<Value, String> {
    loop {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err("diagnostics request cancelled".to_owned());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("language server timed out".to_owned());
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(message) => return Ok(message),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("language server closed output".to_owned())
            }
        }
    }
}

fn answer_lsp_request(stdin: &mut ChildStdin, message: &Value) -> Result<(), String> {
    let (Some(id), Some(method)) = (
        message.get("id"),
        message.get("method").and_then(Value::as_str),
    ) else {
        return Ok(());
    };
    let result = if method == "workspace/configuration" {
        let length = message
            .get("params")
            .and_then(|params| params.get("items"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        Value::Array(vec![Value::Null; length])
    } else {
        Value::Null
    };
    send_lsp_message(stdin, &json!({"jsonrpc":"2.0","id":id,"result":result}))
}

fn send_lsp_message(stdin: &mut ChildStdin, message: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(message).map_err(|error| error.to_string())?;
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len())
        .and_then(|()| stdin.write_all(&body))
        .and_then(|()| stdin.flush())
        .map_err(|error| error.to_string())
}

fn read_lsp_message(reader: &mut impl BufRead) -> Result<Value, String> {
    let mut content_length = None;
    let mut line = String::new();
    loop {
        line.clear();
        let count = reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("language server closed output".to_owned());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((header, value)) = line.split_once(':') {
            if header.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }
    let length = content_length.ok_or_else(|| "missing Content-Length header".to_owned())?;
    if length > 8 * 1024 * 1024 {
        return Err("language-server response exceeded the size limit".to_owned());
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&body).map_err(|error| error.to_string())
}

fn render_diagnostics(diagnostics: Option<&Vec<Value>>) -> String {
    let Some(diagnostics) = diagnostics else {
        return "Language server returned no diagnostic details.".to_owned();
    };
    if diagnostics.is_empty() {
        return "No diagnostics reported by the language server.".to_owned();
    }
    diagnostics
        .iter()
        .take(100)
        .map(|diagnostic| {
            let range = &diagnostic["range"]["start"];
            let line = range["line"].as_u64().unwrap_or(0) + 1;
            let column = range["character"].as_u64().unwrap_or(0) + 1;
            let severity = match diagnostic["severity"].as_u64() {
                Some(1) => "error",
                Some(2) => "warning",
                Some(3) => "info",
                Some(4) => "hint",
                _ => "diagnostic",
            };
            let message = diagnostic["message"]
                .as_str()
                .unwrap_or("unspecified diagnostic");
            format!(
                "{severity} {line}:{column}: {}",
                truncate_text(message, 400)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn lsp_language_id(language: &str) -> &'static str {
    match language {
        "rust" => "rust",
        "typescript" => "typescript",
        "javascript" => "javascript",
        "python" => "python",
        _ => "plaintext",
    }
}

fn file_uri(path: &Path) -> String {
    let mut path = path.to_string_lossy().replace('\\', "/");
    if path.len() >= 2 && path.as_bytes()[1] == b':' {
        path.insert(0, '/');
    }
    let encoded = path
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"/:@-_.~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    format!("file://{encoded}")
}

fn format_file_outline(file: &IndexedFile) -> String {
    let mut lines = vec![format!(
        "{} | language: {} | package: {} | test: {} | config: {}",
        file.path,
        file.language.as_deref().unwrap_or("unknown"),
        file.package.as_deref().unwrap_or("unknown"),
        file.is_test,
        file.is_configuration
    )];
    if !file.imports.is_empty() {
        lines.push(format!("imports: {}", file.imports.join(", ")));
    }
    if !file.exports.is_empty() {
        lines.push(format!("exports: {}", file.exports.join(", ")));
    }
    if !file.symbols.is_empty() {
        lines.push("symbols:".to_owned());
        lines.extend(
            file.symbols
                .iter()
                .take(80)
                .map(|symbol| format!("  {}", format_symbol(symbol))),
        );
    }
    lines.join("\n")
}

fn format_symbol(symbol: &SymbolDefinition) -> String {
    format!(
        "{} [{}] at {}:{}{}",
        symbol.name,
        symbol.kind.as_str(),
        symbol.path,
        symbol.line,
        if symbol.exported { " (exported)" } else { "" }
    )
}

fn parse_file(file: &mut IndexedFile, contents: &str) {
    let Some(language) = file.language.as_deref() else {
        return;
    };
    let patterns = patterns();
    for (line_index, line) in contents.lines().enumerate() {
        let line_number = line_index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("//")
            || trimmed.starts_with('*')
        {
            continue;
        }
        if let Some(import) = parse_import(language, trimmed) {
            push_unique(&mut file.imports, import);
        }
        for (regex, kind) in patterns.for_language(language) {
            if let Some(captures) = regex.captures(trimmed) {
                if let Some(name) = captures.get(1).map(|capture| capture.as_str()) {
                    let exported = is_export(language, trimmed) || trimmed.starts_with("pub ");
                    if exported {
                        push_unique(&mut file.exports, name.to_owned());
                    }
                    file.symbols.push(SymbolDefinition {
                        name: name.to_owned(),
                        kind,
                        path: file.path.clone(),
                        line: line_number,
                        exported,
                    });
                }
            }
        }
        if is_export(language, trimmed)
            && (trimmed.starts_with("export {")
                || trimmed.starts_with("pub use ")
                || trimmed.starts_with("__all__"))
        {
            if let Some(name) = parse_export_name(trimmed) {
                push_unique(&mut file.exports, name);
            }
        }
    }
    file.symbols
        .sort_by(|left, right| left.line.cmp(&right.line).then(left.name.cmp(&right.name)));
}

struct Patterns {
    function: Regex,
    class: Regex,
    interface: Regex,
    type_alias: Regex,
    module: Regex,
    rust_type: Regex,
    rust_function: Regex,
    go_type: Regex,
    go_function: Regex,
    python_function: Regex,
    python_class: Regex,
    ts_function: Regex,
    ts_arrow: Regex,
}

impl Patterns {
    fn new() -> Self {
        Self {
            function: Regex::new(r"^\s*(?:(?:pub(?:\([^)]*\))?|export|async|static|public|private|protected|default|declare|abstract)\s+)*(?:async\s+)?function\s+([A-Za-z_$][\w$]*)").expect("valid function regex"),
            class: Regex::new(r"^\s*(?:(?:export|default|abstract|declare|public)\s+)*class\s+([A-Za-z_$][\w$]*)").expect("valid class regex"),
            interface: Regex::new(r"^\s*(?:(?:export|declare)\s+)*interface\s+([A-Za-z_$][\w$]*)").expect("valid interface regex"),
            type_alias: Regex::new(r"^\s*(?:(?:pub|export|declare)\s+)*type\s+([A-Za-z_$][\w$]*)").expect("valid type regex"),
            module: Regex::new(r"^\s*(?:pub\s+)?mod\s+([A-Za-z_][\w]*)").expect("valid module regex"),
            rust_type: Regex::new(r"^\s*(?:(?:pub(?:\([^)]*\))?|unsafe)\s+)*(?:struct|enum|union|trait|type)\s+([A-Za-z_][\w]*)").expect("valid Rust type regex"),
            rust_function: Regex::new(r#"^\s*(?:(?:pub(?:\([^)]*\))?|async|unsafe|const|extern(?:\s+"[^"]+")?)\s+)*fn\s+([A-Za-z_][\w]*)"#).expect("valid Rust function regex"),
            go_type: Regex::new(r"^\s*type\s+([A-Za-z_][\w]*)\s+(?:struct|interface)").expect("valid Go type regex"),
            go_function: Regex::new(r"^\s*func\s+(?:\([^)]*\)\s*)?([A-Za-z_][\w]*)\s*\(").expect("valid Go function regex"),
            python_function: Regex::new(r"^\s*(?:async\s+)?def\s+([A-Za-z_][\w]*)").expect("valid Python function regex"),
            python_class: Regex::new(r"^\s*class\s+([A-Za-z_][\w]*)").expect("valid Python class regex"),
            ts_function: Regex::new(r"^\s*(?:(?:export|default|async|declare)\s+)*function\s+([A-Za-z_$][\w$]*)").expect("valid TypeScript function regex"),
            ts_arrow: Regex::new(r"^\s*(?:export\s+)?(?:const|let)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*=>").expect("valid arrow function regex"),
        }
    }

    fn for_language(&self, language: &str) -> Vec<(&Regex, SymbolKind)> {
        match language {
            "rust" => vec![
                (&self.rust_type, SymbolKind::Type),
                (&self.rust_function, SymbolKind::Function),
                (&self.module, SymbolKind::Module),
            ],
            "python" => vec![
                (&self.python_class, SymbolKind::Class),
                (&self.python_function, SymbolKind::Function),
            ],
            "typescript" | "javascript" => vec![
                (&self.class, SymbolKind::Class),
                (&self.interface, SymbolKind::Interface),
                (&self.type_alias, SymbolKind::Type),
                (&self.ts_function, SymbolKind::Function),
                (&self.ts_arrow, SymbolKind::Function),
            ],
            "go" => vec![
                (&self.go_type, SymbolKind::Type),
                (&self.go_function, SymbolKind::Function),
            ],
            _ => vec![
                (&self.class, SymbolKind::Class),
                (&self.interface, SymbolKind::Interface),
                (&self.type_alias, SymbolKind::Type),
                (&self.function, SymbolKind::Function),
            ],
        }
    }
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(Patterns::new)
}

fn parse_import(language: &str, line: &str) -> Option<String> {
    let import = match language {
        "rust" => line
            .strip_prefix("use ")
            .or_else(|| line.strip_prefix("pub use "))
            .map(|value| value.trim_end_matches(';')),
        "python" => line
            .strip_prefix("from ")
            .and_then(|value| value.split_once(" import ").map(|(module, _)| module))
            .or_else(|| {
                line.strip_prefix("import ")
                    .map(|value| value.split(',').next().unwrap_or(value))
            }),
        "go" => line
            .strip_prefix("import ")
            .map(|value| value.trim_matches(['"', '(', ')', ' ']))
            .filter(|value| !value.is_empty()),
        _ => {
            if let Some((_, module)) = line.split_once(" from ") {
                module
                    .trim()
                    .trim_end_matches(';')
                    .trim_matches(['"', '\''])
                    .into()
            } else if let Some(module) = line.strip_prefix("import ") {
                module
                    .trim()
                    .trim_end_matches(';')
                    .trim_matches(['"', '\''])
                    .into()
            } else {
                None
            }
        }
    }?;
    let import = import.trim();
    (!import.is_empty()).then(|| import.to_owned())
}

fn is_export(language: &str, line: &str) -> bool {
    line.starts_with("export ")
        || (language == "rust" && line.starts_with("pub use "))
        || (language == "python" && line.starts_with("__all__"))
}

fn parse_export_name(line: &str) -> Option<String> {
    if line.starts_with("__all__") {
        return Some("__all__".to_owned());
    }
    let remainder = line
        .strip_prefix("export ")
        .or_else(|| line.strip_prefix("pub use "))?;
    let candidate = remainder
        .split(|character: char| character.is_whitespace() || matches!(character, '{' | ';' | '='))
        .find(|part| {
            !part.is_empty()
                && !matches!(
                    *part,
                    "default"
                        | "async"
                        | "declare"
                        | "abstract"
                        | "function"
                        | "class"
                        | "interface"
                        | "type"
                        | "const"
                        | "let"
                        | "var"
                )
        })?;
    Some(candidate.trim_matches(['{', '}', '*']).to_owned())
}

fn discover_packages(root: &Path, files: &[PathBuf]) -> Vec<PackageRoot> {
    let mut packages = Vec::new();
    for file in files {
        let Some(name) = file.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let is_manifest = matches!(
            name,
            "Cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" | "pom.xml"
        );
        if !is_manifest {
            continue;
        }
        let Ok(metadata) = fs::metadata(file) else {
            continue;
        };
        if metadata.len() > MAX_INDEX_FILE_BYTES {
            continue;
        }
        let Ok(contents) = fs::read_to_string(file) else {
            continue;
        };
        let package_name = match name {
            "package.json" => serde_json::from_str::<Value>(&contents)
                .ok()
                .and_then(|value| value.get("name").and_then(Value::as_str).map(str::to_owned)),
            "Cargo.toml" | "pyproject.toml" => toml_package_name(&contents),
            "go.mod" => contents
                .lines()
                .find_map(|line| line.trim().strip_prefix("module ").map(str::to_owned)),
            _ => None,
        };
        let name = package_name.unwrap_or_else(|| {
            file.parent()
                .and_then(Path::file_name)
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".to_owned())
        });
        let path = file.parent().unwrap_or(root).to_path_buf();
        packages.push(PackageRoot { path, name });
    }
    packages.sort_by(|left, right| {
        left.path
            .components()
            .count()
            .cmp(&right.path.components().count())
            .then(left.path.cmp(&right.path))
    });
    packages.dedup_by(|left, right| left.path == right.path);
    packages
}

fn toml_package_name(contents: &str) -> Option<String> {
    static NAME: OnceLock<Regex> = OnceLock::new();
    NAME.get_or_init(|| {
        Regex::new(r#"(?m)^\s*name\s*=\s*"([^"]+)""#).expect("valid manifest name regex")
    })
    .captures(contents)
    .and_then(|capture| capture.get(1).map(|value| value.as_str().to_owned()))
}

fn is_package_manifest(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("Cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" | "pom.xml")
    )
}

fn enumerate_files(root: &Path) -> Vec<PathBuf> {
    if let Some(paths) = ripgrep_files(root) {
        return paths;
    }
    native_files(root)
}

fn ripgrep_files(root: &Path) -> Option<Vec<PathBuf>> {
    let mut command = Command::new("rg");
    command
        .args(["--files", "--hidden", "--no-messages"])
        .args([
            "-g",
            "!.git/**",
            "-g",
            "!node_modules/**",
            "-g",
            "!.pnpm-store/**",
            "-g",
            "!target/**",
            "-g",
            "!dist/**",
            "-g",
            "!build/**",
            "-g",
            "!coverage/**",
            "-g",
            "!.next/**",
            "-g",
            "!.turbo/**",
            "-g",
            "!.pytest_cache/**",
            "-g",
            "!.mypy_cache/**",
            "-g",
            "!vendor/**",
            "-g",
            "!.venv/**",
            "-g",
            "!venv/**",
            "-g",
            "!__pycache__/**",
        ])
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::process::configure_process_group(&mut command);
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let reader = BufReader::new(stdout);
    let mut paths = Vec::new();
    for line in reader.lines().take(MAX_INDEX_FILES + 1) {
        let Ok(line) = line else { break };
        let path = root.join(line);
        if path.is_file() {
            paths.push(path);
        }
    }
    if paths.len() > MAX_INDEX_FILES {
        let _ = child.kill();
        paths.truncate(MAX_INDEX_FILES);
    }
    let _ = child.wait();
    paths.sort();
    Some(paths)
}

fn native_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > MAX_DEPTH || files.len() >= MAX_INDEX_FILES {
            continue;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() || is_ignored_directory(&path) {
                continue;
            }
            if kind.is_dir() {
                pending.push((path, depth + 1));
            } else if kind.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn is_ignored_directory(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            ".git"
                | ".hg"
                | ".svn"
                | "node_modules"
                | ".pnpm-store"
                | "target"
                | "dist"
                | "build"
                | "coverage"
                | ".next"
                | ".turbo"
                | ".pytest_cache"
                | ".mypy_cache"
                | ".gradle"
                | "bower_components"
                | "bin"
                | "obj"
                | "vendor"
                | ".venv"
                | "venv"
                | "__pycache__"
        )
    )
}

fn language_for(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?;
    let language = match extension {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        "java" => "java",
        "cs" => "csharp",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "rb" => "ruby",
        "kt" | "kts" => "kotlin",
        _ => return None,
    };
    Some(language.to_owned())
}

fn is_test_path(path: &Path) -> bool {
    let normalized = path_string(path).to_lowercase();
    normalized.contains("/tests/")
        || normalized.contains("/__tests__/")
        || normalized.ends_with("_test.rs")
        || normalized.ends_with("_test.py")
        || normalized.ends_with(".test.ts")
        || normalized.ends_with(".spec.ts")
        || normalized.ends_with(".test.tsx")
        || normalized.ends_with(".spec.tsx")
        || normalized.ends_with("_test.go")
}

fn is_configuration_path(path: &Path) -> bool {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    matches!(
        file_name,
        "Cargo.toml"
            | "package.json"
            | "pyproject.toml"
            | "go.mod"
            | "go.work"
            | "tsconfig.json"
            | "jsconfig.json"
            | "vite.config.ts"
            | "eslint.config.js"
            | "Makefile"
            | "justfile"
            | "Justfile"
            | "Dockerfile"
            | "Cargo.lock"
            | "pnpm-workspace.yaml"
    ) || file_name.starts_with(".eslintrc")
        || file_name.starts_with(".prettierrc")
        || file_name.starts_with("pytest.ini")
}

fn normalize_query_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    if path == "." || path.is_empty() {
        String::new()
    } else {
        format!("{}/", path.trim_matches('/'))
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn canonical_workspace(path: &Path) -> Result<PathBuf, ToolError> {
    fs::canonicalize(path).map_err(|error| index_io_error("resolve workspace", error))
}

fn resolve_search_root(root: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    let path = Path::new(requested);
    if path.is_absolute() {
        return Err(ToolError::PathOutsideWorkspace {
            path: path.to_path_buf(),
        });
    }
    let candidate = fs::canonicalize(root.join(path))
        .map_err(|error| index_io_error("resolve search path", error))?;
    if !candidate.starts_with(root) || !candidate.is_dir() {
        return Err(ToolError::PathOutsideWorkspace {
            path: path.to_path_buf(),
        });
    }
    Ok(candidate)
}

fn ripgrep_search(
    root: &Path,
    search_root: &Path,
    query: &str,
    glob: &str,
    case_sensitive: bool,
    whole_word: bool,
    limit: usize,
) -> Result<(Vec<String>, bool), ToolError> {
    globset::Glob::new(glob).map_err(|error| ToolError::InvalidArguments {
        tool: "search_text".to_owned(),
        message: error.to_string(),
    })?;
    if has_ripgrep() {
        let relative_root = search_root
            .strip_prefix(root)
            .ok()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut command = Command::new("rg");
        command
            .args([
                "--line-number",
                "--no-heading",
                "--color",
                "never",
                "--hidden",
                "--fixed-strings",
                "--path-separator",
                "/",
                "--max-columns",
                "2048",
                "--max-columns-preview",
            ])
            .arg(if case_sensitive {
                "--case-sensitive"
            } else {
                "--ignore-case"
            })
            .args(whole_word.then_some("--word-regexp"))
            .args([
                "-g",
                "!.git/**",
                "-g",
                "!node_modules/**",
                "-g",
                "!.pnpm-store/**",
                "-g",
                "!target/**",
                "-g",
                "!dist/**",
                "-g",
                "!build/**",
                "-g",
                "!.next/**",
                "-g",
                "!.turbo/**",
                "-g",
                "!.pytest_cache/**",
                "-g",
                "!.mypy_cache/**",
                "-g",
                "!vendor/**",
            ])
            .arg("-g")
            .arg(glob)
            .arg("--")
            .arg(query)
            .arg(relative_root)
            .current_dir(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::process::configure_process_group(&mut command);
        if let Ok(mut child) = command.spawn() {
            let Some(stdout) = child.stdout.take() else {
                return Err(ToolError::Process {
                    message: "could not read ripgrep output".to_owned(),
                });
            };
            let mut reader = BufReader::new(stdout);
            let mut lines = Vec::new();
            let mut truncated = false;
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.read_until(b'\n', &mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {
                        if lines.len() >= limit {
                            truncated = true;
                            break;
                        }
                        if buffer.len() > 2_048 {
                            buffer.truncate(2_048);
                        }
                        let line = String::from_utf8_lossy(&buffer).trim_end().to_owned();
                        let line = line
                            .strip_prefix("./")
                            .or_else(|| line.strip_prefix(".\\"))
                            .unwrap_or(&line)
                            .replace('\\', "/");
                        lines.push(line);
                    }
                    Err(_) => break,
                }
            }
            if truncated {
                let _ = child.kill();
            }
            let _ = child.wait();
            return Ok((lines, truncated));
        }
    }
    native_search(search_root, query, glob, case_sensitive, whole_word, limit)
}

fn native_search(
    root: &Path,
    query: &str,
    glob: &str,
    case_sensitive: bool,
    whole_word: bool,
    limit: usize,
) -> Result<(Vec<String>, bool), ToolError> {
    let matcher = globset::Glob::new(glob)
        .map_err(|error| ToolError::InvalidArguments {
            tool: "search_text".to_owned(),
            message: error.to_string(),
        })?
        .compile_matcher();
    let needle = if case_sensitive {
        query.to_owned()
    } else {
        query.to_lowercase()
    };
    let word_regex = if whole_word {
        Some(
            RegexBuilder::new(&format!(r"\b{}\b", regex::escape(&needle)))
                .case_insensitive(!case_sensitive)
                .build()
                .map_err(|error| ToolError::InvalidArguments {
                    tool: "find_references".to_owned(),
                    message: error.to_string(),
                })?,
        )
    } else {
        None
    };
    let mut output = Vec::new();
    let mut truncated = false;
    let mut scanned_bytes = 0_u64;
    for path in native_files(root).into_iter().take(MAX_INDEX_FILES) {
        let relative = path.strip_prefix(root).unwrap_or(&path);
        if !matcher.is_match(relative) {
            continue;
        }
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.len() > MAX_INDEX_FILE_BYTES {
            continue;
        }
        scanned_bytes = scanned_bytes.saturating_add(metadata.len());
        if scanned_bytes > MAX_INDEX_TOTAL_BYTES {
            truncated = true;
            break;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        for (line_index, line) in contents.lines().enumerate() {
            let candidate = if case_sensitive {
                line.to_owned()
            } else {
                line.to_lowercase()
            };
            let matches = if let Some(regex) = &word_regex {
                regex.is_match(line)
            } else {
                candidate.contains(&needle)
            };
            if matches {
                if output.len() >= limit {
                    truncated = true;
                    break;
                }
                let mut text = line.to_owned();
                if text.len() > 500 {
                    text.truncate(500);
                }
                output.push(format!(
                    "{}:{}:{}",
                    path_string(relative),
                    line_index + 1,
                    text
                ));
            }
        }
        if truncated {
            break;
        }
    }
    Ok((output, truncated))
}

fn has_ripgrep() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let mut command = Command::new("rg");
        command
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::process::configure_process_group(&mut command);
        command.status().is_ok()
    })
}

fn required_string(request: &ToolRequest, key: &str) -> Result<String, ToolError> {
    request
        .arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ToolError::InvalidArguments {
            tool: request.name.clone(),
            message: format!("{key} must be a non-empty string"),
        })
}

fn optional_string(request: &ToolRequest, key: &str, default: &str) -> Result<String, ToolError> {
    request
        .arguments
        .get(key)
        .map_or(Ok(default.to_owned()), |value| {
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| ToolError::InvalidArguments {
                    tool: request.name.clone(),
                    message: format!("{key} must be a non-empty string"),
                })
        })
}

fn optional_limit(request: &ToolRequest, key: &str, default: usize) -> Result<usize, ToolError> {
    optional_usize(request, key, default, 1, MAX_QUERY_RESULTS)
}

fn optional_usize(
    request: &ToolRequest,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, ToolError> {
    request.arguments.get(key).map_or(Ok(default), |value| {
        value
            .as_u64()
            .and_then(|number| usize::try_from(number).ok())
            .filter(|number| (min..=max).contains(number))
            .ok_or_else(|| ToolError::InvalidArguments {
                tool: request.name.clone(),
                message: format!("{key} must be between {min} and {max}"),
            })
    })
}

fn optional_bool(request: &ToolRequest, key: &str, default: bool) -> Result<bool, ToolError> {
    request.arguments.get(key).map_or(Ok(default), |value| {
        value.as_bool().ok_or_else(|| ToolError::InvalidArguments {
            tool: request.name.clone(),
            message: format!("{key} must be a boolean"),
        })
    })
}

fn bound_result(mut result: ToolResult) -> ToolResult {
    if result.output.len() > MAX_RESULT_BYTES {
        let mut end = MAX_RESULT_BYTES;
        while !result.output.is_char_boundary(end) {
            end -= 1;
        }
        result.output.truncate(end);
        result.output.push_str("\n[truncated]");
        result.truncated = true;
    }
    result
}

fn index_io_error(operation: &str, error: std::io::Error) -> ToolError {
    ToolError::Io {
        operation: operation.to_owned(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::time::Instant;
    use tempfile::tempdir;

    #[test]
    fn indexes_typescript_python_rust_and_mixed_packages() {
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("packages/web/src")).unwrap();
        fs::create_dir_all(root.join("packages/api/tests")).unwrap();
        fs::create_dir_all(root.join("crates/core/src")).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"mixed-root","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        fs::write(
            root.join("packages/web/package.json"),
            r#"{"name":"web-app"}"#,
        )
        .unwrap();
        fs::write(root.join("packages/web/src/main.ts"), "import { helper } from './helper';\nexport interface User { id: string }\nexport function start() {}\nconst runTask = async () => {};\n").unwrap();
        fs::write(root.join("packages/api/service.py"), "from pathlib import Path\nclass Service:\n    def run(self):\n        return Path('.')\n").unwrap();
        fs::write(
            root.join("packages/api/tests/test_service.py"),
            "def test_service():\n    pass\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/core/Cargo.toml"),
            "[package]\nname = \"core-library\"\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/core/src/lib.rs"),
            "use std::path::Path;\npub struct Engine;\npub fn execute() {}\nmod tests;\n",
        )
        .unwrap();

        let index = RepositoryIndex::build(root).unwrap();
        assert!(index
            .find_symbol("start", 20)
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Function));
        assert!(index
            .find_symbol("User", 20)
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Interface));
        assert!(index
            .find_symbol("Service", 20)
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Class));
        assert!(index
            .find_symbol("Engine", 20)
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Type));
        assert!(index
            .find_symbol("execute", 20)
            .iter()
            .any(|symbol| symbol.exported));
        assert!(index
            .files()
            .any(|file| file.path.ends_with("test_service.py") && file.is_test));
        assert!(index
            .files()
            .any(|file| file.path == "package.json" && file.is_configuration));
        assert!(index.files().any(|file| file.path.ends_with("main.ts")
            && file.imports.iter().any(|import| import == "./helper")));
        assert!(index
            .files()
            .any(|file| file.path.ends_with("lib.rs")
                && file.package.as_deref() == Some("core-library")));
    }

    #[test]
    fn index_updates_changed_files_and_removed_files() {
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn before() {}\n").unwrap();
        let mut index = RepositoryIndex::build(root).unwrap();
        assert_eq!(index.goto_definition("before").len(), 1);
        fs::write(root.join("src/lib.rs"), "fn after() {}\n").unwrap();
        index.update_file(Path::new("src/lib.rs")).unwrap();
        assert!(index.goto_definition("before").is_empty());
        assert_eq!(index.goto_definition("after").len(), 1);
        fs::remove_file(root.join("src/lib.rs")).unwrap();
        index.update_file(Path::new("src/lib.rs")).unwrap();
        assert!(index.goto_definition("after").is_empty());
    }

    #[test]
    fn changed_file_update_is_incremental() {
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        for index in 0..250 {
            fs::write(
                root.join(format!("src/file_{index}.rs")),
                format!("pub fn item_{index}() {{}}\n"),
            )
            .unwrap();
        }
        let mut index = RepositoryIndex::build(root).unwrap();
        let started = Instant::now();
        fs::write(root.join("src/file_100.rs"), "pub fn updated() {}\n").unwrap();
        index.update_file(Path::new("src/file_100.rs")).unwrap();
        assert!(started.elapsed().as_secs() < 2);
        assert_eq!(index.goto_definition("updated").len(), 1);
    }

    #[test]
    fn lsp_framing_and_uri_encoding_are_deterministic() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let message = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut framed = message.into_bytes();
        framed.extend_from_slice(body);
        let parsed = read_lsp_message(&mut Cursor::new(framed)).unwrap();
        assert_eq!(parsed["id"], 1);
        assert_eq!(
            file_uri(Path::new("C:\\my folder\\src\\main.rs")),
            "file:///C:/my%20folder/src/main.rs"
        );
    }

    #[test]
    fn diagnostics_render_compact_severity_and_position() {
        let output = render_diagnostics(Some(&vec![json!({
            "severity": 1,
            "range": {"start": {"line": 2, "character": 4}},
            "message": "expected an expression"
        })]));
        assert_eq!(output, "error 3:5: expected an expression");
    }

    #[test]
    fn diagnostics_respect_pre_cancelled_tool_context() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = query_language_server(
            Path::new("."),
            Path::new("src/lib.rs"),
            "rust",
            "",
            Some(&cancellation),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("diagnostics request cancelled"));
    }

    #[test]
    fn large_repo_map_is_capped_instead_of_injecting_the_full_index() {
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("src")).unwrap();
        let declarations = (0..500)
            .map(|index| format!("pub fn exported_symbol_{index}() {{}}\n"))
            .collect::<String>();
        fs::write(root.join("src/lib.rs"), declarations).unwrap();
        let index = RepositoryIndex::build(root).unwrap();
        let repo_map = index.repo_map();
        assert!(repo_map.len() <= 4_000);
        assert!(repo_map.contains("Key symbols:"));
        assert!(!repo_map.contains("exported_symbol_499"));
    }
}
