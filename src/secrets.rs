//! Optional system-keyring integration for the A6API credential.

use std::env;

use keyring::v1::{Entry, Error as KeyringError};
use thiserror::Error;

use crate::config::{ApiKey, ConfigError};
use crate::xdg::APP_ID;

const KEYRING_USERNAME: &str = "A6API_KEY";

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
        Err(KeyringError::NoEntry) => Ok(None),
        Err(source) => Err(SecretError::Keyring(source)),
    }
}

pub fn store_keyring_key(value: impl Into<String>) -> Result<ApiKey, SecretError> {
    let key = ApiKey::from_secret(value)?;
    keyring_entry()?
        .set_password(key.expose())
        .map_err(SecretError::Keyring)?;
    Ok(key)
}

pub fn forget_keyring_key() -> Result<bool, SecretError> {
    let entry = keyring_entry()?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(KeyringError::NoEntry) => Ok(false),
        Err(source) => Err(SecretError::Keyring(source)),
    }
}

fn keyring_entry() -> Result<Entry, SecretError> {
    Entry::new(APP_ID, KEYRING_USERNAME).map_err(SecretError::Keyring)
}

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("A6API_KEY contains non-Unicode data")]
    NonUnicodeEnvironment,
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
}
