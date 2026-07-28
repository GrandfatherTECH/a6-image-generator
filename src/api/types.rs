use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct ImageGenerationRequest<'a> {
    pub model: &'a str,
    pub prompt: &'a str,
    pub size: &'a str,
    pub quality: &'a str,
    pub output_format: &'a str,
}

impl<'a> ImageGenerationRequest<'a> {
    pub fn smoke_test(model: &'a str) -> Self {
        Self::test_image(
            model,
            "A single solid blue circle centered on a plain white background",
        )
    }

    pub fn test_image(model: &'a str, prompt: &'a str) -> Self {
        Self {
            model,
            prompt,
            size: "1024x1024",
            quality: "low",
            output_format: "png",
        }
    }
}

#[derive(Debug)]
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub metadata: ResponseMetadata,
}

#[derive(Debug)]
pub struct ModelsCheck {
    pub http_status: u16,
    pub model_present: bool,
    pub metadata: ResponseMetadata,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseMetadata {
    pub request_id: Option<String>,
    pub retry_after: Option<String>,
    pub rate_limit_limit: Option<String>,
    pub rate_limit_remaining: Option<String>,
    pub rate_limit_reset: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelsResponse {
    #[serde(default)]
    pub data: Vec<Model>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Model {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ImageGenerationResponse {
    #[serde(default)]
    pub data: Vec<ImageData>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ImageData {
    pub b64_json: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ApiErrorEnvelope {
    pub error: ApiErrorBody,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ApiErrorBody {
    pub message: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub code: Option<serde_json::Value>,
    pub param: Option<serde_json::Value>,
}
