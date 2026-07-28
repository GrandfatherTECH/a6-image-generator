use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, SharedString};
use thiserror::Error;
use tokio::runtime::{Builder, Handle};
use tokio::task::AbortHandle;

use crate::AppWindow;
use crate::api::{ApiClient, ApiError, ImageGenerationRequest, ModelsCheck, ResponseMetadata};
use crate::config::{Config, DEFAULT_BASE_URL, DEFAULT_IMAGE_MODEL};
use crate::storage::{self, SavedImagePreview};

#[derive(Debug, Error)]
pub enum GuiError {
    #[error("failed to create the asynchronous worker runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("failed to initialize or run the desktop interface: {0}")]
    Platform(#[from] slint::PlatformError),
}

#[derive(Clone, Default)]
struct OperationControl {
    active: Arc<Mutex<Option<AbortHandle>>>,
    generation: Arc<AtomicU64>,
}

impl OperationControl {
    fn begin(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
    }

    fn set_active(&self, handle: AbortHandle) -> Result<(), &'static str> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| "operation control lock is unavailable")?;
        *active = Some(handle);
        Ok(())
    }

    fn finish(&self) {
        match self.active.lock() {
            Ok(mut active) => {
                active.take();
            }
            Err(_) => tracing::error!("operation control lock is unavailable"),
        }
    }

    fn cancel(&self) -> bool {
        self.generation.fetch_add(1, Ordering::AcqRel);
        match self.active.lock() {
            Ok(mut active) => active.take().is_some_and(|handle| {
                handle.abort();
                true
            }),
            Err(_) => {
                tracing::error!("operation control lock is unavailable");
                false
            }
        }
    }
}

enum OperationResult {
    Connection(ModelsCheck),
    Generation(GenerationResult),
}

struct GenerationResult {
    saved: SavedImagePreview,
    metadata: ResponseMetadata,
    elapsed: Duration,
    pixels: SharedPixelBuffer<Rgba8Pixel>,
}

pub fn run() -> Result<(), GuiError> {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("a6-image-worker")
        .build()
        .map_err(GuiError::Runtime)?;
    let ui = AppWindow::new()?;
    let config = load_configuration(&ui);
    bind_callbacks(&ui, runtime.handle().clone(), config);
    ui.run()?;
    Ok(())
}

fn load_configuration(ui: &AppWindow) -> Result<Config, String> {
    match Config::from_env() {
        Ok(config) => {
            ui.set_endpoint_text(config.sanitized_base_url().into());
            ui.set_api_key_status(config.api_key().masked().into());
            ui.set_model_text(config.model().into());
            ui.set_connection_status("Ready".into());
            ui.set_configured(true);
            Ok(config)
        }
        Err(error) => {
            ui.set_endpoint_text(fallback_endpoint_label().into());
            ui.set_api_key_status("Not available".into());
            ui.set_model_text(DEFAULT_IMAGE_MODEL.into());
            ui.set_connection_status("Configuration required".into());
            ui.set_error_text(error.to_string().into());
            ui.set_configured(false);
            Err(error.to_string())
        }
    }
}

fn fallback_endpoint_label() -> String {
    let base_url = std::env::var("A6API_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
    Config::new("display-only-placeholder", base_url, DEFAULT_IMAGE_MODEL)
        .map(|config| config.sanitized_base_url())
        .unwrap_or_else(|_| "Invalid A6API_BASE_URL".to_owned())
}

fn bind_callbacks(ui: &AppWindow, runtime: Handle, config: Result<Config, String>) {
    let operations = OperationControl::default();

    ui.on_test_connection({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let config = config.clone();
        let operations = operations.clone();
        move || {
            let Ok(config) = config.clone() else {
                return;
            };
            set_busy(&ui_weak, "Testing connection");
            let generation = operations.begin();
            let worker_operations = operations.clone();
            let worker_ui = ui_weak.clone();
            let task = runtime.spawn(async move {
                let result = async {
                    let client = ApiClient::new(config).map_err(OperationError::from)?;
                    client
                        .check_models()
                        .await
                        .map(OperationResult::Connection)
                        .map_err(OperationError::from)
                }
                .await;
                finish_operation(worker_ui, worker_operations, generation, result);
            });
            if let Err(error) = operations.set_active(task.abort_handle()) {
                task.abort();
                set_controller_error(&ui_weak, error);
            }
        }
    });

    ui.on_generate_image({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let config = config.clone();
        let operations = operations.clone();
        move |prompt: SharedString| {
            let prompt = prompt.trim().to_owned();
            if prompt.is_empty() {
                set_controller_error(&ui_weak, "Enter a prompt before generating an image");
                return;
            }
            let Ok(config) = config.clone() else {
                return;
            };

            set_busy(&ui_weak, "Generating image");
            let generation = operations.begin();
            let worker_operations = operations.clone();
            let worker_ui = ui_weak.clone();
            let task = runtime.spawn(async move {
                let started = Instant::now();
                let result = generate(config, prompt, started).await;
                finish_operation(worker_ui, worker_operations, generation, result);
            });
            if let Err(error) = operations.set_active(task.abort_handle()) {
                task.abort();
                set_controller_error(&ui_weak, error);
            }
        }
    });

    ui.on_cancel_operation({
        let operations = operations.clone();
        let ui_weak = ui.as_weak();
        move || {
            if operations.cancel()
                && let Some(ui) = ui_weak.upgrade()
            {
                ui.set_busy(false);
                ui.set_connection_status("Cancelled".into());
                ui.set_error_text(SharedString::default());
                ui.set_result_details("The active request was cancelled".into());
            }
        }
    });
}

async fn generate(
    config: Config,
    prompt: String,
    started: Instant,
) -> Result<OperationResult, OperationError> {
    let client = ApiClient::new(config.clone())?;
    let request = ImageGenerationRequest::test_image(config.model(), &prompt);
    let generated = client.generate_image(&request).await?;
    let metadata = generated.metadata;
    let mut saved = storage::save_png_with_preview(generated.bytes).await?;
    let rgba = std::mem::take(&mut saved.rgba);
    let pixels =
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, saved.width, saved.height);
    Ok(OperationResult::Generation(GenerationResult {
        saved,
        metadata,
        elapsed: started.elapsed(),
        pixels,
    }))
}

