use std::env;
use std::fmt;

use thiserror::Error;
use url::Url;

pub const DEFAULT_BASE_URL: &str = "https://api.a6api.com";
pub const DEFAULT_IMAGE_MODEL: &str = "gpt-image-2";

#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    fn new(value: impl Into<String>) -> Result<Self, ConfigError> {
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

#[derive(Clone)]
pub struct Config {
    api_key: ApiKey,
    base_url: Url,
    model: String,
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
        let api_key = ApiKey::new(api_key)?;
        let base_url = normalize_base_url(base_url.as_ref())?;
        let model = model.into();
        let model = model.trim();
        if model.is_empty() {
            return Err(ConfigError::MissingModel);
        }

        Ok(Self {
            api_key,
            base_url,
            model: model.to_owned(),
        })
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
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("api_key", &self.api_key)
            .field("base_url", &self.sanitized_base_url())
            .field("model", &self.model)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("A6API_KEY is required and must not be empty")]
    MissingApiKey,
    #[error("{0} contains non-Unicode data")]
    NonUnicodeEnvironment(&'static str),
    #[error("A6API_IMAGE_MODEL must not be empty")]
    MissingModel,
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
        let key = ApiKey::new("sk-1234567890abcd").expect("test key should be valid");

        assert_eq!(key.masked(), "sk-…abcd");
        assert!(!format!("{key:?}").contains("1234567890abcd"));
    }

    #[test]
    fn short_api_keys_are_not_disclosed() {
        let key = ApiKey::new("abcd").expect("test key should be valid");

        assert_eq!(key.masked(), "…");
    }

    #[test]
    fn rejects_base_urls_with_credentials() {
        let error = Config::new("secret", "https://user:pass@example.com", "model")
            .expect_err("credentials in the URL must be rejected");

        assert!(error.to_string().contains("embedded credentials"));
    }
}
