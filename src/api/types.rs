use serde::{Deserialize, Serialize};

use crate::generation::GenerationOptions;

#[derive(Debug, Clone, Serialize)]
pub struct ImageGenerationRequest<'a> {
    pub model: &'a str,
    pub prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_format: Option<&'a str>,
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
            size: Some("1024x1024".to_owned()),
            quality: Some("low"),
            background: None,
            output_format: Some("png"),
        }
    }

    pub fn configured(model: &'a str, prompt: &'a str, options: &'a GenerationOptions) -> Self {
        let compatibility = options.compatibility;
        Self {
            model,
            prompt,
            size: compatibility.send_size.then(|| options.size.api_value()),
            quality: compatibility
                .send_quality
                .then(|| options.quality.api_value()),
            background: compatibility
                .send_background
                .then(|| options.background.api_value()),
            output_format: compatibility
                .send_output_format
                .then(|| options.output_format.api_value()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation::{CompatibilitySettings, GenerationOptions};

    #[test]
    fn compatibility_settings_omit_disabled_parameters() {
        let options = GenerationOptions {
            compatibility: CompatibilitySettings {
                send_quality: false,
                send_background: false,
                ..CompatibilitySettings::default()
            },
            ..GenerationOptions::default()
        };
        let request = ImageGenerationRequest::configured("model", "prompt", &options);
        let json = serde_json::to_value(request).expect("request should serialize");

        assert_eq!(json["size"], "1024x1024");
        assert_eq!(json["output_format"], "png");
        assert!(json.get("quality").is_none());
        assert!(json.get("background").is_none());
    }
}

#[derive(Debug)]
pub struct ImageGenerationOutput {
    pub bytes: Vec<u8>,
    pub metadata: ResponseMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
