use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::HeaderMap;
use reqwest::{Client, Response, StatusCode};
use url::Url;

use crate::config::Config;

use super::ApiError;
use super::error::{HttpFailure, classify_transport};
use super::types::{
    ApiErrorEnvelope, GeneratedImage, ImageGenerationRequest, ImageGenerationResponse, ModelsCheck,
    ModelsResponse, ResponseMetadata,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_RESPONSE_BYTES: usize = 80 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
const MAX_ERROR_BODY_CHARS: usize = 4_096;

#[derive(Clone, Debug)]
pub struct ApiClient {
    config: Config,
    http: Client,
}

impl ApiClient {
    pub fn new(config: Config) -> Result<Self, ApiError> {
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("a6-image-studio/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(classify_transport)?;
        Ok(Self { config, http })
    }

    pub async fn check_models(&self) -> Result<ModelsCheck, ApiError> {
        let endpoint = endpoint_url(self.config.base_url(), "models")?;
        let response = self
            .http
            .get(endpoint)
            .bearer_auth(self.config.api_key().expose())
            .send()
            .await
            .map_err(classify_transport)?;
        let status = response.status();
        let metadata = response_metadata(response.headers());

        if !status.is_success() {
            return Err(self.http_error(response, true).await);
        }

        let body = read_limited_body(response).await?;
        let models: ModelsResponse =
            serde_json::from_slice(&body).map_err(|error| ApiError::InvalidJson {
                message: error.to_string(),
                body: sanitize_body(
                    &String::from_utf8_lossy(&body),
                    self.config.api_key().expose(),
                ),
            })?;

        Ok(ModelsCheck {
            http_status: status.as_u16(),
            model_present: models
                .data
                .iter()
                .any(|model| model.id == self.config.model()),
            metadata,
        })
    }

    pub async fn generate_image(
        &self,
        request: &ImageGenerationRequest<'_>,
    ) -> Result<GeneratedImage, ApiError> {
        let endpoint = endpoint_url(self.config.base_url(), "images/generations")?;
        let response = self
            .http
            .post(endpoint)
            .bearer_auth(self.config.api_key().expose())
            .json(request)
            .send()
            .await
            .map_err(classify_transport)?;
        if !response.status().is_success() {
            return Err(self.http_error(response, false).await);
        }

        let metadata = response_metadata(response.headers());
        let body = read_limited_body(response).await?;
        match parse_image_response(&body, self.config.api_key().expose())? {
            ImageSource::Bytes(bytes) => Ok(GeneratedImage { bytes, metadata }),
            ImageSource::Url(url) => {
                let bytes = self.download_image(url).await?;
                Ok(GeneratedImage { bytes, metadata })
            }
        }
    }

    async fn download_image(&self, url: Url) -> Result<Vec<u8>, ApiError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ApiError::UnsupportedImageUrlScheme(url.scheme().to_owned()));
        }

        // The gateway bearer token is intentionally not sent to third-party image URLs.
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(classify_transport)?;
        if !response.status().is_success() {
            return Err(self.http_error(response, false).await);
        }
        read_limited_body(response).await
    }

    async fn http_error(&self, response: Response, model_list: bool) -> ApiError {
        let status = response.status();
        let metadata = response_metadata(response.headers());
        let raw_body = read_error_body(response).await;
        let body = sanitize_body(&raw_body, self.config.api_key().expose());
        let message = structured_error_message(&raw_body)
            .map(|message| sanitize_body(&message, self.config.api_key().expose()))
            .unwrap_or_else(|| body.clone());

        if model_list
            && matches!(
                status,
                StatusCode::NOT_FOUND
                    | StatusCode::METHOD_NOT_ALLOWED
                    | StatusCode::NOT_IMPLEMENTED
            )
        {
            return ApiError::UnsupportedEndpoint {
                status,
                message,
                metadata: Box::new(metadata),
            };
        }

        let kind = match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => HttpFailure::Authentication,
            StatusCode::TOO_MANY_REQUESTS => HttpFailure::RateLimited,
            status if status.is_server_error() => HttpFailure::Server,
            _ => HttpFailure::Client,
        };
        ApiError::Http {
            status,
            kind,
            message,
            body,
            metadata: Box::new(metadata),
        }
    }
}

#[derive(Debug)]
enum ImageSource {
    Bytes(Vec<u8>),
    Url(Url),
}

fn parse_image_response(body: &[u8], api_key: &str) -> Result<ImageSource, ApiError> {
    let response: ImageGenerationResponse =
        serde_json::from_slice(body).map_err(|error| ApiError::InvalidJson {
            message: error.to_string(),
            body: sanitize_body(&String::from_utf8_lossy(body), api_key),
        })?;

    if let Some(encoded) = response
        .data
        .iter()
        .find_map(|image| image.b64_json.as_deref())
    {
        return STANDARD
            .decode(encoded)
            .map(ImageSource::Bytes)
            .map_err(ApiError::InvalidBase64);
    }
    if let Some(url) = response.data.iter().find_map(|image| image.url.as_deref()) {
        return Url::parse(url)
            .map(ImageSource::Url)
            .map_err(ApiError::InvalidImageUrl);
    }
    Err(ApiError::MissingImageData)
}

