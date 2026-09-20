//! Transactional SQLite persistence for sessions, generations, and errors.
//!
//! The database intentionally stores metadata only. Generated image bytes and
//! Base64 provider payloads remain outside the database.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::domain::GeneratedImage;
use crate::generation::{
    CompatibilitySettings, GenerationInput, GenerationOptions, ImageBackground, ImageQuality,
    ImageSize, OutputFormat,
};
use crate::xdg::{AppPaths, XdgPathError};

const DATABASE_SCHEMA_VERSION: i64 = 1;
const LEGACY_HISTORY_FILENAME: &str = "history.json";
const LEGACY_ERRORS_FILENAME: &str = "errors.json";
const LEGACY_HISTORY_IMPORT_KEY: &str = "legacy_history_json_v1";
const LEGACY_ERRORS_IMPORT_KEY: &str = "legacy_errors_json_v1";
const LEGACY_DOCUMENT_VERSION: u32 = 1;
const MAX_HISTORY_ENTRIES: usize = 2_000;
const MAX_ERROR_ENTRIES: usize = 1_000;

const HISTORY_SELECT: &str = "
    SELECT id, session_id, timestamp, output_path, prompt, model,
           size, quality, background, output_format,
           send_size, send_quality, send_background, send_output_format,
           width, height, request_id
      FROM generations
     ORDER BY timestamp DESC, rowid DESC
";

const ERROR_SELECT: &str = "
    SELECT id, session_id, timestamp, operation, prompt, model, endpoint,
           category, summary, full_response, response_truncated, request_id
      FROM errors
     ORDER BY timestamp DESC, rowid DESC
";

/// Serializable generation and compatibility settings stored with history.
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

/// Metadata-only record for one accepted generated image.
///
/// The referenced image file is independent and may later be moved or deleted.
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

/// Sanitized, bounded operational error record stored in SQLite.
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
struct LegacyHistoryDocument {
    version: u32,
    entries: Vec<HistoryEntry>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct LegacyErrorDocument {
    version: u32,
    entries: Vec<ErrorLogEntry>,
}

#[derive(Debug)]
struct RepositoryState {
    connection: Connection,
    history: Vec<HistoryEntry>,
    errors: Vec<ErrorLogEntry>,
}

/// Transactional SQLite repository for history and error metadata.
///
/// In-memory indexes are updated only after successful database commits.
#[derive(Clone, Debug)]
pub struct HistoryRepository {
    database_path: PathBuf,
    state: Arc<Mutex<RepositoryState>>,
}

impl HistoryRepository {
    pub fn discover() -> Result<Self, HistoryError> {
        let paths = AppPaths::discover()?;
        Self::open_with_legacy(
            paths.database_path(),
            Some(paths.legacy_data_dir().join(LEGACY_HISTORY_FILENAME)),
            Some(paths.legacy_data_dir().join(LEGACY_ERRORS_FILENAME)),
        )
    }

    pub fn open(database_path: PathBuf) -> Result<Self, HistoryError> {
        Self::open_with_legacy(database_path, None, None)
    }

    pub fn in_memory() -> Result<Self, HistoryError> {
        let database_path = PathBuf::from(":memory:");
        let mut connection = Connection::open_in_memory()
            .map_err(|source| database_error(&database_path, source))?;
        initialize_database(&mut connection, &database_path)?;
        Self::from_connection(database_path, connection)
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn append_history(&self, entry: HistoryEntry) -> Result<(), HistoryError> {
        let mut state = self.lock();
        {
            let transaction = state
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|source| database_error(&self.database_path, source))?;
            upsert_session(&transaction, &entry.session_id, &entry.timestamp)
                .and_then(|()| insert_history(&transaction, &entry, true))
                .and_then(|()| trim_history(&transaction))
                .and_then(|()| transaction.commit())
                .map_err(|source| database_error(&self.database_path, source))?;
        }
        state.history.retain(|existing| existing.id != entry.id);
        state.history.insert(0, entry);
        state.history.truncate(MAX_HISTORY_ENTRIES);
        Ok(())
    }

