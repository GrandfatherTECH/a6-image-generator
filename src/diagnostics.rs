//! Privacy-conscious diagnostic reports suitable for attaching to bug reports.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::history::ErrorLogEntry;
use crate::xdg::APP_ID;

#[derive(Serialize)]
struct DiagnosticReport {
    schema_version: u32,
    generated_at_unix_seconds: u64,
    application: ApplicationDetails,
    system: SystemDetails,
    session_id: String,
    error_count: usize,
    errors: Vec<DiagnosticError>,
}

#[derive(Serialize)]
struct ApplicationDetails {
    id: &'static str,
    name: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
struct SystemDetails {
    operating_system: &'static str,
    architecture: &'static str,
}

#[derive(Serialize)]
struct DiagnosticError {
    timestamp: String,
    session_id: String,
    operation: String,
    model: String,
    endpoint: String,
    category: String,
    summary: String,
    request_id: Option<String>,
    provider_response_was_truncated: bool,
}

/// Serializes a report without prompts, provider response bodies, output paths,
/// database paths, preferences, or API keys.
pub(crate) fn export_report(
    session_id: &str,
    errors: Vec<ErrorLogEntry>,
    api_key: Option<&str>,
) -> Result<Vec<u8>, serde_json::Error> {
    let error_count = errors.len();
    let errors = errors
        .into_iter()
        .map(|entry| DiagnosticError {
            timestamp: redact(entry.timestamp, api_key),
            session_id: redact(entry.session_id, api_key),
            operation: redact(entry.operation, api_key),
            model: redact(entry.model, api_key),
            endpoint: redact(entry.endpoint, api_key),
            category: redact(entry.category, api_key),
            summary: redact(entry.summary, api_key),
            request_id: entry.request_id.map(|value| redact(value, api_key)),
            provider_response_was_truncated: entry.response_truncated,
        })
        .collect();
    let report = DiagnosticReport {
        schema_version: 1,
        generated_at_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        application: ApplicationDetails {
            id: APP_ID,
            name: "A6 Image Studio",
            version: env!("CARGO_PKG_VERSION"),
        },
        system: SystemDetails {
            operating_system: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
        },
        session_id: redact(session_id.to_owned(), api_key),
        error_count,
        errors,
    };
    serde_json::to_vec_pretty(&report)
}

fn redact(mut value: String, secret: Option<&str>) -> String {
    if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
        value = value.replace(secret, "[REDACTED]");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_excludes_sensitive_error_fields_and_redacts_the_active_key() {
        let api_key = "secret-test-api-key";
        let prompt = "a prompt that must not be exported";
        let provider_body = "a provider body that must not be exported";
        let report = export_report(
            "session-test",
            vec![ErrorLogEntry {
                id: "internal-error-id".to_owned(),
                session_id: "session-test".to_owned(),
                timestamp: "2026-08-02T00:00:00Z".to_owned(),
                operation: "image generation".to_owned(),
                prompt: Some(prompt.to_owned()),
                model: "gpt-image-2".to_owned(),
                endpoint: "https://example.invalid/v1".to_owned(),
                category: "gateway/server error".to_owned(),
                summary: format!("request using {api_key} failed"),
                full_response: Some(provider_body.to_owned()),
                response_truncated: true,
                request_id: Some("request-123".to_owned()),
            }],
            Some(api_key),
        )
        .expect("diagnostic report should serialize");
        let report = String::from_utf8(report).expect("diagnostic JSON should be UTF-8");

        assert!(!report.contains(api_key));
        assert!(!report.contains(prompt));
        assert!(!report.contains(provider_body));
        assert!(!report.contains("internal-error-id"));
        assert!(report.contains("[REDACTED]"));
        assert!(report.contains(APP_ID));
    }
}
