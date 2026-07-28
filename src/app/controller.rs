use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, SharedString};
use thiserror::Error;
use tokio::runtime::{Builder, Handle};
use tokio::task::AbortHandle;

use super::actions;
use super::state::{
    ApplicationState, ConnectionResult, OperationId, OperationKind, StateMachine, SuccessResult,
};
use crate::AppWindow;
use crate::api::{ApiClient, ApiError, ImageGenerationRequest, ModelsCheck, ResponseMetadata};
use crate::config::{Config, DEFAULT_BASE_URL, DEFAULT_IMAGE_MODEL};
use crate::domain::{GeneratedImage, PersistedImage};
use crate::generation::{
    CompatibilitySettings, GenerationInput, GenerationOptions, GenerationValidationError,
    ImageBackground, ImageQuality, ImageSize, OutputFormat,
};
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
            if active.id != operation {
                tracing::warn!(
                    active = ?active.id,
                    cancelled = ?operation,
                    "discarding an abort handle that did not match the active state"
                );
            }
            active.abort.abort();
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

#[derive(Clone, Default)]
struct ResultStore {
    latest: Arc<Mutex<Option<GeneratedImage>>>,
}

impl ResultStore {
    fn latest(&self) -> Option<GeneratedImage> {
        match self.latest.lock() {
            Ok(latest) => latest.clone(),
            Err(poisoned) => {
                tracing::error!("result store lock was poisoned; recovering image");
                poisoned.into_inner().clone()
            }
        }
    }

    fn replace(&self, image: GeneratedImage) {
        match self.latest.lock() {
            Ok(mut latest) => *latest = Some(image),
            Err(poisoned) => {
                tracing::error!("result store lock was poisoned; replacing image");
                *poisoned.into_inner() = Some(image);
            }
        }
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
    let results = ResultStore::default();
    present_state(&ui, &operations.state());
    bind_callbacks(&ui, runtime.handle().clone(), backend, operations, results);
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
    results: ResultStore,
) {
    bind_connection_callback(ui, &runtime, &backend, &operations, &results);
    bind_generation_callbacks(ui, &runtime, &backend, &operations, &results);
    bind_result_actions(ui, &runtime, &results);

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

fn bind_connection_callback(
    ui: &AppWindow,
    runtime: &Handle,
    backend: &Result<Backend, String>,
    operations: &OperationControl,
    results: &ResultStore,
) {
    ui.on_test_connection({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let backend = backend.clone();
        let operations = operations.clone();
        let results = results.clone();
        move || {
            let Ok(backend) = backend.clone() else {
                return;
            };
            let (operation, state) = operations.begin(OperationKind::Connecting);
            present_weak_state(&ui_weak, &state);

            let worker_operations = operations.clone();
            let worker_results = results.clone();
            let worker_ui = ui_weak.clone();
            let task = runtime.spawn(async move {
                let result = backend
                    .client
                    .check_models()
                    .await
                    .map(OperationResult::Connection)
                    .map_err(OperationError::from);
                finish_operation(
                    worker_ui,
                    worker_operations,
                    worker_results,
                    operation,
                    result,
                );
            });
            operations.attach(operation, task.abort_handle());
        }
    });
}

fn bind_generation_callbacks(
    ui: &AppWindow,
    runtime: &Handle,
    backend: &Result<Backend, String>,
    operations: &OperationControl,
    results: &ResultStore,
) {
    ui.on_generate_image({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let backend = backend.clone();
        let operations = operations.clone();
        let results = results.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let input = match generation_input_from_ui(&ui) {
                Ok(input) => input,
                Err(error) => {
                    let state = operations.reject(error.to_string());
                    present_state(&ui, &state);
                    return;
                }
            };
            drop(ui);
            start_generation(&runtime, &backend, &operations, &results, &ui_weak, input);
        }
    });

    ui.on_regenerate({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let backend = backend.clone();
        let operations = operations.clone();
        let results = results.clone();
        move || {
            let Some(image) = results.latest() else {
                return;
            };
            let input = image.request().clone();
            if let Some(ui) = ui_weak.upgrade() {
                present_generation_input(&ui, &input);
            }
            start_generation(&runtime, &backend, &operations, &results, &ui_weak, input);
        }
    });
}

fn start_generation(
    runtime: &Handle,
    backend: &Result<Backend, String>,
    operations: &OperationControl,
    results: &ResultStore,
    ui_weak: &slint::Weak<AppWindow>,
    input: GenerationInput,
) {
    let Ok(backend) = backend.clone() else {
        return;
    };
    let (operation, state) = operations.begin(OperationKind::Generating);
    present_weak_state(ui_weak, &state);
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_action_status(SharedString::default());
    }

