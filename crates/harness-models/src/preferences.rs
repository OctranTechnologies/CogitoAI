//! Non-secret model preferences and project/session selection helpers.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ModelConfig, ProviderKind, ReasoningEffort};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelPreference {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl ModelPreference {
    pub fn from_canonical_id(
        value: &str,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<Self, ModelPreferenceError> {
        let (provider_id, model_id) = value
            .trim()
            .split_once('/')
            .ok_or(ModelPreferenceError::Invalid)?;
        let preference = Self {
            provider_id: provider_id.to_ascii_lowercase(),
            model_id: strip_model_namespace(provider_id, model_id.trim()).to_owned(),
            reasoning_effort,
        };
        preference.validate()?;
        Ok(preference)
    }

    pub fn canonical_id(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }

    pub fn apply_to(&self, config: &mut ModelConfig) -> Result<(), ModelPreferenceError> {
        self.validate()?;
        let provider = parse_provider(&self.provider_id)?;
        config.select_provider(provider);
        config.model = match provider {
            ProviderKind::OpenCodeZen | ProviderKind::OpenCodeGo => {
                format!("{}/{}", self.provider_id, self.model_id)
            }
            _ => self.model_id.clone(),
        };
        config.reasoning_effort = self.reasoning_effort;
        Ok(())
    }

    fn validate(&self) -> Result<(), ModelPreferenceError> {
        if self.model_id.is_empty() || self.model_id.len() > 200 {
            return Err(ModelPreferenceError::Invalid);
        }
        parse_provider(&self.provider_id).map(|_| ())
    }
}

#[derive(Debug, Error)]
pub enum ModelPreferenceError {
    #[error("model preference must use provider/model format")]
    Invalid,
    #[error("model preference file could not be read")]
    Read,
    #[error("model preference file could not be written")]
    Write,
    #[error("model preference file is malformed")]
    Parse,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct UserPreferencesFile {
    #[serde(default)]
    preferred_model: Option<ModelPreference>,
}

/// Location for non-secret user preferences. `COGITO_CONFIG_DIR` supports
/// portable and managed installations without changing project files.
pub fn user_model_preferences_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("COGITO_CONFIG_DIR") {
        return Some(PathBuf::from(path).join("models.toml"));
    }
    #[cfg(windows)]
    {
        return std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("CogitoAI").join("models.toml"));
    }
    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|path| path.join("Library/Application Support/CogitoAI/models.toml"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(path).join("cogitoai/models.toml"));
        }
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|path| path.join(".config/cogitoai/models.toml"));
    }
    #[allow(unreachable_code)]
    None
}

pub fn load_user_model_preference() -> Result<Option<ModelPreference>, ModelPreferenceError> {
    let Some(path) = user_model_preferences_path() else {
        return Ok(None);
    };
    load_user_model_preference_from(&path)
}

pub fn load_user_model_preference_from(
    path: &Path,
) -> Result<Option<ModelPreference>, ModelPreferenceError> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path).map_err(|_| ModelPreferenceError::Read)?;
    let preferences: UserPreferencesFile =
        toml::from_str(&contents).map_err(|_| ModelPreferenceError::Parse)?;
    if let Some(preference) = &preferences.preferred_model {
        preference.validate()?;
    }
    Ok(preferences.preferred_model)
}

pub fn save_user_model_preference(
    preference: &ModelPreference,
) -> Result<(), ModelPreferenceError> {
    let Some(path) = user_model_preferences_path() else {
        return Err(ModelPreferenceError::Write);
    };
    save_user_model_preference_to(&path, preference)
}

pub fn save_user_model_preference_to(
    path: &Path,
    preference: &ModelPreference,
) -> Result<(), ModelPreferenceError> {
    preference.validate()?;
    let preferences = UserPreferencesFile {
        preferred_model: Some(preference.clone()),
    };
    let contents = toml::to_string_pretty(&preferences).map_err(|_| ModelPreferenceError::Write)?;
    write_preference_file(path, &contents)
}

pub fn load_project_model_preference(
    workspace_root: &Path,
) -> Result<Option<ModelPreference>, ModelPreferenceError> {
    let path = workspace_root.join(".agent/config.toml");
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path).map_err(|_| ModelPreferenceError::Read)?;
    let document = contents
        .parse::<toml::Table>()
        .map_err(|_| ModelPreferenceError::Parse)?;
    let Some(model) = document.get("model").and_then(toml::Value::as_table) else {
        return Ok(None);
    };
    let Some(provider) = model.get("provider").and_then(toml::Value::as_str) else {
        return Ok(None);
    };
    let Some(model_id) = model.get("model").and_then(toml::Value::as_str) else {
        return Ok(None);
    };
    let reasoning_effort = model
        .get("reasoning_effort")
        .map(|value| value.clone().try_into())
        .transpose()
        .map_err(|_| ModelPreferenceError::Parse)?;
    let preference = ModelPreference {
        provider_id: provider.to_ascii_lowercase(),
        model_id: strip_model_namespace(provider, model_id).to_owned(),
        reasoning_effort,
    };
    preference.validate()?;
    Ok(Some(preference))
}

