use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const SUPPORTED_CONFIG_VERSION: u32 = 1;
const MIN_CACHE_SIZE_MB: u64 = 1;
const MAX_CACHE_SIZE_MB: u64 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub storage: StorageConfig,
    pub durability: DurabilityConfig,
    pub logging: LoggingConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub path: PathBuf,
    pub cache_size_mb: u64,
    pub create_if_missing: bool,
    pub read_only: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DurabilityConfig {
    pub sync_on_commit: bool,
    pub page_checksums: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: LogLevel,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let source = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;

        let mut config: Self =
            serde_saphyr::from_str(&source).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source: Box::new(source),
            })?;

        config.validate()?;
        config.resolve_storage_path(path);
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != SUPPORTED_CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion {
                found: self.version,
                supported: SUPPORTED_CONFIG_VERSION,
            });
        }

        if self.storage.path.as_os_str().is_empty() {
            return Err(ConfigError::InvalidValue {
                field: "storage.path",
                reason: "must not be empty".to_owned(),
            });
        }

        if !(MIN_CACHE_SIZE_MB..=MAX_CACHE_SIZE_MB).contains(&self.storage.cache_size_mb) {
            return Err(ConfigError::InvalidValue {
                field: "storage.cache_size_mb",
                reason: format!("must be between {MIN_CACHE_SIZE_MB} and {MAX_CACHE_SIZE_MB} MiB"),
            });
        }

        if self.storage.read_only && self.storage.create_if_missing {
            return Err(ConfigError::InvalidValue {
                field: "storage.create_if_missing",
                reason: "cannot be true when storage.read_only is true".to_owned(),
            });
        }

        Ok(())
    }

    pub fn cache_size_bytes(&self) -> u64 {
        self.storage.cache_size_mb * 1024 * 1024
    }

    fn resolve_storage_path(&mut self, config_path: &Path) {
        if self.storage.path.is_absolute() {
            return;
        }

        let config_directory = config_path.parent().unwrap_or_else(|| Path::new("."));
        self.storage.path = config_directory.join(&self.storage.path);
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: Box<serde_saphyr::Error>,
    },
    UnsupportedVersion {
        found: u32,
        supported: u32,
    },
    InvalidValue {
        field: &'static str,
        reason: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "failed to read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(formatter, "invalid YAML in {}: {source}", path.display())
            }
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "unsupported configuration version {found}; North supports version {supported}"
            ),
            Self::InvalidValue { field, reason } => {
                write!(formatter, "invalid {field}: {reason}")
            }
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source.as_ref()),
            Self::UnsupportedVersion { .. } | Self::InvalidValue { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_CONFIG: &str = r#"
version: 1
storage:
  path: data.north
  cache_size_mb: 64
  create_if_missing: true
  read_only: false
durability:
  sync_on_commit: true
  page_checksums: true
logging:
  level: info
"#;

    fn parse(source: &str) -> Result<Config, serde_saphyr::Error> {
        serde_saphyr::from_str(source)
    }

    #[test]
    fn parses_valid_configuration() {
        let config = parse(VALID_CONFIG).expect("configuration should parse");
        config.validate().expect("configuration should validate");
        assert_eq!(config.cache_size_bytes(), 64 * 1024 * 1024);
        assert_eq!(config.logging.level, LogLevel::Info);
    }

    #[test]
    fn rejects_unknown_fields() {
        let source = VALID_CONFIG.replace(
            "  read_only: false",
            "  read_only: false\n  page_size: 4096",
        );
        assert!(parse(&source).is_err());
    }

    #[test]
    fn rejects_read_only_creation() {
        let source = VALID_CONFIG
            .replace("create_if_missing: true", "create_if_missing: false")
            .replace("read_only: false", "read_only: true");
        let mut config = parse(&source).expect("configuration should parse");
        config.storage.create_if_missing = true;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue {
                field: "storage.create_if_missing",
                ..
            })
        ));
    }
}
