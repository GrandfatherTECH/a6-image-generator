//! Optional system-keyring integration for the A6API credential.

use std::collections::HashMap;
use std::env;
use std::path::Path;

use keyring::v1::{Entry, Error as KeyringError};
use thiserror::Error;

use crate::config::{ApiKey, ConfigError};
const KEYRING_SERVICE: &str = "io.github.grandfathertech.A6ImageStudio.key";
const PREVIOUS_KEYRING_SERVICE: &str = "org.a6-studio.key";
const ORIGINAL_KEYRING_SERVICE: &str = "io.github.grandfathertech.a6-image-studio";
const ORIGINAL_KEYRING_USERNAME: &str = "A6API_KEY";

/// Origin of the credential currently active in the desktop application.
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

/// Validated API key together with its precedence source.
#[derive(Clone, Debug)]
pub struct ResolvedApiKey {
    pub key: ApiKey,
    pub source: ApiKeySource,
}

/// Read and validate `A6API_KEY` without consulting the desktop keyring.
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

/// Read the current keyring entry, then compatible legacy entries if absent.
pub fn load_keyring_key() -> Result<Option<ResolvedApiKey>, SecretError> {
    let entry = keyring_entry()?;
    match entry.get_password() {
        Ok(value) => Ok(Some(ResolvedApiKey {
            key: ApiKey::from_secret(value)?,
            source: ApiKeySource::Keyring,
        })),
        Err(KeyringError::NoEntry) => load_legacy_keyring_key(),
        Err(source) => Err(SecretError::Keyring(source)),
    }
}

/// Store a validated credential under the current per-user keyring identity.
///
/// Legacy entries are removed only after the new write succeeds.
pub fn store_keyring_key(value: impl Into<String>) -> Result<ApiKey, SecretError> {
    let key = ApiKey::from_secret(value)?;
    keyring_entry()?
        .set_password(key.expose())
        .map_err(SecretError::Keyring)?;
    remove_legacy_keyring_keys_best_effort();
    Ok(key)
}

/// Remove current and legacy application-owned keyring entries.
///
/// Returns `true` when at least one entry was removed.
pub fn forget_keyring_key() -> Result<bool, SecretError> {
    let removed_current = delete_if_present(&keyring_entry()?)?;
    let removed_previous = delete_if_present(&previous_keyring_entry()?)?;
    let removed_original = delete_if_present(&original_keyring_entry()?)?;
    Ok(removed_current || removed_previous || removed_original)
}

fn load_legacy_keyring_key() -> Result<Option<ResolvedApiKey>, SecretError> {
    for entry in [previous_keyring_entry()?, original_keyring_entry()?] {
        match entry.get_password() {
            Ok(value) => {
                return Ok(Some(ResolvedApiKey {
                    key: ApiKey::from_secret(value)?,
                    source: ApiKeySource::Keyring,
                }));
            }
            Err(KeyringError::NoEntry) => {}
            Err(source) => return Err(SecretError::Keyring(source)),
        }
    }
    Ok(None)
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

fn previous_keyring_entry() -> Result<Entry, SecretError> {
    Entry::new(PREVIOUS_KEYRING_SERVICE, &login_username()?).map_err(SecretError::Keyring)
}

fn original_keyring_entry() -> Result<Entry, SecretError> {
    Entry::new(ORIGINAL_KEYRING_SERVICE, ORIGINAL_KEYRING_USERNAME).map_err(SecretError::Keyring)
}

fn delete_if_present(entry: &Entry) -> Result<bool, SecretError> {
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(KeyringError::NoEntry) => Ok(false),
        Err(source) => Err(SecretError::Keyring(source)),
    }
}

fn remove_legacy_keyring_keys_best_effort() {
    for entry in [previous_keyring_entry(), original_keyring_entry()] {
        let result = entry.and_then(|entry| delete_if_present(&entry));
        if let Err(error) = result {
            tracing::warn!(%error, "new API key was stored, but a legacy keyring entry could not be removed");
        }
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

/// Environment, identity, validation, or keyring failure.
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
            "io.github.grandfathertech.A6ImageStudio.key.grandfathertech"
        );
        assert_eq!(
            keyring_label("studio user"),
            "io.github.grandfathertech.A6ImageStudio.key.studio-user"
        );
    }
}