pub fn save_project_model_preference(
    workspace_root: &Path,
    preference: &ModelPreference,
) -> Result<(), ModelPreferenceError> {
    preference.validate()?;
    let config_dir = workspace_root.join(".agent");
    let path = config_dir.join("config.toml");
    let mut document = if path.exists() {
        fs::read_to_string(&path)
            .map_err(|_| ModelPreferenceError::Read)?
            .parse::<toml::Table>()
            .map_err(|_| ModelPreferenceError::Parse)?
    } else {
        toml::Table::new()
    };
    let mut model = document
        .get("model")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    model.insert(
        "provider".to_owned(),
        toml::Value::String(preference.provider_id.clone()),
    );
    model.insert(
        "model".to_owned(),
        toml::Value::String(preference.model_id.clone()),
    );
    match preference.reasoning_effort {
        Some(effort) => {
            let value = toml::Value::try_from(effort).map_err(|_| ModelPreferenceError::Write)?;
            model.insert("reasoning_effort".to_owned(), value);
        }
        None => {
            model.remove("reasoning_effort");
        }
    }
    document.insert("model".to_owned(), toml::Value::Table(model));
    let contents = toml::to_string_pretty(&document).map_err(|_| ModelPreferenceError::Write)?;
    fs::create_dir_all(&config_dir).map_err(|_| ModelPreferenceError::Write)?;
    write_preference_file(&path, &contents)
}

pub fn has_model_environment_override() -> bool {
    std::env::var_os("COGITO_MODEL_PROVIDER").is_some()
        || std::env::var_os("COGITO_MODEL").is_some()
        || std::env::var_os("COGITO_MODEL_REASONING_EFFORT").is_some()
}

fn write_preference_file(path: &Path, contents: &str) -> Result<(), ModelPreferenceError> {
    let parent = path.parent().ok_or(ModelPreferenceError::Write)?;
    fs::create_dir_all(parent).map_err(|_| ModelPreferenceError::Write)?;
    fs::write(path, contents).map_err(|_| ModelPreferenceError::Write)
}

fn strip_model_namespace<'a>(provider_id: &str, model_id: &'a str) -> &'a str {
    model_id
        .strip_prefix(provider_id)
        .and_then(|value| value.strip_prefix('/'))
        .unwrap_or(model_id)
}

fn parse_provider(provider_id: &str) -> Result<ProviderKind, ModelPreferenceError> {
    match provider_id {
        "mock" => Ok(ProviderKind::Mock),
        "openai" => Ok(ProviderKind::OpenAi),
        "anthropic" => Ok(ProviderKind::Anthropic),
        "gemini" => Ok(ProviderKind::Gemini),
        "opencode-zen" => Ok(ProviderKind::OpenCodeZen),
        "opencode-go" => Ok(ProviderKind::OpenCodeGo),
        _ => Err(ModelPreferenceError::Invalid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn user_preference_round_trips_provider_model_and_effort() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("models.toml");
        let preference = ModelPreference::from_canonical_id(
            "opencode-go/vendor/model-x",
            Some(ReasoningEffort::High),
        )
        .unwrap();
        save_user_model_preference_to(&path, &preference).unwrap();
        assert_eq!(
            load_user_model_preference_from(&path).unwrap(),
            Some(preference)
        );
        let mut config = ModelConfig::default();
        load_user_model_preference_from(&path)
            .unwrap()
            .unwrap()
            .apply_to(&mut config)
            .unwrap();
        assert_eq!(config.provider, ProviderKind::OpenCodeGo);
        assert_eq!(config.model, "opencode-go/vendor/model-x");
        assert_eq!(config.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn project_preference_preserves_existing_project_configuration() {
        let directory = tempdir().unwrap();
        let config_dir = directory.path().join(".agent");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            "package_manager = 'pnpm'\n[commands]\ntest = ['pnpm', 'test']\n",
        )
        .unwrap();
        let preference = ModelPreference::from_canonical_id(
            "anthropic/claude-sonnet-4-6",
            Some(ReasoningEffort::Medium),
        )
        .unwrap();
        save_project_model_preference(directory.path(), &preference).unwrap();
        assert_eq!(
            load_project_model_preference(directory.path()).unwrap(),
            Some(preference)
        );
        let config = fs::read_to_string(config_dir.join("config.toml")).unwrap();
        assert!(config.contains("package_manager = \"pnpm\""));
        assert!(config.contains("test = [\n    \"pnpm\",\n    \"test\",\n]"));
    }

    #[test]
    fn explicit_namespaces_must_be_well_formed() {
        assert!(ModelPreference::from_canonical_id("openai", None).is_err());
        assert!(ModelPreference::from_canonical_id("unknown/model", None).is_err());
        assert!(ModelPreference::from_canonical_id("openai/", None).is_err());
    }
}
