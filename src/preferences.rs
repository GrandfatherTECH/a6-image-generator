//! Persistent, non-secret desktop preferences.
//!
//! API credentials never enter this module. The JSON document is deliberately
//! limited to ordinary settings that are safe to keep below XDG_CONFIG_HOME.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{Config, ConfigError, DEFAULT_BASE_URL, DEFAULT_IMAGE_MODEL};
use crate::generation::{ImageQuality, ImageSize, OutputFormat};
use crate::xdg::{AppPaths, XdgPathError};

pub const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 300;
const SETTINGS_FILENAME: &str = "settings.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppPreferences {
    pub base_url: String,
    pub model: String,
    pub default_size: String,
    pub default_quality: String,
    pub default_format: String,
    pub output_directory: PathBuf,
    pub request_timeout_seconds: u64,
    pub retain_history: bool,
}

impl AppPreferences {
    pub fn defaults(paths: &AppPaths) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_IMAGE_MODEL.to_owned(),
            default_size: "1024x1024".to_owned(),
            default_quality: "low".to_owned(),
            default_format: "PNG".to_owned(),
            output_directory: paths.data_dir().join("outputs"),
            request_timeout_seconds: DEFAULT_REQUEST_TIMEOUT_SECONDS,
            retain_history: true,
        }
    }

    pub fn validate(&self) -> Result<(), PreferencesError> {
        Config::validate_endpoint_and_model(&self.base_url, &self.model)?;
        self.default_size
            .parse::<ImageSize>()
            .map_err(|error| PreferencesError::InvalidSetting(error.to_string()))?;
        self.default_quality
            .parse::<ImageQuality>()
            .map_err(|error| PreferencesError::InvalidSetting(error.to_string()))?;
        self.default_format
            .parse::<OutputFormat>()
            .map_err(|error| PreferencesError::InvalidSetting(error.to_string()))?;
        if !self.output_directory.is_absolute() {
            return Err(PreferencesError::InvalidSetting(
                "the default output directory must be an absolute path".to_owned(),
            ));
        }
        if !(10..=1_800).contains(&self.request_timeout_seconds) {
            return Err(PreferencesError::InvalidSetting(
                "request timeout must be between 10 and 1800 seconds".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Default for AppPreferences {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_IMAGE_MODEL.to_owned(),
            default_size: "1024x1024".to_owned(),
            default_quality: "low".to_owned(),
            default_format: "PNG".to_owned(),
            output_directory: PathBuf::from("/tmp/a6-image-studio-outputs"),
            request_timeout_seconds: DEFAULT_REQUEST_TIMEOUT_SECONDS,
            retain_history: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreferencesStore {
    path: PathBuf,
}

impl PreferencesStore {
    pub fn discover() -> Result<Self, PreferencesError> {
        let paths = AppPaths::discover()?;
        Ok(Self {
            path: paths.config_dir().join(SETTINGS_FILENAME),
        })
    }

    pub fn from_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self, defaults: &AppPreferences) -> Result<AppPreferences, PreferencesError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(defaults.clone()),
            Err(source) => {
                return Err(PreferencesError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let mut loaded: AppPreferences =
            serde_json::from_slice(&bytes).map_err(|source| PreferencesError::Json {
                path: self.path.clone(),
                source,
            })?;
        if loaded.output_directory == AppPreferences::default().output_directory {
            loaded.output_directory = defaults.output_directory.clone();
        }
        loaded.validate()?;
        Ok(loaded)
    }

    pub fn save(&self, preferences: &AppPreferences) -> Result<(), PreferencesError> {
        preferences.validate()?;
        let bytes =
            serde_json::to_vec_pretty(preferences).map_err(|source| PreferencesError::Json {
                path: self.path.clone(),
                source,
            })?;
        atomic_write(&self.path, &bytes).map_err(|source| PreferencesError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

#[derive(Debug, Error)]
pub enum PreferencesError {
    #[error(transparent)]
    Xdg(#[from] XdgPathError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("invalid setting: {0}")]
    InvalidSetting(String),
    #[error("failed to access preferences file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid preferences JSON in {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| "settings".into()),
        std::process::id()
    ));
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "a6-image-studio-preferences-{name}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn settings_round_trip_without_any_api_key_field() {
        let path = temporary_path("round-trip.json");
        let store = PreferencesStore::from_path(path.clone());
        let preferences = AppPreferences {
            output_directory: temporary_path("outputs"),
            ..AppPreferences::default()
        };

        store.save(&preferences).expect("preferences should save");
        let contents = fs::read_to_string(&path).expect("settings file should exist");
        assert!(!contents.to_lowercase().contains("api_key"));
        assert!(!contents.contains("sk-"));
        assert_eq!(
            store.load(&preferences).expect("preferences should load"),
            preferences
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rejects_relative_output_directories() {
        let preferences = AppPreferences {
            output_directory: PathBuf::from("relative"),
            ..AppPreferences::default()
        };

        assert!(preferences.validate().is_err());
    }
}