fn endpoint_url(base_url: &Url, endpoint: &str) -> Result<Url, ApiError> {
    let mut url = base_url.clone();
    let has_v1_suffix = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        == Some("v1");
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| ApiError::EndpointConstruction)?;
    segments.pop_if_empty();
    if !has_v1_suffix {
        segments.push("v1");
    }
    for segment in endpoint.split('/').filter(|segment| !segment.is_empty()) {
        segments.push(segment);
    }
    drop(segments);
    Ok(url)
}

async fn read_limited_body(response: Response) -> Result<Vec<u8>, ApiError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(ApiError::ResponseTooLarge {
            limit: MAX_RESPONSE_BYTES,
        });
    }
    let bytes = response.bytes().await.map_err(classify_transport)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(ApiError::ResponseTooLarge {
            limit: MAX_RESPONSE_BYTES,
        });
    }
    Ok(bytes.to_vec())
}

async fn read_error_body(mut response: Response) -> String {
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(MAX_ERROR_BODY_BYTES as u64) as usize,
    );
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
                body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if chunk.len() >= remaining {
                    break;
                }
            }
            Ok(None) => break,
            Err(error) if body.is_empty() => {
                return format!("unable to read error response body: {error}");
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&body).into_owned()
}

fn response_metadata(headers: &HeaderMap) -> ResponseMetadata {
    ResponseMetadata {
        request_id: first_header(
            headers,
            &["x-request-id", "request-id", "openai-request-id", "cf-ray"],
        ),
        retry_after: first_header(headers, &["retry-after"]),
        rate_limit_limit: first_header(headers, &["x-ratelimit-limit-requests", "ratelimit-limit"]),
        rate_limit_remaining: first_header(
            headers,
            &["x-ratelimit-remaining-requests", "ratelimit-remaining"],
        ),
        rate_limit_reset: first_header(headers, &["x-ratelimit-reset-requests", "ratelimit-reset"]),
    }
}

fn first_header(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    })
}

fn structured_error_message(body: &str) -> Option<String> {
    let parsed: ApiErrorEnvelope = serde_json::from_str(body).ok()?;
    let mut message = parsed.error.message;
    if let Some(kind) = parsed.error.kind {
        message.push_str(&format!(" (type: {kind})"));
    }
    if let Some(code) = parsed.error.code {
        message.push_str(&format!(" (code: {code})"));
    }
    if let Some(param) = parsed.error.param {
        message.push_str(&format!(" (param: {param})"));
    }
    Some(message)
}

fn sanitize_body(body: &str, api_key: &str) -> String {
    let redacted = body.replace(api_key, "[REDACTED]");
    let cleaned: String = redacted
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .take(MAX_ERROR_BODY_CHARS)
        .collect();
    if redacted.chars().count() > MAX_ERROR_BODY_CHARS {
        format!("{cleaned}…")
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_endpoints_with_or_without_v1() {
        let without_v1 = Url::parse("https://api.example.com").expect("valid test URL");
        let with_v1 = Url::parse("https://api.example.com/v1").expect("valid test URL");

        assert_eq!(
            endpoint_url(&without_v1, "images/generations")
                .expect("endpoint should be constructed")
                .as_str(),
            "https://api.example.com/v1/images/generations"
        );
        assert_eq!(
            endpoint_url(&with_v1, "models")
                .expect("endpoint should be constructed")
                .as_str(),
            "https://api.example.com/v1/models"
        );
    }

    #[test]
    fn parses_base64_response() {
        let source = parse_image_response(br#"{"data":[{"b64_json":"aW1hZ2U="}]}"#, "secret")
            .expect("response should parse");

        assert!(matches!(source, ImageSource::Bytes(bytes) if bytes == b"image"));
    }

    #[test]
    fn prefers_base64_to_url_output() {
        let source = parse_image_response(
            br#"{"data":[{"url":"https://example.com/image.png","b64_json":"aW1hZ2U="}]}"#,
            "secret",
        )
        .expect("response should parse");

        assert!(matches!(source, ImageSource::Bytes(_)));
    }

    #[test]
    fn parses_url_response() {
        let source = parse_image_response(
            br#"{"data":[{"url":"https://example.com/image.png"}]}"#,
            "secret",
        )
        .expect("response should parse");

        assert!(
            matches!(source, ImageSource::Url(url) if url.as_str() == "https://example.com/image.png")
        );
    }

    #[test]
    fn rejects_missing_image_data() {
        let error = parse_image_response(br#"{"data":[{}]}"#, "secret")
            .expect_err("missing image data should fail");

        assert!(matches!(error, ApiError::MissingImageData));
    }

    #[test]
    fn rejects_invalid_base64() {
        let error = parse_image_response(br#"{"data":[{"b64_json":"%%%"}]}"#, "secret")
            .expect_err("invalid Base64 should fail");

        assert!(matches!(error, ApiError::InvalidBase64(_)));
    }

    #[test]
    fn parses_structured_error_details() {
        let message = structured_error_message(
            r#"{"error":{"message":"bad request","type":"invalid_request","code":"bad","param":"size"}}"#,
        )
        .expect("structured error should parse");

        assert!(message.contains("bad request"));
        assert!(message.contains("invalid_request"));
        assert!(message.contains("size"));
    }

    #[test]
    fn sanitizes_non_json_error_bodies() {
        let body = sanitize_body("gateway failed for secret-value\u{0}", "secret-value");

        assert_eq!(body, "gateway failed for [REDACTED]");
    }
}