#[derive(Debug, Error)]
enum OperationError {
    #[error("{category}: {source}")]
    Api {
        category: &'static str,
        #[source]
        source: ApiError,
    },
    #[error("output error: {0}")]
    Storage(#[from] storage::StorageError),
}

impl From<ApiError> for OperationError {
    fn from(source: ApiError) -> Self {
        Self::Api {
            category: source.category(),
            source,
        }
    }
}

fn set_busy(ui_weak: &slint::Weak<AppWindow>, status: &str) {
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_busy(true);
        ui.set_connection_status(status.into());
        ui.set_error_text(SharedString::default());
        ui.set_result_details(SharedString::default());
    }
}

fn set_controller_error(ui_weak: &slint::Weak<AppWindow>, message: &str) {
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_busy(false);
        ui.set_connection_status("Error".into());
        ui.set_error_text(message.into());
    }
}

fn finish_operation(
    ui_weak: slint::Weak<AppWindow>,
    operations: OperationControl,
    generation: u64,
    result: Result<OperationResult, OperationError>,
) {
    let error_message = result.as_ref().err().map(ToString::to_string);
    let scheduled = ui_weak.upgrade_in_event_loop(move |ui| {
        if !operations.is_current(generation) {
            return;
        }
        operations.finish();
        ui.set_busy(false);
        match result {
            Ok(OperationResult::Connection(check)) => apply_connection_result(&ui, check),
            Ok(OperationResult::Generation(generated)) => {
                apply_generation_result(&ui, generated);
            }
            Err(error) => {
                ui.set_connection_status("Error".into());
                ui.set_error_text(error.to_string().into());
            }
        }
    });
    if let Err(error) = scheduled {
        tracing::debug!(%error, ?error_message, "desktop result dropped after event loop exit");
    }
}

fn apply_connection_result(ui: &AppWindow, check: ModelsCheck) {
    let availability = if check.model_present {
        "model available"
    } else {
        "model not listed"
    };
    ui.set_connection_status(
        format!("Connected - HTTP {} - {availability}", check.http_status).into(),
    );
    ui.set_result_details(metadata_text(&check.metadata).into());
    ui.set_error_text(SharedString::default());
}

fn apply_generation_result(ui: &AppWindow, generated: GenerationResult) {
    let image = Image::from_rgba8(generated.pixels);
    ui.set_generated_image(image);
    ui.set_connection_status("Image generated".into());
    ui.set_error_text(SharedString::default());
    let metadata = metadata_text(&generated.metadata);
    let metadata = if metadata.is_empty() {
        String::new()
    } else {
        format!(" | {metadata}")
    };
    ui.set_result_details(
        format!(
            "{}x{} | {} bytes | {:.2?} | {}{}",
            generated.saved.width,
            generated.saved.height,
            generated.saved.file_size,
            generated.elapsed,
            generated.saved.path.display(),
            metadata,
        )
        .into(),
    );
}

fn metadata_text(metadata: &ResponseMetadata) -> String {
    metadata
        .request_id
        .as_ref()
        .map(|request_id| format!("Request ID: {request_id}"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_request_id_without_other_response_data() {
        let metadata = ResponseMetadata {
            request_id: Some("request-123".to_owned()),
            ..ResponseMetadata::default()
        };

        assert_eq!(metadata_text(&metadata), "Request ID: request-123");
    }

    #[test]
    fn operation_cancellation_invalidates_current_generation() {
        let operations = OperationControl::default();
        let generation = operations.begin();

        assert!(operations.is_current(generation));
        assert!(!operations.cancel());
        assert!(!operations.is_current(generation));
    }
}
