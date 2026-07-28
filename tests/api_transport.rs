use std::collections::HashMap;
use std::io;
use std::time::Duration;

use a6_image_studio::api::{ApiClient, ApiError, HttpFailure, ImageGenerationRequest};
use a6_image_studio::config::Config;
use a6_image_studio::generation::{
    GenerationOptions, ImageBackground, ImageQuality, ImageSize, OutputFormat,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const MAX_TEST_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct MockResponse {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
}

impl MockResponse {
    fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("content-type", "application/json")],
            body: serde_json::to_vec(&body).expect("test JSON should serialize"),
        }
    }

    fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("content-type", "text/plain")],
            body: body.as_bytes().to_vec(),
        }
    }

    fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }
}

async fn start_mock_server(
    responses: Vec<MockResponse>,
) -> io::Result<(String, JoinHandle<io::Result<Vec<CapturedRequest>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let base_url = format!("http://{address}");
    let response_base_url = base_url.clone();
    let task = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(responses.len());
        for response in responses {
            let (mut stream, _) = listener.accept().await?;
            requests.push(read_request(&mut stream).await?);

            let MockResponse {
                status,
                headers,
                body,
            } = response;
            let body = if body
                .windows(b"{{MOCK_BASE_URL}}".len())
                .any(|window| window == b"{{MOCK_BASE_URL}}")
            {
                String::from_utf8_lossy(&body)
                    .replace("{{MOCK_BASE_URL}}", &response_base_url)
                    .into_bytes()
            } else {
                body
            };
            let reason = match status {
                200 => "OK",
                401 => "Unauthorized",
                404 => "Not Found",
                502 => "Bad Gateway",
                _ => "Test Response",
            };
            let mut head = format!(
                "HTTP/1.1 {} {}\r\ncontent-length: {}\r\nconnection: close\r\n",
                status,
                reason,
                body.len()
            );
            for (name, value) in headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str("\r\n");
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(&body).await?;
            stream.shutdown().await?;
        }
        Ok(requests)
    });
    Ok((base_url, task))
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> io::Result<CapturedRequest> {
    let mut bytes = Vec::new();
    let (header_end, content_length) = loop {
        if bytes.len() >= MAX_TEST_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "mock request exceeded safety limit",
            ));
        }
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request ended before its headers",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = find_header_end(&bytes) {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            break (header_end, content_length);
        }
    };

    let body_start = header_end + 4;
    while bytes.len() < body_start + content_length {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request body ended early",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_TEST_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "mock request exceeded safety limit",
            ));
        }
    }

    parse_request(&bytes[..body_start + content_length], header_end)
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_request(bytes: &[u8], header_end: usize) -> io::Result<CapturedRequest> {
    let head = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = head.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_owned();
    let path = request_parts.next().unwrap_or_default().to_owned();
    if method.is_empty() || path.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid request line",
        ));
    }

    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    Ok(CapturedRequest {
        method,
        path,
        headers,
        body: bytes[header_end + 4..].to_vec(),
    })
}

async fn finish_server(task: JoinHandle<io::Result<Vec<CapturedRequest>>>) -> Vec<CapturedRequest> {
    timeout(Duration::from_secs(5), task)
        .await
        .expect("mock server should finish before timeout")
        .expect("mock server task should not panic")
        .expect("mock server should handle requests")
}

fn test_config(base_url: &str) -> Config {
    Config::new("test-secret-key", base_url, "gpt-image-2")
        .expect("mock server URL should make a valid configuration")
}

#[tokio::test]
async fn checks_models_with_authentication_and_metadata() {
    let response = MockResponse::json(200, serde_json::json!({"data": [{"id": "gpt-image-2"}]}))
        .with_header("x-request-id", "request-123");
    let (base_url, server) = start_mock_server(vec![response])
        .await
        .expect("mock server should start");

    let client = ApiClient::new(test_config(&base_url)).expect("client should build");
    let result = client
        .check_models()
        .await
        .expect("model check should succeed");
    let requests = finish_server(server).await;

    assert_eq!(result.http_status, 200);
    assert!(result.model_present);
    assert_eq!(result.metadata.request_id.as_deref(), Some("request-123"));
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/v1/models");
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer test-secret-key")
    );
}

#[tokio::test]
async fn generates_from_base64_response() {
    let response = MockResponse::json(
        200,
        serde_json::json!({
            "data": [{"b64_json": STANDARD.encode(b"image bytes")}]
        }),
    );
    let (base_url, server) = start_mock_server(vec![response])
        .await
        .expect("mock server should start");

    let client = ApiClient::new(test_config(&base_url)).expect("client should build");
    let request = ImageGenerationRequest::smoke_test("gpt-image-2");
    let result = client
        .generate_image(&request)
        .await
        .expect("generation response should decode");
    let requests = finish_server(server).await;

    assert_eq!(result.bytes, b"image bytes");
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/images/generations");
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer test-secret-key")
    );
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("request should contain JSON");
    assert_eq!(body["model"], "gpt-image-2");
    assert_eq!(body["size"], "1024x1024");
    assert_eq!(body["quality"], "low");
    assert_eq!(body["output_format"], "png");
}