    pub fn append_error(&self, entry: ErrorLogEntry) -> Result<(), HistoryError> {
        let mut state = self.lock();
        {
            let transaction = state
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|source| database_error(&self.database_path, source))?;
            upsert_session(&transaction, &entry.session_id, &entry.timestamp)
                .and_then(|()| insert_error(&transaction, &entry, true))
                .and_then(|()| trim_errors(&transaction))
                .and_then(|()| transaction.commit())
                .map_err(|source| database_error(&self.database_path, source))?;
        }
        state.errors.retain(|existing| existing.id != entry.id);
        state.errors.insert(0, entry);
        state.errors.truncate(MAX_ERROR_ENTRIES);
        Ok(())
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
        {
            let transaction = state
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|source| database_error(&self.database_path, source))?;
            transaction
                .execute("DELETE FROM generations", [])
                .and_then(|_| remove_empty_sessions(&transaction))
                .and_then(|()| transaction.commit())
                .map_err(|source| database_error(&self.database_path, source))?;
        }
        state.history.clear();
        Ok(())
    }

    pub fn clear_errors(&self) -> Result<(), HistoryError> {
        let mut state = self.lock();
        {
            let transaction = state
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|source| database_error(&self.database_path, source))?;
            transaction
                .execute("DELETE FROM errors", [])
                .and_then(|_| remove_empty_sessions(&transaction))
                .and_then(|()| transaction.commit())
                .map_err(|source| database_error(&self.database_path, source))?;
        }
        state.errors.clear();
        Ok(())
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

    /// Open the database and transactionally import each legacy JSON source at
    /// most once without deleting the recovery copy.
    fn open_with_legacy(
        database_path: PathBuf,
        legacy_history_path: Option<PathBuf>,
        legacy_errors_path: Option<PathBuf>,
    ) -> Result<Self, HistoryError> {
        let parent = database_path.parent().ok_or_else(|| HistoryError::Io {
            path: database_path.clone(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "database path has no parent"),
        })?;
        fs::create_dir_all(parent).map_err(|source| HistoryError::Io {
            path: parent.to_owned(),
            source,
        })?;
        let mut connection = Connection::open(&database_path)
            .map_err(|source| database_error(&database_path, source))?;
        initialize_database(&mut connection, &database_path)?;
        if let Some(path) = legacy_history_path.as_deref() {
            import_legacy_history(&mut connection, &database_path, path)?;
        }
        if let Some(path) = legacy_errors_path.as_deref() {
            import_legacy_errors(&mut connection, &database_path, path)?;
        }
        Self::from_connection(database_path, connection)
    }

    fn from_connection(
        database_path: PathBuf,
        connection: Connection,
    ) -> Result<Self, HistoryError> {
        let history =
            load_history(&connection).map_err(|source| database_error(&database_path, source))?;
        let errors =
            load_errors(&connection).map_err(|source| database_error(&database_path, source))?;
        Ok(Self {
            database_path,
            state: Arc::new(Mutex::new(RepositoryState {
                connection,
                history,
                errors,
            })),
        })
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

/// Search result grouping history entries by application session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistorySession {
    pub id: String,
    pub timestamp: String,
    pub count: usize,
}

/// Create a process-unique, timestamped identifier for one application run.
pub fn new_session_id() -> Result<String, HistoryError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| HistoryError::InvalidSystemTime)?
        .as_millis();
    Ok(format!("session-{millis}-{}", std::process::id()))
}

/// Format an RFC3339 timestamp for the desktop while preserving invalid input.
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

/// Case-insensitive search across user-visible history metadata.
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

/// Case-insensitive search across sanitized error metadata and response text.
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

