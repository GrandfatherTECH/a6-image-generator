//! Validated gateway configuration and non-disclosing API-key handling.
//!
//! [`ApiKey`] intentionally exposes the secret only within this crate. Its
//! `Debug` implementation always redacts the value.

use std::env;
use std::fmt;
use std::time::Duration;

use thiserror::Error;
use url::Url;

/// Default OpenAI-compatible gateway root.
pub const DEFAULT_BASE_URL: &str = "https://api.a6api.com";
/// Default image-generation model identifier.
pub const DEFAULT_IMAGE_MODEL: &str = "gpt-image-2";
/// Default overall timeout for one HTTP attempt.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Validated API credential with redacted formatting.
///
/// The raw value is accessible only within this crate for authorization and
/// explicit redaction. Callers should use [`ApiKey::masked`] for display.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub(crate) fn from_secret(value: impl Into<String>) -> Result<Self, ConfigError> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(ConfigError::MissingApiKey);
        }
        Ok(Self(trimmed.to_owned()))
    }

    pub fn masked(&self) -> String {
        let characters: Vec<char> = self.0.chars().collect();
        if characters.len() <= 4 {
            return "…".to_owned();
        }

        let suffix: String = characters[characters.len() - 4..].iter().collect();
        if characters.len() <= 12 {
            return format!("…{suffix}");
        }

        let prefix: String = characters[..3].iter().collect();
        format!("{prefix}…{suffix}")
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey([REDACTED])")
    }
}

/// Fully validated connection configuration used to build an API client.
#[derive(Clone)]
pub struct Config {
    api_key: ApiKey,
    base_url: Url,
    model: String,
    request_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let api_key = match env::var("A6API_KEY") {
            Ok(value) => value,
            Err(env::VarError::NotPresent) => return Err(ConfigError::MissingApiKey),
            Err(env::VarError::NotUnicode(_)) => {
                return Err(ConfigError::NonUnicodeEnvironment("A6API_KEY"));
            }
        };
        let base_url = read_optional_env("A6API_BASE_URL", DEFAULT_BASE_URL)?;
        let model = read_optional_env("A6API_IMAGE_MODEL", DEFAULT_IMAGE_MODEL)?;
        Self::new(api_key, base_url, model)
    }

    pub fn new(
        api_key: impl Into<String>,
        base_url: impl AsRef<str>,
        model: impl Into<String>,
    ) -> Result<Self, ConfigError> {
        Self::with_timeout(
            ApiKey::from_secret(api_key)?,
            base_url,
            model,
            DEFAULT_REQUEST_TIMEOUT,
        )
    }

    pub fn with_timeout(
        api_key: ApiKey,
        base_url: impl AsRef<str>,
        model: impl Into<String>,
        request_timeout: Duration,
    ) -> Result<Self, ConfigError> {
        let model = model.into();
        let (base_url, model) = Self::validate_endpoint_and_model(base_url.as_ref(), &model)?;
        if request_timeout.is_zero() {
            return Err(ConfigError::InvalidRequestTimeout);
        }

        Ok(Self {
            api_key,
            base_url,
            model,
            request_timeout,
        })
    }

    pub fn validate_endpoint_and_model(
        base_url: &str,
        model: &str,
    ) -> Result<(Url, String), ConfigError> {
        let base_url = normalize_base_url(base_url)?;
        let model = model.trim();
        if model.is_empty() {
            return Err(ConfigError::MissingModel);
        }
        Ok((base_url, model.to_owned()))
    }

    pub fn api_key(&self) -> &ApiKey {
        &self.api_key
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn sanitized_base_url(&self) -> String {
        self.base_url.as_str().trim_end_matches('/').to_owned()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("api_key", &self.api_key)
            .field("base_url", &self.sanitized_base_url())
            .field("model", &self.model)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

/// Invalid or unavailable connection configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("A6API_KEY is required and must not be empty")]
    MissingApiKey,
    #[error("{0} contains non-Unicode data")]
    NonUnicodeEnvironment(&'static str),
    #[error("A6API_IMAGE_MODEL must not be empty")]
    MissingModel,
    #[error("request timeout must be greater than zero")]
    InvalidRequestTimeout,
    #[error("invalid A6API_BASE_URL: {0}")]
    InvalidBaseUrl(String),
}

fn read_optional_env(name: &'static str, default: &str) -> Result<String, ConfigError> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => Ok(default.to_owned()),
        Err(env::VarError::NotUnicode(_)) => Err(ConfigError::NonUnicodeEnvironment(name)),
    }
}

fn normalize_base_url(value: &str) -> Result<Url, ConfigError> {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(ConfigError::InvalidBaseUrl("URL is empty".to_owned()));
    }

    let mut parsed =
        Url::parse(trimmed).map_err(|error| ConfigError::InvalidBaseUrl(error.to_string()))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ConfigError::InvalidBaseUrl(
            "scheme must be http or https".to_owned(),
        ));
    }
    if parsed.host_str().is_none() {
        return Err(ConfigError::InvalidBaseUrl(
            "URL must include a host".to_owned(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ConfigError::InvalidBaseUrl(
            "embedded credentials are not allowed".to_owned(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ConfigError::InvalidBaseUrl(
            "query strings and fragments are not allowed".to_owned(),
        ));
    }
    let normalized_path = parsed.path().trim_end_matches('/').to_owned();
    if normalized_path.is_empty() {
        parsed.set_path("/");
    } else {
        parsed.set_path(&normalized_path);
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_trailing_slashes() {
        let config = Config::new(
            "sk-example-secret",
            " https://api.a6api.com/v1/// ",
            DEFAULT_IMAGE_MODEL,
        )
        .expect("test configuration should be valid");

        assert_eq!(config.sanitized_base_url(), "https://api.a6api.com/v1");
        assert_eq!(config.base_url().path(), "/v1");
    }

    #[test]
    fn masks_api_keys_without_revealing_the_complete_value() {
        let key = ApiKey::from_secret("sk-1234567890abcd").expect("test key should be valid");

        assert_eq!(key.masked(), "sk-…abcd");
        assert!(!format!("{key:?}").contains("1234567890abcd"));
    }

    #[test]
    fn short_api_keys_are_not_disclosed() {
        let key = ApiKey::from_secret("abcd").expect("test key should be valid");

        assert_eq!(key.masked(), "…");
    }

    #[test]
    fn rejects_base_urls_with_credentials() {
        let error = Config::new("secret", "https://user:pass@example.com", "model")
            .expect_err("credentials in the URL must be rejected");

        assert!(error.to_string().contains("embedded credentials"));
    }

    #[test]
    fn preserves_a_custom_request_timeout() {
        let config = Config::with_timeout(
            ApiKey::from_secret("secret").expect("valid key"),
            "https://example.com",
            "model",
            Duration::from_secs(42),
        )
        .expect("configuration should be valid");

        assert_eq!(config.request_timeout(), Duration::from_secs(42));
    }
}
