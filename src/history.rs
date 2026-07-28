//! Persistent successful-generation history and sanitized error logs.
//!
//! The successful history schema intentionally contains metadata only. Image
//! bytes and Base64 responses are never copied into these files.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::domain::GeneratedImage;
use crate::generation::{
    CompatibilitySettings, GenerationInput, GenerationOptions, ImageBackground, ImageQuality,
    ImageSize, OutputFormat,
};
use crate::preferences::atomic_write;
use crate::xdg::{AppPaths, XdgPathError};

const HISTORY_FILENAME: &str = "history.json";
const ERRORS_FILENAME: &str = "errors.json";
const HISTORY_VERSION: u32 = 1;
const MAX_HISTORY_ENTRIES: usize = 2_000;
const MAX_ERROR_ENTRIES: usize = 1_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGenerationSettings {
    pub size: String,
    pub quality: String,
    pub background: String,
    pub output_format: String,
    pub send_size: bool,
    pub send_quality: bool,
    pub send_background: bool,
    pub send_output_format: bool,
}

impl StoredGenerationSettings {
    pub fn from_options(options: &GenerationOptions) -> Self {
        Self {
            size: options.size.to_string(),
            quality: options.quality.to_string(),
            background: options.background.to_string(),
            output_format: options.output_format.to_string(),
            send_size: options.compatibility.send_size,
            send_quality: options.compatibility.send_quality,
            send_background: options.compatibility.send_background,
            send_output_format: options.compatibility.send_output_format,
        }
    }

