use std::error::Error as StdError;

use reqwest::StatusCode;
use thiserror::Error;

use super::ResponseMetadata;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkFailure {
    Dns,
    Tls,
    Timeout,
    Connection,
    Request,
}

impl NetworkFailure {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dns => "dns failure",
            Self::Tls => "tls failure",
            Self::Timeout => "timeout",
            Self::Connection => "connection failure",
            Self::Request => "network request failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpFailure {
    Authentication,
    RateLimited,
    Server,
    Client,
}

impl HttpFailure {
    pub fn label(self) -> &'static str {
        match self {
            Self::Authentication => "authentication rejection",
            Self::RateLimited => "rate limiting",
            Self::Server => "gateway/server error",
            Self::Client => "http client error",
        }
    }
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("{kind:?}: {source}")]
    Transport {
        kind: NetworkFailure,
        #[source]
        source: reqwest::Error,
    },
    #[error("models endpoint returned HTTP {status}: {message}")]
    UnsupportedEndpoint {
        status: StatusCode,
        message: String,
        body: String,
        body_truncated: bool,
        metadata: Box<ResponseMetadata>,
    },
    #[error("HTTP {status}: {message}")]
    Http {
        status: StatusCode,
        kind: HttpFailure,
        message: String,
        body: String,
        body_truncated: bool,
        metadata: Box<ResponseMetadata>,
    },
    #[error("invalid JSON response: {message}; body: {body}")]
    InvalidJson { message: String, body: String },
    #[error("image response did not contain b64_json or url data")]
    MissingImageData,
    #[error("invalid Base64 image data: {0}")]
    InvalidBase64(#[from] base64::DecodeError),
    #[error("invalid image URL in API response: {0}")]
    InvalidImageUrl(#[from] url::ParseError),
    #[error("unsupported image URL scheme: {0}")]
    UnsupportedImageUrlScheme(String),
    #[error("response body exceeds the {limit} byte safety limit")]
    ResponseTooLarge { limit: usize },
    #[error("cannot construct an endpoint from the configured base URL")]
    EndpointConstruction,
}

impl ApiError {
    pub fn category(&self) -> &'static str {
        match self {
            Self::Transport { kind, .. } => kind.label(),
            Self::UnsupportedEndpoint { .. } => "unsupported model-list endpoint",
            Self::Http { kind, .. } => kind.label(),
            Self::InvalidJson { .. }
            | Self::MissingImageData
            | Self::InvalidBase64(_)
            | Self::InvalidImageUrl(_)
            | Self::UnsupportedImageUrlScheme(_)
            | Self::ResponseTooLarge { .. }
            | Self::EndpointConstruction => "invalid gateway response",
        }
    }

    pub fn metadata(&self) -> Option<&ResponseMetadata> {
        match self {
            Self::UnsupportedEndpoint { metadata, .. } | Self::Http { metadata, .. } => {
                Some(metadata.as_ref())
            }
            _ => None,
        }
    }

    /// Returns the sanitized provider response body for non-successful HTTP
    /// responses. Successful image payloads, including Base64 data, are never
    /// exposed through this method.
    pub fn full_response(&self) -> Option<(&str, bool)> {
        match self {
            Self::UnsupportedEndpoint {
                body,
                body_truncated,
                ..
            }
            | Self::Http {
                body,
                body_truncated,
                ..
            } => Some((body, *body_truncated)),
            _ => None,
        }
    }
}

pub(crate) fn classify_transport(error: reqwest::Error) -> ApiError {
    let kind = if error.is_timeout() {
        NetworkFailure::Timeout
    } else {
        let mut details = error.to_string().to_lowercase();
        let mut source = error.source();
        while let Some(cause) = source {
            details.push(' ');
            details.push_str(&cause.to_string().to_lowercase());
            source = cause.source();
        }

        if details.contains("dns")
            || details.contains("name resolution")
            || details.contains("failed to lookup address")
        {
            NetworkFailure::Dns
        } else if details.contains("tls")
            || details.contains("certificate")
            || details.contains("rustls")
        {
            NetworkFailure::Tls
        } else if error.is_connect() {
            NetworkFailure::Connection
        } else {
            NetworkFailure::Request
        }
    };

    ApiError::Transport {
        kind,
        source: error,
    }
}