fn initialize_database(
    connection: &mut Connection,
    database_path: &Path,
) -> Result<(), HistoryError> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .and_then(|()| {
            connection.execute_batch(
                "
                PRAGMA foreign_keys = ON;
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                PRAGMA temp_store = MEMORY;
                ",
            )
        })
        .map_err(|source| database_error(database_path, source))?;
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|source| database_error(database_path, source))?;
    if version > DATABASE_SCHEMA_VERSION {
        return Err(HistoryError::UnsupportedDatabaseVersion {
            path: database_path.to_owned(),
            found: version,
            supported: DATABASE_SCHEMA_VERSION,
        });
    }
    if version == 0 {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| database_error(database_path, source))?;
        transaction
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS app_metadata (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS sessions (
                    id TEXT PRIMARY KEY NOT NULL,
                    started_at TEXT NOT NULL,
                    last_activity_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS generations (
                    id TEXT PRIMARY KEY NOT NULL,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    timestamp TEXT NOT NULL,
                    output_path TEXT NOT NULL,
                    prompt TEXT NOT NULL,
                    model TEXT NOT NULL,
                    size TEXT NOT NULL,
                    quality TEXT NOT NULL,
                    background TEXT NOT NULL,
                    output_format TEXT NOT NULL,
                    send_size INTEGER NOT NULL CHECK (send_size IN (0, 1)),
                    send_quality INTEGER NOT NULL CHECK (send_quality IN (0, 1)),
                    send_background INTEGER NOT NULL CHECK (send_background IN (0, 1)),
                    send_output_format INTEGER NOT NULL CHECK (send_output_format IN (0, 1)),
                    width INTEGER NOT NULL CHECK (width >= 0),
                    height INTEGER NOT NULL CHECK (height >= 0),
                    request_id TEXT
                );

                CREATE TABLE IF NOT EXISTS errors (
                    id TEXT PRIMARY KEY NOT NULL,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    timestamp TEXT NOT NULL,
                    operation TEXT NOT NULL,
                    prompt TEXT,
                    model TEXT NOT NULL,
                    endpoint TEXT NOT NULL,
                    category TEXT NOT NULL,
                    summary TEXT NOT NULL,
                    full_response TEXT,
                    response_truncated INTEGER NOT NULL
                        CHECK (response_truncated IN (0, 1)),
                    request_id TEXT
                );

                CREATE INDEX IF NOT EXISTS generations_session_timestamp_idx
                    ON generations(session_id, timestamp DESC);
                CREATE INDEX IF NOT EXISTS generations_timestamp_idx
                    ON generations(timestamp DESC);
                CREATE INDEX IF NOT EXISTS generations_request_id_idx
                    ON generations(request_id);
                CREATE INDEX IF NOT EXISTS errors_session_timestamp_idx
                    ON errors(session_id, timestamp DESC);
                CREATE INDEX IF NOT EXISTS errors_timestamp_idx
                    ON errors(timestamp DESC);
                CREATE INDEX IF NOT EXISTS errors_request_id_idx
                    ON errors(request_id);
                CREATE INDEX IF NOT EXISTS sessions_activity_idx
                    ON sessions(last_activity_at DESC);

                PRAGMA user_version = 1;
                ",
            )
            .and_then(|()| transaction.commit())
            .map_err(|source| database_error(database_path, source))?;
    }
    Ok(())
}

fn load_history(connection: &Connection) -> rusqlite::Result<Vec<HistoryEntry>> {
    let mut statement = connection.prepare(HISTORY_SELECT)?;
    statement.query_map([], history_from_row)?.collect()
}

fn load_errors(connection: &Connection) -> rusqlite::Result<Vec<ErrorLogEntry>> {
    let mut statement = connection.prepare(ERROR_SELECT)?;
    statement.query_map([], error_from_row)?.collect()
}