    let worker_operations = operations.clone();
    let worker_results = results.clone();
    let worker_ui = ui_weak.clone();
    let task = runtime.spawn(async move {
        let result = generate(backend, input, Instant::now()).await;
        finish_operation(
            worker_ui,
            worker_operations,
            worker_results,
            operation,
            result,
        );
    });
    operations.attach(operation, task.abort_handle());
}

fn bind_result_actions(ui: &AppWindow, runtime: &Handle, results: &ResultStore) {
    ui.on_save_as({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let results = results.clone();
        move || {
            let Some(image) = results.latest() else {
                return;
            };
            spawn_action(&runtime, &ui_weak, "Opening Save As…", async move {
                match actions::save_as(&image).await? {
                    Some(path) => Ok(format!("Saved copy to {}", path.display())),
                    None => Ok("Save As cancelled".to_owned()),
                }
            });
        }
    });

    ui.on_copy_image({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let results = results.clone();
        move || {
            let Some(image) = results.latest() else {
                return;
            };
            spawn_action(&runtime, &ui_weak, "Copying image…", async move {
                actions::copy_image(&image).await?;
                Ok("Image copied to clipboard".to_owned())
            });
        }
    });

    ui.on_copy_prompt({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        move |prompt: SharedString| {
            let prompt = prompt.to_string();
            spawn_action(&runtime, &ui_weak, "Copying prompt…", async move {
                actions::copy_prompt(prompt).await?;
                Ok("Prompt copied to clipboard".to_owned())
            });
        }
    });

    ui.on_open_containing_folder({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let results = results.clone();
        move || {
            let Some(image) = results.latest() else {
                return;
            };
            spawn_action(
                &runtime,
                &ui_weak,
                "Opening containing folder…",
                async move {
                    actions::open_containing_folder(&image).await?;
                    Ok("Opened containing folder".to_owned())
                },
            );
        }
    });
}

fn spawn_action<F>(
    runtime: &Handle,
    ui_weak: &slint::Weak<AppWindow>,
    pending_message: &str,
    future: F,
) where
    F: Future<Output = Result<String, actions::ResultActionError>> + Send + 'static,
{
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_action_busy(true);
        ui.set_action_status(pending_message.into());
    }
    let ui_weak = ui_weak.clone();
    runtime.spawn(async move {
        let result = future.await;
        let error_message = result.as_ref().err().map(ToString::to_string);
        let scheduled = ui_weak.upgrade_in_event_loop(move |ui| {
            ui.set_action_busy(false);
            match result {
                Ok(message) => ui.set_action_status(message.into()),
                Err(error) => ui.set_action_status(format!("Action failed: {error}").into()),
            }
        });
        if let Err(error) = scheduled {
            tracing::debug!(%error, ?error_message, "result action dropped after event loop exit");
        }
    });
}

