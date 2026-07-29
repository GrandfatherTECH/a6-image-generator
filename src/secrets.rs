//! Optional system-keyring integration for the A6API credential.

use std::collections::HashMap;
use std::env;
use std::path::Path;

use keyring::v1::{Entry, Error as KeyringError};
use thiserror::Error;

use crate::config::{ApiKey, ConfigError};
use crate::xdg::LEGACY_APP_ID;

const KEYRING_SERVICE: &str = "org.a6-studio.key";
const LEGACY_KEYRING_USERNAME: &str = "A6API_KEY";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiKeySource {
    Environment,
    Keyring,
}

impl ApiKeySource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Keyring => "system keyring",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedApiKey {
    pub key: ApiKey,
    pub source: ApiKeySource,
}

pub fn environment_key() -> Result<Option<ResolvedApiKey>, SecretError> {
    match env::var("A6API_KEY") {
        Ok(value) => Ok(Some(ResolvedApiKey {
            key: ApiKey::from_secret(value)?,
            source: ApiKeySource::Environment,
        })),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(SecretError::NonUnicodeEnvironment),
    }
}

pub fn load_keyring_key() -> Result<Option<ResolvedApiKey>, SecretError> {
    let entry = keyring_entry()?;
    match entry.get_password() {
        Ok(value) => Ok(Some(ResolvedApiKey {
            key: ApiKey::from_secret(value)?,
            source: ApiKeySource::Keyring,
        })),
        Err(KeyringError::NoEntry) => migrate_legacy_keyring_key(),
        Err(source) => Err(SecretError::Keyring(source)),
    }
}

pub fn store_keyring_key(value: impl Into<String>) -> Result<ApiKey, SecretError> {
    let key = ApiKey::from_secret(value)?;
    keyring_entry()?
        .set_password(key.expose())
        .map_err(SecretError::Keyring)?;
    remove_legacy_keyring_key_best_effort();
    Ok(key)
}

pub fn forget_keyring_key() -> Result<bool, SecretError> {
    let removed_current = delete_if_present(&keyring_entry()?)?;
    let removed_legacy = delete_if_present(&legacy_keyring_entry()?)?;
    Ok(removed_current || removed_legacy)
}

fn migrate_legacy_keyring_key() -> Result<Option<ResolvedApiKey>, SecretError> {
    let legacy = legacy_keyring_entry()?;
    let value = match legacy.get_password() {
        Ok(value) => value,
        Err(KeyringError::NoEntry) => return Ok(None),
        Err(source) => return Err(SecretError::Keyring(source)),
    };
    let key = ApiKey::from_secret(value)?;
    keyring_entry()?
        .set_password(key.expose())
        .map_err(SecretError::Keyring)?;
    if let Err(error) = legacy.delete_credential()
        && !matches!(error, KeyringError::NoEntry)
    {
        tracing::warn!(%error, "API key migrated, but the legacy keyring entry could not be removed");
    }
    Ok(Some(ResolvedApiKey {
        key,
        source: ApiKeySource::Keyring,
    }))
}

fn keyring_entry() -> Result<Entry, SecretError> {
    let username = login_username()?;
    let label = keyring_label(&username);

    // The v1 constructor initializes keyring-core's platform-native default
    // store. Build the actual entry with a label modifier so KeepSecret and
    // other Secret Service managers show a deliberate, stable name.
    let _initializer = Entry::new(KEYRING_SERVICE, &username).map_err(SecretError::Keyring)?;
    let modifiers = HashMap::from([("label", label.as_str())]);
    let inner = keyring_core::Entry::new_with_modifiers(KEYRING_SERVICE, &username, &modifiers)
        .map_err(SecretError::Keyring)?;
    Ok(Entry { inner })
}

fn legacy_keyring_entry() -> Result<Entry, SecretError> {
    Entry::new(LEGACY_APP_ID, LEGACY_KEYRING_USERNAME).map_err(SecretError::Keyring)
}

fn delete_if_present(entry: &Entry) -> Result<bool, SecretError> {
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(KeyringError::NoEntry) => Ok(false),
        Err(source) => Err(SecretError::Keyring(source)),
    }
}

fn remove_legacy_keyring_key_best_effort() {
    let result = legacy_keyring_entry().and_then(|entry| delete_if_present(&entry));
    if let Err(error) = result {
        tracing::warn!(%error, "new API key was stored, but the legacy keyring entry could not be removed");
    }
}

fn login_username() -> Result<String, SecretError> {
    ["USER", "LOGNAME"]
        .into_iter()
        .find_map(|name| env::var(name).ok().filter(|value| !value.trim().is_empty()))
        .or_else(|| {
            env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .as_deref()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
        })
        .ok_or(SecretError::UsernameUnavailable)
}

fn keyring_label(username: &str) -> String {
    let component = username
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("{KEYRING_SERVICE}.{component}")
}

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("A6API_KEY contains non-Unicode data")]
    NonUnicodeEnvironment,
    #[error("the login username required for the system-keyring identity is unavailable")]
    UsernameUnavailable,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("system keyring error: {0}")]
    Keyring(#[source] KeyringError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_labels_are_explicit() {
        assert_eq!(ApiKeySource::Environment.label(), "environment");
        assert_eq!(ApiKeySource::Keyring.label(), "system keyring");
    }

    #[test]
    fn visible_keyring_label_uses_the_requested_username_shape() {
        assert_eq!(
            keyring_label("grandfathertech"),
            "org.a6-studio.key.grandfathertech"
        );
        assert_eq!(
            keyring_label("studio user"),
            "org.a6-studio.key.studio-user"
        );
    }
}
