//! TOML config loader.
//!
//! Three layers, in priority order (highest wins):
//!   1. CLI flags (handled by clap, not this module)
//!   2. Project-local: `<cwd>/.nanopi/config.toml`
//!   3. Global: `~/.nanopi/config.toml`
//!
//! Missing files → defaults. Invalid TOML → error with file path.
//! All fields optional; serde defaults fill them in.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse TOML in {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Default model id (used when --model is absent).
    pub default_model: Option<String>,

    /// Default base URL (OpenAI-compatible provider root).
    pub default_base_url: Option<String>,

    /// How to resolve API key. v0.5 supports `env` (read OPENAI_API_KEY).
    /// v0.6+ will add `file` (read 0600 file).
    #[serde(default = "default_api_key_source")]
    pub api_key_source: String,

    /// Optional path to API key file (0600 perms).
    pub api_key_file: Option<PathBuf>,

    #[serde(default)]
    pub tools: ToolsConfig,

    #[serde(default)]
    pub trust: TrustConfig,

    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub default_timeout_secs: Option<u64>,
    pub bash_max_output_bytes: Option<usize>,
    pub bash_max_output_lines: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TrustConfig {
    /// One of: `ask`, `always`, `never`. Default `ask`.
    pub default: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: Option<String>,
    pub file: Option<PathBuf>,
}

fn default_api_key_source() -> String {
    "env".to_string()
}

impl Config {
    /// Built-in defaults when no config files exist.
    pub fn builtin_defaults() -> Self {
        Self {
            default_model: None,
            default_base_url: None,
            api_key_source: "env".to_string(),
            api_key_file: None,
            tools: ToolsConfig::default(),
            trust: TrustConfig::default(),
            logging: LoggingConfig::default(),
        }
    }

    /// Resolve a config value by key, in priority order:
    /// flag > project > global > builtin default.
    ///
    /// Used by `Args::resolve()` to fill in fields the user didn't pass.
    pub fn resolve_string(&self, key: &str) -> Option<String> {
        match key {
            "default_model" => self.default_model.clone(),
            "default_base_url" => self.default_base_url.clone(),
            _ => None,
        }
    }
}

/// Load and merge configs from global + project-local. Missing files are
/// silently ignored (use defaults). Malformed TOML is a hard error.
pub fn load_config(cwd: &Path) -> Result<Config, ConfigError> {
    let global_path = global_config_path();
    let local_path = cwd.join(".nanopi").join("config.toml");

    let mut merged = Config::builtin_defaults();

    if let Some(path) = global_path {
        if path.exists() {
            merged = merge(merged, load_one(&path)?);
        }
    }

    if local_path.exists() {
        merged = merge(merged, load_one(&local_path)?);
    }

    Ok(merged)
}

/// Path to the global config file, if $HOME is available.
pub fn global_config_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".nanopi").join("config.toml"))
}

fn load_one(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    toml::from_str::<Config>(&text).map_err(|e| ConfigError::Toml {
        path: path.to_path_buf(),
        source: e,
    })
}

/// Deep-merge two Configs. `b` wins on field-level conflicts.
///
/// Both sides already have serde defaults applied, so a field present
/// only in `a` keeps its value. `Option<String>` semantics: `None` means
/// "unset" (don't overwrite `b`'s value), `Some` means "use this".
fn merge(a: Config, b: Config) -> Config {
    Config {
        default_model: b.default_model.or(a.default_model),
        default_base_url: b.default_base_url.or(a.default_base_url),
        api_key_source: if b.api_key_source != "env" || a.api_key_source != "env" {
            // either side set something; b wins unless b is the default
            if b.api_key_source == "env" {
                a.api_key_source
            } else {
                b.api_key_source
            }
        } else {
            "env".to_string()
        },
        api_key_file: b.api_key_file.or(a.api_key_file),
        tools: ToolsConfig {
            default_timeout_secs: b.tools.default_timeout_secs.or(a.tools.default_timeout_secs),
            bash_max_output_bytes: b.tools.bash_max_output_bytes.or(a.tools.bash_max_output_bytes),
            bash_max_output_lines: b.tools.bash_max_output_lines.or(a.tools.bash_max_output_lines),
        },
        trust: TrustConfig {
            default: b.trust.default.or(a.trust.default),
        },
        logging: LoggingConfig {
            level: b.logging.level.or(a.logging.level),
            file: b.logging.file.or(a.logging.file),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Write;

    /// Build a fresh temp dir; auto-cleaned on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!("nanopi-test-{}", uuid_v7()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, name: &str, content: &str) -> PathBuf {
            let p = self.0.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(content.as_bytes()).unwrap();
            p
        }
        fn _unused(&self) -> HashMap<(), ()> {
            HashMap::new()
        }
    }

    fn uuid_v7() -> String {
        crate::util::uuid::v7().to_string()
    }

    #[test]
    fn builtin_defaults() {
        let c = Config::builtin_defaults();
        assert_eq!(c.default_model, None);
        assert_eq!(c.default_base_url, None);
        assert_eq!(c.api_key_source, "env");
    }

    #[test]
    fn missing_files_use_defaults() {
        let tmp = TempDir::new();
        let c = load_config(tmp.path()).unwrap();
        assert_eq!(c.api_key_source, "env");
        assert_eq!(c.default_model, None);
    }

    #[test]
    fn project_overrides_global() {
        // We can't easily override $HOME for the test, so simulate by
        // creating only a project-local config and checking it's loaded.
        let tmp = TempDir::new();
        tmp.write(
            ".nanopi/config.toml",
            r#"
default_model = "project-model"
default_base_url = "https://project.example/v1"
"#,
        );
        let c = load_config(tmp.path()).unwrap();
        assert_eq!(c.default_model.as_deref(), Some("project-model"));
        assert_eq!(
            c.default_base_url.as_deref(),
            Some("https://project.example/v1")
        );
    }

    #[test]
    fn invalid_toml_is_error() {
        let tmp = TempDir::new();
        tmp.write(
            ".nanopi/config.toml",
            "this is not valid toml = === =",
        );
        let r = load_config(tmp.path());
        assert!(matches!(r, Err(ConfigError::Toml { .. })));
    }

    #[test]
    fn merge_preserves_unset_fields() {
        let a = Config {
            default_model: Some("a-model".into()),
            default_base_url: None,
            api_key_source: "env".into(),
            api_key_file: Some("/tmp/key".into()),
            tools: ToolsConfig {
                default_timeout_secs: Some(10),
                bash_max_output_bytes: None,
                bash_max_output_lines: None,
            },
            trust: TrustConfig::default(),
            logging: LoggingConfig::default(),
        };
        let b = Config {
            default_model: None,
            default_base_url: Some("https://b".into()),
            api_key_source: "env".into(),
            api_key_file: None,
            tools: ToolsConfig::default(),
            trust: TrustConfig::default(),
            logging: LoggingConfig::default(),
        };
        let m = merge(a, b);
        assert_eq!(m.default_model.as_deref(), Some("a-model"));
        assert_eq!(m.default_base_url.as_deref(), Some("https://b"));
        assert_eq!(m.api_key_file.as_deref(), Some(Path::new("/tmp/key")));
    }

    #[test]
    fn merge_lets_b_override_a() {
        let a = Config {
            default_model: Some("a-model".into()),
            ..Config::builtin_defaults()
        };
        let b = Config {
            default_model: Some("b-model".into()),
            ..Config::builtin_defaults()
        };
        let m = merge(a, b);
        assert_eq!(m.default_model.as_deref(), Some("b-model"));
    }
}