use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, SharedString};
use thiserror::Error;
use tokio::runtime::{Builder, Handle};
use tokio::task::AbortHandle;

use super::state::{
    ApplicationState, ConnectionResult, OperationId, OperationKind, StateMachine, SuccessResult,
};
use crate::AppWindow;
use crate::api::{ApiClient, ApiError, ImageGenerationRequest, ModelsCheck, ResponseMetadata};
use crate::config::{Config, DEFAULT_BASE_URL, DEFAULT_IMAGE_MODEL};
use crate::domain::GeneratedImage;
use crate::storage::{self, SavedImagePreview};

#[derive(Debug, Error)]
pub enum GuiError {
    #[error("failed to create the asynchronous worker runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("failed to initialize or run the desktop interface: {0}")]
    Platform(#[from] slint::PlatformError),
}

#[derive(Clone)]
struct Backend {
    client: ApiClient,
    model: String,
}

struct ActiveOperation {
    id: OperationId,
    abort: AbortHandle,
}

struct OperationCoordinator {
    state: StateMachine,
    active: Option<ActiveOperation>,
}

#[derive(Clone)]
struct OperationControl {
    inner: Arc<Mutex<OperationCoordinator>>,
}

impl OperationControl {
    fn new(initial_state: ApplicationState) -> Self {
        Self {
            inner: Arc::new(Mutex::new(OperationCoordinator {
                state: StateMachine::new(initial_state),
                active: None,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, OperationCoordinator> {
        match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => {
                tracing::error!("operation coordinator lock was poisoned; recovering state");
                poisoned.into_inner()
            }
        }
    }

    fn state(&self) -> ApplicationState {
        self.lock().state.state().clone()
    }

    fn begin(&self, kind: OperationKind) -> (OperationId, ApplicationState) {
        let mut coordinator = self.lock();
        if let Some(active) = coordinator.active.take() {
            active.abort.abort();
        }
        let operation = coordinator.state.begin(kind);
        (operation, coordinator.state.state().clone())
    }

    fn attach(&self, operation: OperationId, abort: AbortHandle) {
        let mut coordinator = self.lock();
        if coordinator.state.is_active(operation) {
            if let Some(previous) = coordinator.active.replace(ActiveOperation {
                id: operation,
                abort,
            }) {
                previous.abort.abort();
            }
        } else {
            abort.abort();
        }
    }

    fn cancel(&self) -> Option<ApplicationState> {
        let mut coordinator = self.lock();
        let operation = coordinator.state.cancel()?;
        if let Some(active) = coordinator.active.take() {
            if active.id == operation {
                active.abort.abort();
            } else {
                tracing::warn!(
                    active = ?active.id,
                    cancelled = ?operation,
                    "discarding an abort handle that did not match the active state"
                );
                active.abort.abort();
            }
        }
        Some(coordinator.state.state().clone())
    }

    fn reject(&self, message: String) -> ApplicationState {
        let mut coordinator = self.lock();
        if let Some(active) = coordinator.active.take() {
            active.abort.abort();
        }
        coordinator.state.reject(message);
        coordinator.state.state().clone()
    }

    fn succeed(&self, operation: OperationId, result: SuccessResult) -> Option<ApplicationState> {
        let mut coordinator = self.lock();
        if !coordinator.state.succeed(operation, result) {
            return None;
        }
        clear_finished_handle(&mut coordinator, operation);
        Some(coordinator.state.state().clone())
    }

    fn fail(&self, operation: OperationId, message: String) -> Option<ApplicationState> {
        let mut coordinator = self.lock();
        if !coordinator.state.fail(operation, message) {
            return None;
        }
        clear_finished_handle(&mut coordinator, operation);
        Some(coordinator.state.state().clone())
    }
}

fn clear_finished_handle(coordinator: &mut OperationCoordinator, operation: OperationId) {
    if coordinator
        .active
        .as_ref()
        .is_some_and(|active| active.id == operation)
    {
        coordinator.active.take();
    }
}

enum OperationResult {
    Connection(ModelsCheck),
    Generation(GenerationResult),
}

struct GenerationResult {
    image: GeneratedImage,
    pixels: SharedPixelBuffer<Rgba8Pixel>,
}

pub fn run() -> Result<(), GuiError> {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("a6-image-worker")
        .build()
        .map_err(GuiError::Runtime)?;
    let ui = AppWindow::new()?;
    let (backend, initial_state) = load_backend(&ui);
    let operations = OperationControl::new(initial_state);
    present_state(&ui, &operations.state());
    bind_callbacks(&ui, runtime.handle().clone(), backend, operations);
    ui.run()?;
    Ok(())
}

fn load_backend(ui: &AppWindow) -> (Result<Backend, String>, ApplicationState) {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            let message = error.to_string();
            ui.set_endpoint_text(fallback_endpoint_label().into());
            ui.set_api_key_status("Not available".into());
            ui.set_model_text(DEFAULT_IMAGE_MODEL.into());
            ui.set_configured(false);
            return (
                Err(message.clone()),
                ApplicationState::Error {
                    operation: None,
                    message,
                },
            );
        }
    };

    ui.set_endpoint_text(config.sanitized_base_url().into());
    ui.set_api_key_status(config.api_key().masked().into());
    ui.set_model_text(config.model().into());

    match ApiClient::new(config.clone()) {
        Ok(client) => {
            ui.set_configured(true);
            (
                Ok(Backend {
                    client,
                    model: config.model().to_owned(),
                }),
                ApplicationState::Idle,
            )
        }
        Err(error) => {
            let message = format!("{}: {error}", error.category());
            ui.set_configured(false);
            (
                Err(message.clone()),
                ApplicationState::Error {
                    operation: None,
                    message,
                },
            )
        }
    }
}

fn fallback_endpoint_label() -> String {
    let base_url = std::env::var("A6API_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
    Config::new("display-only-placeholder", base_url, DEFAULT_IMAGE_MODEL)
        .map(|config| config.sanitized_base_url())
        .unwrap_or_else(|_| "Invalid A6API_BASE_URL".to_owned())
}

fn bind_callbacks(
    ui: &AppWindow,
    runtime: Handle,
    backend: Result<Backend, String>,
    operations: OperationControl,
) {
    ui.on_test_connection({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let backend = backend.clone();
        let operations = operations.clone();
        move || {
            let Ok(backend) = backend.clone() else {
                return;
            };
            let (operation, state) = operations.begin(OperationKind::Connecting);
            present_weak_state(&ui_weak, &state);

            let worker_operations = operations.clone();
            let worker_ui = ui_weak.clone();
            let task = runtime.spawn(async move {
                let result = backend
                    .client
                    .check_models()
                    .await
                    .map(OperationResult::Connection)
                    .map_err(OperationError::from);
                finish_operation(worker_ui, worker_operations, operation, result);
            });
            operations.attach(operation, task.abort_handle());
        }
    });

    ui.on_generate_image({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let backend = backend.clone();
        let operations = operations.clone();
        move |prompt: SharedString| {
            let prompt = prompt.trim().to_owned();
            if prompt.is_empty() {
                let state =
                    operations.reject("Enter a prompt before generating an image".to_owned());
                present_weak_state(&ui_weak, &state);
                return;
            }
            let Ok(backend) = backend.clone() else {
                return;
            };

            let (operation, state) = operations.begin(OperationKind::Generating);
            present_weak_state(&ui_weak, &state);

            let worker_operations = operations.clone();
            let worker_ui = ui_weak.clone();
            let task = runtime.spawn(async move {
                let result = generate(backend, prompt, Instant::now()).await;
                finish_operation(worker_ui, worker_operations, operation, result);
            });
            operations.attach(operation, task.abort_handle());
        }
    });

    ui.on_cancel_operation({
        let operations = operations.clone();
        let ui_weak = ui.as_weak();
        move || {
            if let Some(state) = operations.cancel() {
                present_weak_state(&ui_weak, &state);
            }
        }
    });
}

async fn generate(
    backend: Backend,
    prompt: String,
    started: Instant,
) -> Result<OperationResult, OperationError> {
    let request = ImageGenerationRequest::test_image(&backend.model, &prompt);
    let generated = backend.client.generate_image(&request).await?;
    let saved = storage::save_png_with_preview(generated.bytes).await?;
    let image = generated_image(saved, generated.metadata, started);
    let pixels = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        image.preview_rgba(),
        image.width(),
        image.height(),
    );

    Ok(OperationResult::Generation(GenerationResult {
        image,
        pixels,
    }))
}

fn generated_image(
    saved: SavedImagePreview,
    metadata: ResponseMetadata,
    started: Instant,
) -> GeneratedImage {
    GeneratedImage::new(
        saved.path,
        saved.width,
        saved.height,
        saved.file_size,
        started.elapsed(),
        metadata,
        saved.rgba,
    )
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

fn finish_operation(
    ui_weak: slint::Weak<AppWindow>,
    operations: OperationControl,
    operation: OperationId,
    result: Result<OperationResult, OperationError>,
) {
    let error_message = result.as_ref().err().map(ToString::to_string);
    let scheduled = ui_weak.upgrade_in_event_loop(move |ui| match result {
        Ok(OperationResult::Connection(check)) => {
            let success = SuccessResult::Connection(ConnectionResult::from(check));
            if let Some(state) = operations.succeed(operation, success) {
                present_state(&ui, &state);
            }
        }
        Ok(OperationResult::Generation(generated)) => {
            let success = SuccessResult::Generation(generated.image.clone());
            if let Some(state) = operations.succeed(operation, success) {
                present_state(&ui, &state);
                ui.set_generated_image(Image::from_rgba8(generated.pixels));
            }
        }
        Err(error) => {
            if let Some(state) = operations.fail(operation, error.to_string()) {
                present_state(&ui, &state);
            }
        }
    });
    if let Err(error) = scheduled {
        tracing::debug!(%error, ?error_message, "desktop result dropped after event loop exit");
    }
}

fn present_weak_state(ui_weak: &slint::Weak<AppWindow>, state: &ApplicationState) {
    if let Some(ui) = ui_weak.upgrade() {
        present_state(&ui, state);
    }
}

fn present_state(ui: &AppWindow, state: &ApplicationState) {
    match state {
        ApplicationState::Idle => {
            ui.set_busy(false);
            ui.set_connection_status("Ready".into());
            ui.set_error_text(SharedString::default());
            ui.set_result_details(SharedString::default());
        }
        ApplicationState::Connecting { .. } => present_busy(ui, "Testing connection"),
        ApplicationState::Generating { .. } => present_busy(ui, "Generating image"),
        ApplicationState::Success {
            result: SuccessResult::Connection(connection),
            ..
        } => {
            let availability = if connection.model_present {
                "model available"
            } else {
                "model not listed"
            };
            ui.set_busy(false);
            ui.set_connection_status(
                format!(
                    "Connected - HTTP {} - {availability}",
                    connection.http_status
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
            ui.set_result_details(metadata_text(&connection.metadata).into());
        }
        ApplicationState::Success {
            result: SuccessResult::Generation(image),
            ..
        } => {
            ui.set_busy(false);
            ui.set_connection_status("Image generated".into());
            ui.set_error_text(SharedString::default());
            ui.set_result_details(generated_image_details(image).into());
        }
        ApplicationState::Cancelled { .. } => {
            ui.set_busy(false);
            ui.set_connection_status("Cancelled".into());
            ui.set_error_text(SharedString::default());
            ui.set_result_details("The active request was cancelled".into());
        }
        ApplicationState::Error { message, .. } => {
            ui.set_busy(false);
            ui.set_connection_status("Error".into());
            ui.set_error_text(message.into());
            ui.set_result_details(SharedString::default());
        }
    }
}

fn present_busy(ui: &AppWindow, status: &str) {
    ui.set_busy(true);
    ui.set_connection_status(status.into());
    ui.set_error_text(SharedString::default());
    ui.set_result_details(SharedString::default());
}

fn generated_image_details(image: &GeneratedImage) -> String {
    let metadata = metadata_text(image.response_metadata());
    let metadata = if metadata.is_empty() {
        String::new()
    } else {
        format!(" | {metadata}")
    };
    format!(
        "{}x{} | {} bytes | {:.2?} | {}{}",
        image.width(),
        image.height(),
        image.file_size(),
        image.elapsed(),
        image.path().display(),
        metadata,
    )
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
    use std::future;

    use super::*;

    #[test]
    fn formats_request_id_without_other_response_data() {
        let metadata = ResponseMetadata {
            request_id: Some("request-123".to_owned()),
            ..ResponseMetadata::default()
        };

        assert_eq!(metadata_text(&metadata), "Request ID: request-123");
    }

    #[tokio::test]
    async fn cancellation_aborts_the_attached_worker_task() {
        let operations = OperationControl::new(ApplicationState::Idle);
        let (operation, _) = operations.begin(OperationKind::Generating);
        let task = tokio::spawn(future::pending::<()>());
        operations.attach(operation, task.abort_handle());

        let state = operations.cancel().expect("active operation should cancel");
        assert_eq!(state, ApplicationState::Cancelled { operation });

        let join_error = task
            .await
            .expect_err("cancelled worker task should not complete");
        assert!(join_error.is_cancelled());
    }
}