#[tokio::test]
async fn sends_all_configured_generation_parameters() {
    let response = MockResponse::json(
        200,
        serde_json::json!({
            "data": [{"b64_json": STANDARD.encode(b"image bytes")}]
        }),
    );
    let (base_url, server) = start_mock_server(vec![response])
        .await
        .expect("mock server should start");

    let client = ApiClient::new(test_config(&base_url)).expect("client should build");
    let options = GenerationOptions {
        size: ImageSize::dimensions(2_048, 1_152).expect("documented 2K size should be valid"),
        quality: ImageQuality::High,
        background: ImageBackground::Transparent,
        output_format: OutputFormat::WebP,
        ..GenerationOptions::default()
    };
    let request = ImageGenerationRequest::configured("gpt-image-2", "phase four prompt", &options);
    client
        .generate_image(&request)
        .await
        .expect("generation response should decode");
    let requests = finish_server(server).await;

    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("request should contain JSON");
    assert_eq!(body["model"], "gpt-image-2");
    assert_eq!(body["prompt"], "phase four prompt");
    assert_eq!(body["size"], "2048x1152");
    assert_eq!(body["quality"], "high");
    assert_eq!(body["background"], "transparent");
    assert_eq!(body["output_format"], "webp");
}

#[tokio::test]
async fn downloads_url_fallback_without_forwarding_authentication() {
    let responses = vec![
        MockResponse::json(
            200,
            serde_json::json!({
                "data": [{"url": "{{MOCK_BASE_URL}}/generated/image.png"}]
            }),
        ),
        MockResponse {
            status: 200,
            headers: vec![("content-type", "image/png")],
            body: b"downloaded image".to_vec(),
        },
    ];
    let (actual_base_url, server) = start_mock_server(responses)
        .await
        .expect("mock server should start");

    // Rebuild the response against the server's actual bound address.
    let client = ApiClient::new(test_config(&actual_base_url)).expect("client should build");
    let request = ImageGenerationRequest::smoke_test("gpt-image-2");
    let result = client
        .generate_image(&request)
        .await
        .expect("URL image should download");
    let requests = finish_server(server).await;

    assert_eq!(result.bytes, b"downloaded image");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, "GET");
    assert_eq!(requests[1].path, "/generated/image.png");
    assert!(!requests[1].headers.contains_key("authorization"));
}

#[tokio::test]
async fn preserves_structured_api_errors() {
    let response = MockResponse::json(
        401,
        serde_json::json!({
            "error": {
                "message": "invalid test-secret-key credential",
                "type": "authentication_error",
                "code": "invalid_api_key"
            }
        }),
    )
    .with_header("x-request-id", "rejected-123");
    let (base_url, server) = start_mock_server(vec![response])
        .await
        .expect("mock server should start");

    let client = ApiClient::new(test_config(&base_url)).expect("client should build");
    let request = ImageGenerationRequest::smoke_test("gpt-image-2");
    let error = client
        .generate_image(&request)
        .await
        .expect_err("request should fail");
    let _requests = finish_server(server).await;

    match error {
        ApiError::Http {
            kind,
            message,
            metadata,
            ..
        } => {
            assert_eq!(kind, HttpFailure::Authentication);
            assert!(message.contains("invalid [REDACTED] credential"));
            assert!(!message.contains("test-secret-key"));
            assert_eq!(metadata.request_id.as_deref(), Some("rejected-123"));
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[tokio::test]
async fn preserves_non_json_api_error_body_and_redacts_key() {
    let response = MockResponse::text(
        502,
        "upstream rejected test-secret-key while processing the request",
    );
    let (base_url, server) = start_mock_server(vec![response])
        .await
        .expect("mock server should start");

    let client = ApiClient::new(test_config(&base_url)).expect("client should build");
    let error = client
        .check_models()
        .await
        .expect_err("request should fail");
    let _requests = finish_server(server).await;

    match error {
        ApiError::Http { body, .. } => {
            assert!(body.contains("[REDACTED]"));
            assert!(!body.contains("test-secret-key"));
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[tokio::test]
async fn treats_missing_models_endpoint_as_unsupported() {
    let (base_url, server) = start_mock_server(vec![MockResponse::text(404, "not found")])
        .await
        .expect("mock server should start");

    let client = ApiClient::new(test_config(&base_url)).expect("client should build");
    let error = client
        .check_models()
        .await
        .expect_err("missing endpoint should be reported");
    let _requests = finish_server(server).await;

    assert!(matches!(error, ApiError::UnsupportedEndpoint { .. }));
}