fn history_from_row(row: &Row<'_>) -> rusqlite::Result<HistoryEntry> {
    Ok(HistoryEntry {
        id: row.get(0)?,
        session_id: row.get(1)?,
        timestamp: row.get(2)?,
        output_path: PathBuf::from(row.get::<_, String>(3)?),
        prompt: row.get(4)?,
        model: row.get(5)?,
        settings: StoredGenerationSettings {
            size: row.get(6)?,
            quality: row.get(7)?,
            background: row.get(8)?,
            output_format: row.get(9)?,
            send_size: row.get(10)?,
            send_quality: row.get(11)?,
            send_background: row.get(12)?,
            send_output_format: row.get(13)?,
        },
        width: row_u32(row, 14)?,
        height: row_u32(row, 15)?,
        request_id: row.get(16)?,
    })
}

fn error_from_row(row: &Row<'_>) -> rusqlite::Result<ErrorLogEntry> {
    Ok(ErrorLogEntry {
        id: row.get(0)?,
        session_id: row.get(1)?,
        timestamp: row.get(2)?,
        operation: row.get(3)?,
        prompt: row.get(4)?,
        model: row.get(5)?,
        endpoint: row.get(6)?,
        category: row.get(7)?,
        summary: row.get(8)?,
        full_response: row.get(9)?,
        response_truncated: row.get(10)?,
        request_id: row.get(11)?,
    })
}

fn row_u32(row: &Row<'_>, index: usize) -> rusqlite::Result<u32> {
    let value = row.get::<_, i64>(index)?;
    u32::try_from(value).map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(source))
    })
}

fn upsert_session(
    transaction: &Transaction<'_>,
    session_id: &str,
    timestamp: &str,
) -> rusqlite::Result<()> {
    transaction.execute(
        "
        INSERT INTO sessions(id, started_at, last_activity_at)
        VALUES (?1, ?2, ?2)
        ON CONFLICT(id) DO UPDATE SET
            started_at = MIN(started_at, excluded.started_at),
            last_activity_at = MAX(last_activity_at, excluded.last_activity_at)
        ",
        params![session_id, timestamp],
    )?;
    Ok(())
}

fn insert_history(
    transaction: &Transaction<'_>,
    entry: &HistoryEntry,
    replace: bool,
) -> rusqlite::Result<()> {
    let conflict = if replace { "REPLACE" } else { "IGNORE" };
    transaction.execute(
        &format!(
            "
            INSERT OR {conflict} INTO generations(
                id, session_id, timestamp, output_path, prompt, model,
                size, quality, background, output_format,
                send_size, send_quality, send_background, send_output_format,
                width, height, request_id
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17
            )
            "
        ),
        params![
            entry.id,
            entry.session_id,
            entry.timestamp,
            entry.output_path.to_string_lossy(),
            entry.prompt,
            entry.model,
            entry.settings.size,
            entry.settings.quality,
            entry.settings.background,
            entry.settings.output_format,
            entry.settings.send_size,
            entry.settings.send_quality,
            entry.settings.send_background,
            entry.settings.send_output_format,
            i64::from(entry.width),
            i64::from(entry.height),
            entry.request_id,
        ],
    )?;
    Ok(())
}

fn insert_error(
    transaction: &Transaction<'_>,
    entry: &ErrorLogEntry,
    replace: bool,
) -> rusqlite::Result<()> {
    let conflict = if replace { "REPLACE" } else { "IGNORE" };
    transaction.execute(
        &format!(
            "
            INSERT OR {conflict} INTO errors(
                id, session_id, timestamp, operation, prompt, model, endpoint,
                category, summary, full_response, response_truncated, request_id
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12
            )
            "
        ),
        params![
            entry.id,
            entry.session_id,
            entry.timestamp,
            entry.operation,
            entry.prompt,
            entry.model,
            entry.endpoint,
            entry.category,
            entry.summary,
            entry.full_response,
            entry.response_truncated,
            entry.request_id,
        ],
    )?;
    Ok(())
}