fn generation_input_from_ui(ui: &AppWindow) -> Result<GenerationInput, GenerationValidationError> {
    let options = GenerationOptions {
        size: ui.get_size_value().as_str().parse::<ImageSize>()?,
        quality: ui.get_quality_value().as_str().parse::<ImageQuality>()?,
        background: ui
            .get_background_value()
            .as_str()
            .parse::<ImageBackground>()?,
        output_format: ui
            .get_output_format_value()
            .as_str()
            .parse::<OutputFormat>()?,
        compatibility: CompatibilitySettings {
            send_size: ui.get_send_size(),
            send_quality: ui.get_send_quality(),
            send_background: ui.get_send_background(),
            send_output_format: ui.get_send_output_format(),
        },
    };
    GenerationInput::new(ui.get_prompt().as_str(), options)
}

fn present_generation_input(ui: &AppWindow, input: &GenerationInput) {
    let options = input.options();
    ui.set_prompt(input.prompt().into());
    ui.set_size_value(options.size.to_string().into());
    ui.set_quality_value(options.quality.to_string().into());
    ui.set_background_value(options.background.to_string().into());
    ui.set_output_format_value(options.output_format.to_string().into());
    ui.set_send_size(options.compatibility.send_size);
    ui.set_send_quality(options.compatibility.send_quality);
    ui.set_send_background(options.compatibility.send_background);
    ui.set_send_output_format(options.compatibility.send_output_format);
}

async fn generate(
    backend: Backend,
    input: GenerationInput,
    started: Instant,
) -> Result<OperationResult, OperationError> {
    let request =
        ImageGenerationRequest::configured(&backend.model, input.prompt(), input.options());
    let generated = backend.client.generate_image(&request).await?;
    let output_format = input.options().output_format;
    let saved = storage::save_with_preview(generated.bytes, output_format).await?;
    let image = generated_image(saved, generated.metadata, started, input, output_format);
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
    input: GenerationInput,
    output_format: OutputFormat,
) -> GeneratedImage {
    GeneratedImage::new(
        PersistedImage {
            path: saved.path,
            width: saved.width,
            height: saved.height,
            file_size: saved.file_size,
            preview_rgba: saved.rgba,
            output_format,
        },
        started.elapsed(),
        metadata,
        input,
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
    results: ResultStore,
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
                results.replace(generated.image);
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
            ui.set_result_details(SharedString::default());
            present_generated_image(ui, image);
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

fn present_generated_image(ui: &AppWindow, image: &GeneratedImage) {
    ui.set_has_result(true);
    ui.set_result_duration(format_duration(image.elapsed()).into());
    ui.set_result_dimensions(format!("{} × {}", image.width(), image.height()).into());
    ui.set_result_file_size(format_file_size(image.file_size()).into());
    ui.set_result_format(image.output_format().display_name().into());
    ui.set_result_path(image.path().display().to_string().into());
    ui.set_result_request_id(
        image
            .response_metadata()
            .request_id
            .as_deref()
            .unwrap_or("Not provided")
            .into(),
    );
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{:.2} s", duration.as_secs_f64())
    } else {
        format!("{} ms", duration.as_millis())
    }
}

fn format_file_size(bytes: u64) -> String {
    const KIBIBYTE: f64 = 1024.0;
    const MEBIBYTE: f64 = 1024.0 * 1024.0;
    if bytes >= MEBIBYTE as u64 {
        format!("{:.2} MiB ({bytes} bytes)", bytes as f64 / MEBIBYTE)
    } else if bytes >= KIBIBYTE as u64 {
        format!("{:.1} KiB ({bytes} bytes)", bytes as f64 / KIBIBYTE)
    } else {
        format!("{bytes} bytes")
    }
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

    #[test]
    fn formats_human_readable_duration_and_file_size() {
        assert_eq!(format_duration(Duration::from_millis(725)), "725 ms");
        assert_eq!(format_duration(Duration::from_millis(1_250)), "1.25 s");
        assert_eq!(format_file_size(512), "512 bytes");
        assert_eq!(format_file_size(2_048), "2.0 KiB (2048 bytes)");
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