    pub fn to_input(&self, prompt: &str) -> Result<GenerationInput, HistoryError> {
        let options = GenerationOptions {
            size: self
                .size
                .parse::<ImageSize>()
                .map_err(|error| HistoryError::InvalidEntry(error.to_string()))?,
            quality: self
                .quality
                .parse::<ImageQuality>()
                .map_err(|error| HistoryError::InvalidEntry(error.to_string()))?,
            background: self
                .background
                .parse::<ImageBackground>()
                .map_err(|error| HistoryError::InvalidEntry(error.to_string()))?,
            output_format: self
                .output_format
                .parse::<OutputFormat>()
                .map_err(|error| HistoryError::InvalidEntry(error.to_string()))?,
            compatibility: CompatibilitySettings {
                send_size: self.send_size,
                send_quality: self.send_quality,
                send_background: self.send_background,
                send_output_format: self.send_output_format,
            },
        };
        GenerationInput::new(prompt, options)
            .map_err(|error| HistoryError::InvalidEntry(error.to_string()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub session_id: String,
    pub timestamp: String,
    pub output_path: PathBuf,
    pub prompt: String,
    pub model: String,
    pub settings: StoredGenerationSettings,
    pub width: u32,
    pub height: u32,
    pub request_id: Option<String>,
}

impl HistoryEntry {
    pub fn from_generated(
        session_id: &str,
        model: &str,
        image: &GeneratedImage,
    ) -> Result<Self, HistoryError> {
        let timestamp = current_timestamp()?;
        Ok(Self {
            id: entry_id(session_id, &timestamp, image.path()),
            session_id: session_id.to_owned(),
            timestamp,
            output_path: image.path().to_owned(),
            prompt: image.request().prompt().to_owned(),
            model: model.to_owned(),
            settings: StoredGenerationSettings::from_options(image.request().options()),
            width: image.width(),
            height: image.height(),
            request_id: image.response_metadata().request_id.clone(),
        })
    }

    pub fn image_exists(&self) -> bool {
        self.output_path.is_file()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorLogEntry {
    pub id: String,
    pub session_id: String,
    pub timestamp: String,
    pub operation: String,
    pub prompt: Option<String>,
    pub model: String,
    pub endpoint: String,
    pub category: String,
    pub summary: String,
    pub full_response: Option<String>,
    pub response_truncated: bool,
    pub request_id: Option<String>,
}

impl ErrorLogEntry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: &str,
        operation: &str,
        prompt: Option<String>,
        model: String,
        endpoint: String,
        category: String,
        summary: String,
        full_response: Option<String>,
        response_truncated: bool,
        request_id: Option<String>,
    ) -> Result<Self, HistoryError> {
        let timestamp = current_timestamp()?;
        Ok(Self {
            id: format!("{session_id}-error-{timestamp}-{}", std::process::id()),
            session_id: session_id.to_owned(),
            timestamp,
            operation: operation.to_owned(),
            prompt,
            model,
            endpoint,
            category,
            summary,
            full_response,
            response_truncated,
            request_id,
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct HistoryDocument {
    version: u32,
    entries: Vec<HistoryEntry>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ErrorDocument {
    version: u32,
    entries: Vec<ErrorLogEntry>,
}

#[derive(Clone, Debug)]
struct RepositoryState {
    history: Vec<HistoryEntry>,
    errors: Vec<ErrorLogEntry>,
}

#[derive(Clone, Debug)]
pub struct HistoryRepository {
    history_path: PathBuf,
    errors_path: PathBuf,
    state: Arc<Mutex<RepositoryState>>,
}

impl HistoryRepository {
    pub fn discover() -> Result<Self, HistoryError> {
        let paths = AppPaths::discover()?;
        Self::open(
            paths.data_dir().join(HISTORY_FILENAME),
            paths.data_dir().join(ERRORS_FILENAME),
        )
    }

    pub fn open(history_path: PathBuf, errors_path: PathBuf) -> Result<Self, HistoryError> {
        let history = load_document::<HistoryDocument>(&history_path)?
            .map(|document| document.entries)
            .unwrap_or_default();
        let errors = load_document::<ErrorDocument>(&errors_path)?
            .map(|document| document.entries)
            .unwrap_or_default();
        Ok(Self {
            history_path,
            errors_path,
            state: Arc::new(Mutex::new(RepositoryState { history, errors })),
        })
    }

    pub fn empty(history_path: PathBuf, errors_path: PathBuf) -> Self {
        Self {
            history_path,
            errors_path,
            state: Arc::new(Mutex::new(RepositoryState {
                history: Vec::new(),
                errors: Vec::new(),
            })),
        }
    }

    pub fn append_history(&self, entry: HistoryEntry) -> Result<(), HistoryError> {
        let mut state = self.lock();
        state.history.insert(0, entry);
        state.history.truncate(MAX_HISTORY_ENTRIES);
        save_history(&self.history_path, &state.history)
    }

    pub fn append_error(&self, entry: ErrorLogEntry) -> Result<(), HistoryError> {
        let mut state = self.lock();
        state.errors.insert(0, entry);
        state.errors.truncate(MAX_ERROR_ENTRIES);
        save_errors(&self.errors_path, &state.errors)
    }

    pub fn history(&self) -> Vec<HistoryEntry> {
        self.lock().history.clone()
    }

    pub fn errors(&self) -> Vec<ErrorLogEntry> {
        self.lock().errors.clone()
    }

    pub fn history_entry(&self, id: &str) -> Option<HistoryEntry> {
        self.lock()
            .history
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
    }

    pub fn error_entry(&self, id: &str) -> Option<ErrorLogEntry> {
        self.lock()
            .errors
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
    }

    pub fn clear_history(&self) -> Result<(), HistoryError> {
        let mut state = self.lock();
        state.history.clear();
        remove_or_write_empty(&self.history_path)
    }

    pub fn clear_errors(&self) -> Result<(), HistoryError> {
        let mut state = self.lock();
        state.errors.clear();
        remove_or_write_empty(&self.errors_path)
    }

    pub fn sessions(&self, query: &str) -> Vec<HistorySession> {
        let query = query.trim().to_lowercase();
        let state = self.lock();
        let mut grouped = BTreeMap::<String, Vec<&HistoryEntry>>::new();
        for entry in &state.history {
            if query.is_empty() || history_matches(entry, &query) {
                grouped
                    .entry(entry.session_id.clone())
                    .or_default()
                    .push(entry);
            }
        }
        let mut sessions = grouped
            .into_iter()
            .map(|(id, entries)| HistorySession {
                id,
                timestamp: entries
                    .iter()
                    .map(|entry| entry.timestamp.as_str())
                    .max()
                    .unwrap_or_default()
                    .to_owned(),
                count: entries.len(),
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
        sessions
    }

    fn lock(&self) -> MutexGuard<'_, RepositoryState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                tracing::error!("history repository lock was poisoned; recovering");
                poisoned.into_inner()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistorySession {
    pub id: String,
    pub timestamp: String,
    pub count: usize,
}

pub fn new_session_id() -> Result<String, HistoryError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| HistoryError::InvalidSystemTime)?
        .as_millis();
    Ok(format!("session-{millis}-{}", std::process::id()))
}

pub fn display_timestamp(timestamp: &str) -> String {
    OffsetDateTime::parse(timestamp, &Rfc3339)
        .ok()
        .and_then(|value| {
            value
                .format(time::macros::format_description!(
                    "[year]-[month]-[day] [hour]:[minute]:[second] UTC"
                ))
                .ok()
        })
        .unwrap_or_else(|| timestamp.to_owned())
}

pub fn history_matches(entry: &HistoryEntry, query: &str) -> bool {
    [
        entry.session_id.as_str(),
        entry.timestamp.as_str(),
        entry.prompt.as_str(),
        entry.model.as_str(),
        entry.output_path.to_string_lossy().as_ref(),
        entry.request_id.as_deref().unwrap_or_default(),
    ]
    .iter()
    .any(|value| value.to_lowercase().contains(query))
}

pub fn error_matches(entry: &ErrorLogEntry, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || [
            entry.session_id.as_str(),
            entry.timestamp.as_str(),
            entry.operation.as_str(),
            entry.prompt.as_deref().unwrap_or_default(),
            entry.model.as_str(),
            entry.endpoint.as_str(),
            entry.category.as_str(),
            entry.summary.as_str(),
            entry.full_response.as_deref().unwrap_or_default(),
            entry.request_id.as_deref().unwrap_or_default(),
        ]
        .iter()
        .any(|value| value.to_lowercase().contains(&query))
}

fn current_timestamp() -> Result<String, HistoryError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|error| HistoryError::InvalidEntry(error.to_string()))
}

fn entry_id(session_id: &str, timestamp: &str, path: &Path) -> String {
    format!(
        "{session_id}-{timestamp}-{}",
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default()
    )
}

fn load_document<T>(path: &Path) -> Result<Option<T>, HistoryError>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(HistoryError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| HistoryError::Json {
            path: path.to_owned(),
            source,
        })
}

fn save_history(path: &Path, entries: &[HistoryEntry]) -> Result<(), HistoryError> {
    save_document(
        path,
        &HistoryDocument {
            version: HISTORY_VERSION,
            entries: entries.to_vec(),
        },
    )
}

fn save_errors(path: &Path, entries: &[ErrorLogEntry]) -> Result<(), HistoryError> {
    save_document(
        path,
        &ErrorDocument {
            version: HISTORY_VERSION,
            entries: entries.to_vec(),
        },
    )
}

fn save_document(path: &Path, document: &impl Serialize) -> Result<(), HistoryError> {
    let bytes = serde_json::to_vec_pretty(document).map_err(|source| HistoryError::Json {
        path: path.to_owned(),
        source,
    })?;
    atomic_write(path, &bytes).map_err(|source| HistoryError::Io {
        path: path.to_owned(),
        source,
    })
}

fn remove_or_write_empty(path: &Path) -> Result<(), HistoryError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(HistoryError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error(transparent)]
    Xdg(#[from] XdgPathError),
    #[error("failed to access local history file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid history JSON in {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid history entry: {0}")]
    InvalidEntry(String),
    #[error("system clock is earlier than the Unix epoch")]
    InvalidSystemTime,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ResponseMetadata;
    use crate::domain::PersistedImage;
    use std::time::Duration;

    fn paths(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "a6-image-studio-history-{name}-{}",
            std::process::id()
        ));
        (
            base.with_extension("history.json"),
            base.with_extension("errors.json"),
        )
    }

    fn generated_image(path: PathBuf) -> GeneratedImage {
        GeneratedImage::new(
            PersistedImage {
                path,
                width: 1024,
                height: 1024,
                file_size: 12,
                output_format: OutputFormat::Png,
            },
            Duration::from_secs(1),
            ResponseMetadata {
                request_id: Some("request-1".to_owned()),
                ..ResponseMetadata::default()
            },
            GenerationInput::new("history prompt", GenerationOptions::default())
                .expect("valid input"),
        )
    }

    #[test]
    fn history_round_trip_contains_metadata_but_no_image_payload() {
        let (history_path, errors_path) = paths("round-trip");
        let repository =
            HistoryRepository::open(history_path.clone(), errors_path).expect("repository opens");
        let image = generated_image(PathBuf::from("/tmp/generated.png"));
        let entry =
            HistoryEntry::from_generated("session-1", "model", &image).expect("entry builds");
        repository
            .append_history(entry.clone())
            .expect("history appends");

        let contents = fs::read_to_string(&history_path).expect("history exists");
        assert!(!contents.contains("b64_json"));
        assert!(!contents.contains("image_bytes"));
        assert_eq!(repository.history(), vec![entry]);
        let _ = fs::remove_file(history_path);
    }

    #[test]
    fn missing_output_is_reported_without_losing_the_prompt() {
        let image = generated_image(PathBuf::from("/definitely/missing/generated.png"));
        let entry =
            HistoryEntry::from_generated("session-1", "model", &image).expect("entry builds");

        assert!(!entry.image_exists());
        assert_eq!(entry.prompt, "history prompt");
        assert!(entry.settings.to_input(&entry.prompt).is_ok());
    }

    #[test]
    fn search_covers_sessions_prompts_and_full_error_responses() {
        let (history_path, errors_path) = paths("search");
        let repository =
            HistoryRepository::open(history_path.clone(), errors_path.clone()).expect("opens");
        let image = generated_image(PathBuf::from("/tmp/generated-search.png"));
        repository
            .append_history(
                HistoryEntry::from_generated("session-search", "model", &image)
                    .expect("entry builds"),
            )
            .expect("history appends");
        let error = ErrorLogEntry::new(
            "session-search",
            "generation",
            Some("history prompt".to_owned()),
            "model".to_owned(),
            "https://example.com".to_owned(),
            "authentication".to_owned(),
            "request failed".to_owned(),
            Some("provider diagnostic needle".to_owned()),
            false,
            Some("request-search".to_owned()),
        )
        .expect("error builds");
        repository
            .append_error(error.clone())
            .expect("error appends");

        assert_eq!(repository.sessions("history prompt").len(), 1);
        assert!(error_matches(&error, "diagnostic needle"));
        assert!(error_matches(&error, "session-search"));
        assert!(!error_matches(&error, "not present"));

        repository.clear_history().expect("history clears");
        repository.clear_errors().expect("errors clear");
        assert!(!history_path.exists());
        assert!(!errors_path.exists());
    }
}