fn trim_history(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute(
        "
        DELETE FROM generations
         WHERE id IN (
            SELECT id
              FROM generations
             ORDER BY timestamp DESC, rowid DESC
             LIMIT -1 OFFSET ?1
         )
        ",
        params![MAX_HISTORY_ENTRIES as i64],
    )?;
    remove_empty_sessions(transaction)
}

fn trim_errors(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute(
        "
        DELETE FROM errors
         WHERE id IN (
            SELECT id
              FROM errors
             ORDER BY timestamp DESC, rowid DESC
             LIMIT -1 OFFSET ?1
         )
        ",
        params![MAX_ERROR_ENTRIES as i64],
    )?;
    remove_empty_sessions(transaction)
}

fn remove_empty_sessions(transaction: &Transaction<'_>) -> rusqlite::Result<()> {
    transaction.execute(
        "
        DELETE FROM sessions
         WHERE NOT EXISTS (
            SELECT 1 FROM generations WHERE generations.session_id = sessions.id
         )
           AND NOT EXISTS (
            SELECT 1 FROM errors WHERE errors.session_id = sessions.id
         )
        ",
        [],
    )?;
    Ok(())
}

fn import_legacy_history(
    connection: &mut Connection,
    database_path: &Path,
    legacy_path: &Path,
) -> Result<(), HistoryError> {
    if migration_complete(connection, LEGACY_HISTORY_IMPORT_KEY)
        .map_err(|source| database_error(database_path, source))?
    {
        return Ok(());
    }
    let Some(document) = read_legacy_history(legacy_path)? else {
        mark_migration_complete(connection, LEGACY_HISTORY_IMPORT_KEY)
            .map_err(|source| database_error(database_path, source))?;
        return Ok(());
    };
    validate_legacy_version(legacy_path, document.version)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| database_error(database_path, source))?;
    for entry in &document.entries {
        upsert_session(&transaction, &entry.session_id, &entry.timestamp)
            .and_then(|()| insert_history(&transaction, entry, false))
            .map_err(|source| database_error(database_path, source))?;
    }
    trim_history(&transaction)
        .and_then(|()| mark_migration_complete_tx(&transaction, LEGACY_HISTORY_IMPORT_KEY))
        .and_then(|()| transaction.commit())
        .map_err(|source| database_error(database_path, source))
}

fn import_legacy_errors(
    connection: &mut Connection,
    database_path: &Path,
    legacy_path: &Path,
) -> Result<(), HistoryError> {
    if migration_complete(connection, LEGACY_ERRORS_IMPORT_KEY)
        .map_err(|source| database_error(database_path, source))?
    {
        return Ok(());
    }
    let Some(document) = read_legacy_errors(legacy_path)? else {
        mark_migration_complete(connection, LEGACY_ERRORS_IMPORT_KEY)
            .map_err(|source| database_error(database_path, source))?;
        return Ok(());
    };
    validate_legacy_version(legacy_path, document.version)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| database_error(database_path, source))?;
    for entry in &document.entries {
        upsert_session(&transaction, &entry.session_id, &entry.timestamp)
            .and_then(|()| insert_error(&transaction, entry, false))
            .map_err(|source| database_error(database_path, source))?;
    }
    trim_errors(&transaction)
        .and_then(|()| mark_migration_complete_tx(&transaction, LEGACY_ERRORS_IMPORT_KEY))
        .and_then(|()| transaction.commit())
        .map_err(|source| database_error(database_path, source))
}

fn migration_complete(connection: &Connection, key: &str) -> rusqlite::Result<bool> {
    connection
        .query_row(
            "SELECT value FROM app_metadata WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map(|value| value.as_deref() == Some("complete"))
}

fn mark_migration_complete(connection: &Connection, key: &str) -> rusqlite::Result<()> {
    connection.execute(
        "
        INSERT INTO app_metadata(key, value) VALUES (?1, 'complete')
        ON CONFLICT(key) DO UPDATE SET value = excluded.value
        ",
        params![key],
    )?;
    Ok(())
}

fn mark_migration_complete_tx(transaction: &Transaction<'_>, key: &str) -> rusqlite::Result<()> {
    transaction.execute(
        "
        INSERT INTO app_metadata(key, value) VALUES (?1, 'complete')
        ON CONFLICT(key) DO UPDATE SET value = excluded.value
        ",
        params![key],
    )?;
    Ok(())
}

fn read_legacy_history(path: &Path) -> Result<Option<LegacyHistoryDocument>, HistoryError> {
    read_legacy_document(path)
}

fn read_legacy_errors(path: &Path) -> Result<Option<LegacyErrorDocument>, HistoryError> {
    read_legacy_document(path)
}

fn read_legacy_document<T>(path: &Path) -> Result<Option<T>, HistoryError>
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
        .map_err(|source| HistoryError::LegacyJson {
            path: path.to_owned(),
            source,
        })
}

fn validate_legacy_version(path: &Path, version: u32) -> Result<(), HistoryError> {
    if version == LEGACY_DOCUMENT_VERSION {
        Ok(())
    } else {
        Err(HistoryError::UnsupportedLegacyVersion {
            path: path.to_owned(),
            found: version,
            supported: LEGACY_DOCUMENT_VERSION,
        })
    }
}

fn database_error(path: &Path, source: rusqlite::Error) -> HistoryError {
    HistoryError::Database {
        path: path.to_owned(),
        source,
    }
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

/// Persistence, migration, schema-version, or record-validation failure.
#[derive(Debug, Error)]
pub enum HistoryError {
    #[error(transparent)]
    Xdg(#[from] XdgPathError),
    #[error("failed to access local application data at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("SQLite database operation failed for {path}: {source}")]
    Database {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("legacy history JSON in {path} is invalid: {source}")]
    LegacyJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "SQLite database {path} uses schema version {found}, but this build supports up to {supported}"
    )]
    UnsupportedDatabaseVersion {
        path: PathBuf,
        found: i64,
        supported: i64,
    },
    #[error(
        "legacy history file {path} uses version {found}, but this build supports version {supported}"
    )]
    UnsupportedLegacyVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
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
    use std::time::Duration as StdDuration;

    fn temporary_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "a6-studio-history-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
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
            StdDuration::from_secs(1),
            ResponseMetadata {
                request_id: Some("request-1".to_owned()),
                ..ResponseMetadata::default()
            },
            GenerationInput::new("history prompt", GenerationOptions::default())
                .expect("valid input"),
        )
    }

    fn error_entry() -> ErrorLogEntry {
        ErrorLogEntry::new(
            "session-search",
            "generate",
            Some("failed prompt".to_owned()),
            "model".to_owned(),
            "https://example.invalid".to_owned(),
            "authentication".to_owned(),
            "summary".to_owned(),
            Some(r#"{"diagnostic":"needle"}"#.to_owned()),
            false,
            Some("request-error".to_owned()),
        )
        .expect("error entry")
    }

    #[test]
    fn sqlite_round_trip_contains_metadata_but_no_image_payload_column() {
        let root = temporary_root("round-trip");
        let database_path = root.join("a6-studio.sqlite3");
        let repository = HistoryRepository::open(database_path.clone()).expect("repository opens");
        let image = generated_image(PathBuf::from("/tmp/generated.png"));
        let entry =
            HistoryEntry::from_generated("session-1", "model", &image).expect("entry builds");
        repository
            .append_history(entry.clone())
            .expect("history appends");

        assert_eq!(repository.history(), vec![entry]);
        let header = fs::read(&database_path).expect("database exists");
        assert!(header.starts_with(b"SQLite format 3\0"));
        let connection = Connection::open(&database_path).expect("database opens");
        let generation_columns = connection
            .prepare("PRAGMA table_info(generations)")
            .expect("schema query")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("columns query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("columns");
        assert!(!generation_columns.iter().any(|column| {
            matches!(
                column.as_str(),
                "image_bytes" | "b64_json" | "image_payload"
            )
        }));
        assert_eq!(
            connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .expect("integrity check"),
            "ok"
        );
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .expect("schema version"),
            DATABASE_SCHEMA_VERSION
        );

        drop(connection);
        drop(repository);
        let _ = fs::remove_dir_all(root);
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
    fn sqlite_search_state_covers_sessions_prompts_and_full_error_responses() {
        let root = temporary_root("search");
        let repository = HistoryRepository::open(root.join("a6-studio.sqlite3")).expect("opens");
        let image = generated_image(PathBuf::from("/tmp/generated-search.png"));
        repository
            .append_history(
                HistoryEntry::from_generated("session-search", "model", &image).expect("entry"),
            )
            .expect("history appends");
        let error = error_entry();
        repository
            .append_error(error.clone())
            .expect("error appends");

        assert_eq!(repository.sessions("history prompt").len(), 1);
        assert!(error_matches(&error, "needle"));
        assert!(error_matches(&error, "session-search"));
        assert!(!error_matches(&error, "not present"));

        repository.clear_history().expect("history clears");
        repository.clear_errors().expect("errors clear");
        assert!(repository.history().is_empty());
        assert!(repository.errors().is_empty());
        assert!(repository.database_path().is_file());
        let state = repository.lock();
        assert_eq!(
            state
                .connection
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("session count"),
            0
        );
        drop(state);

        drop(repository);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_json_is_imported_once_without_deleting_the_backup() {
        let root = temporary_root("legacy-import");
        fs::create_dir_all(&root).expect("root");
        let database_path = root.join("a6-studio.sqlite3");
        let history_path = root.join("history.json");
        let errors_path = root.join("errors.json");
        let image = generated_image(PathBuf::from("/tmp/legacy-generated.png"));
        let history_entry =
            HistoryEntry::from_generated("session-legacy", "model", &image).expect("entry");
        let error = error_entry();
        fs::write(
            &history_path,
            serde_json::to_vec_pretty(&LegacyHistoryDocument {
                version: LEGACY_DOCUMENT_VERSION,
                entries: vec![history_entry.clone()],
            })
            .expect("history serializes"),
        )
        .expect("history writes");
        fs::write(
            &errors_path,
            serde_json::to_vec_pretty(&LegacyErrorDocument {
                version: LEGACY_DOCUMENT_VERSION,
                entries: vec![error.clone()],
            })
            .expect("errors serialize"),
        )
        .expect("errors write");

        let repository = HistoryRepository::open_with_legacy(
            database_path.clone(),
            Some(history_path.clone()),
            Some(errors_path.clone()),
        )
        .expect("legacy import");
        assert_eq!(repository.history(), vec![history_entry.clone()]);
        assert_eq!(repository.errors(), vec![error.clone()]);
        drop(repository);

        let reopened = HistoryRepository::open_with_legacy(
            database_path,
            Some(history_path.clone()),
            Some(errors_path.clone()),
        )
        .expect("repository reopens");
        assert_eq!(reopened.history(), vec![history_entry]);
        assert_eq!(reopened.errors(), vec![error]);
        assert!(history_path.is_file());
        assert!(errors_path.is_file());

        drop(reopened);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn future_database_versions_are_rejected_without_modification() {
        let root = temporary_root("future-version");
        fs::create_dir_all(&root).expect("root");
        let database_path = root.join("a6-studio.sqlite3");
        let connection = Connection::open(&database_path).expect("database");
        connection
            .execute_batch("PRAGMA user_version = 999;")
            .expect("version set");
        drop(connection);

        let error = HistoryRepository::open(database_path).expect_err("future schema fails");
        assert!(matches!(
            error,
            HistoryError::UnsupportedDatabaseVersion { found: 999, .. }
        ));

        let _ = fs::remove_dir_all(root);
    }
}
