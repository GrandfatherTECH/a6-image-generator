use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel,
};
use thiserror::Error;
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::AbortHandle;

use super::actions;
use super::state::{
    ApplicationState, ConnectionResult, OperationId, OperationKind, StateMachine, SuccessResult,
};
use crate::api::{ApiClient, ApiError, ImageGenerationRequest, ModelsCheck, ResponseMetadata};
use crate::config::{Config, DEFAULT_BASE_URL, DEFAULT_IMAGE_MODEL};
use crate::domain::{GeneratedImage, PersistedImage};
use crate::generation::{
    CompatibilitySettings, GenerationInput, GenerationOptions, GenerationValidationError,
    ImageBackground, ImageDimensions, ImageQuality, ImageSize, OutputFormat,
};
use crate::storage::{self, SavedImagePreview};
use crate::xdg::APP_ID;
use crate::{AppWindow, RecentGenerationItem};

const MAX_RECENT_RESULTS: usize = 6;
const CUSTOM_SIZE_LABEL: &str = "Custom…";
const SIZE_PRESETS: &[&str] = &[
    "auto",
    "1024x1024",
    "1536x1024",
    "1024x1536",
    "2048x2048",
    "2048x1152",
    "1152x2048",
    "2560x1440",
    "1440x2560",
    "3840x2160",
    "2160x3840",
];

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

#[derive(Default)]
struct ResultCollection {
    items: Vec<GenerationResult>,
    selected: Option<usize>,
}

#[derive(Clone, Default)]
struct ResultStore {
    inner: Arc<Mutex<ResultCollection>>,
}

impl ResultStore {
    fn selected(&self) -> Option<GenerationResult> {
        match self.inner.lock() {
            Ok(collection) => collection
                .selected
                .and_then(|index| collection.items.get(index))
                .cloned(),
            Err(poisoned) => {
                tracing::error!("result store lock was poisoned; recovering image");
                let collection = poisoned.into_inner();
                collection
                    .selected
                    .and_then(|index| collection.items.get(index))
                    .cloned()
            }
        }
    }

    fn push(&self, result: GenerationResult) {
        match self.inner.lock() {
            Ok(mut collection) => push_result(&mut collection, result),
            Err(poisoned) => {
                tracing::error!("result store lock was poisoned; replacing image");
                push_result(&mut poisoned.into_inner(), result);
            }
        }
    }

    fn select(&self, index: usize) -> Option<GenerationResult> {
        match self.inner.lock() {
            Ok(mut collection) => select_result(&mut collection, index),
            Err(poisoned) => {
                tracing::error!("result store lock was poisoned; selecting image");
                select_result(&mut poisoned.into_inner(), index)
            }
        }
    }

    fn snapshot(&self) -> Vec<(GenerationResult, bool)> {
        match self.inner.lock() {
            Ok(collection) => result_snapshot(&collection),
            Err(poisoned) => {
                tracing::error!("result store lock was poisoned; recovering recent results");
                result_snapshot(&poisoned.into_inner())
            }
        }
    }
}

fn push_result(collection: &mut ResultCollection, result: GenerationResult) {
    collection.items.insert(0, result);
    collection.items.truncate(MAX_RECENT_RESULTS);
    collection.selected = Some(0);
}

fn select_result(collection: &mut ResultCollection, index: usize) -> Option<GenerationResult> {
    let result = collection.items.get(index)?.clone();
    collection.selected = Some(index);
    Some(result)
}

fn result_snapshot(collection: &ResultCollection) -> Vec<(GenerationResult, bool)> {
    collection
        .items
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, result)| (result, collection.selected == Some(index)))
        .collect()
}

enum OperationResult {
    Connection(ModelsCheck),
    Generation(GenerationResult),
}

#[derive(Clone)]
struct GenerationResult {
    image: GeneratedImage,
    pixels: SharedPixelBuffer<Rgba8Pixel>,
}

pub fn run() -> Result<(), GuiError> {
    let runtime = build_worker_runtime()?;
    with_runtime_context(&runtime, |runtime_handle| {
        select_desktop_backend()?;
        slint::set_xdg_app_id(APP_ID)?;
        let ui = AppWindow::new()?;
        let (backend, initial_state) = load_backend(&ui);
        let operations = OperationControl::new(initial_state);
        let results = ResultStore::default();
        present_state(&ui, &operations.state());
        bind_callbacks(&ui, runtime_handle, backend, operations, results);
        ui.run()?;
        Ok(())
    })
}

fn select_desktop_backend() -> Result<(), slint::PlatformError> {
    if let Some(requested) = std::env::var("SLINT_BACKEND")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        tracing::info!(backend = %requested, "honoring explicit Slint backend selection");
        return slint::BackendSelector::new().select();
    }

    match slint::BackendSelector::new()
        .backend_name("winit".to_owned())
        .select()
    {
        Ok(()) => {
            tracing::info!("using the preferred Winit desktop backend");
            Ok(())
        }
        Err(winit_error) => {
            tracing::warn!(
                %winit_error,
                "Winit could not be initialized; trying the Qt fallback"
            );
            slint::BackendSelector::new()
                .backend_name("qt".to_owned())
                .select()
                .map_err(|qt_error| {
                    format!(
                        "neither preferred Winit nor fallback Qt could be initialized \
                         (Winit: {winit_error}; Qt: {qt_error})"
                    )
                    .into()
                })
        }
    }
}

fn build_worker_runtime() -> Result<Runtime, GuiError> {
    Builder::new_multi_thread()
        .enable_all()
        .thread_name("a6-image-worker")
        .build()
        .map_err(GuiError::Runtime)
}

fn with_runtime_context<T>(runtime: &Runtime, action: impl FnOnce(Handle) -> T) -> T {
    // Slint owns and polls its main-thread event loop. Keep a Tokio reactor
    // entered on that thread as well: Linux integrations can acquire Tokio
    // through Cargo feature unification even when our own work uses handles.
    let _runtime_context = runtime.enter();
    action(runtime.handle().clone())
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
    bind_dimension_callbacks(ui);

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
            let Some(result) = results.selected() else {
                return;
            };
            let input = result.image.request().clone();
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
            let Some(result) = results.selected() else {
                return;
            };
            let image = result.image;
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
            let Some(result) = results.selected() else {
                return;
            };
            let image = result.image;
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
            let Some(result) = results.selected() else {
                return;
            };
            let image = result.image;
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

    ui.on_select_recent({
        let ui_weak = ui.as_weak();
        let results = results.clone();
        move |index| {
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            let Some(result) = results.select(index) else {
                return;
            };
            if let Some(ui) = ui_weak.upgrade() {
                present_selected_result(&ui, &result);
                present_recent_results(&ui, &results);
            }
        }
    });
}

fn bind_dimension_callbacks(ui: &AppWindow) {
    ui.on_custom_dimensions_changed({
        let ui_weak = ui.as_weak();
        move |width, height| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_dimension_status(dimension_status(width, height).into());
            }
        }
    });
    ui.set_dimension_status(dimension_status(ui.get_custom_width(), ui.get_custom_height()).into());
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
    let size = if ui.get_size_value().as_str() == CUSTOM_SIZE_LABEL {
        let width = u32::try_from(ui.get_custom_width()).unwrap_or_default();
        let height = u32::try_from(ui.get_custom_height()).unwrap_or_default();
        ImageSize::dimensions(width, height)?
    } else {
        ui.get_size_value().as_str().parse::<ImageSize>()?
    };
    let options = GenerationOptions {
        size,
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
    let size = options.size.to_string();
    if SIZE_PRESETS.contains(&size.as_str()) {
        ui.set_size_value(size.into());
    } else if let Some(dimensions) = options.size.explicit_dimensions() {
        ui.set_size_value(CUSTOM_SIZE_LABEL.into());
        ui.set_custom_width(dimensions.width() as i32);
        ui.set_custom_height(dimensions.height() as i32);
        ui.set_dimension_status(
            dimension_status(dimensions.width() as i32, dimensions.height() as i32).into(),
        );
    }
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
    warn_if_dimensions_mismatch(
        &input,
        saved.width,
        saved.height,
        generated.metadata.request_id.as_deref(),
    );
    let pixels = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        &saved.preview_rgba,
        saved.preview_width,
        saved.preview_height,
    );
    let image = generated_image(saved, generated.metadata, started, input, output_format);

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
                results.push(generated);
                present_state(&ui, &state);
                if let Some(selected) = results.selected() {
                    present_selected_result(&ui, &selected);
                }
                present_recent_results(&ui, &results);
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
            ui.set_connection_status(
                if image.dimensions_match_request() == Some(false) {
                    "Image generated · size mismatch"
                } else {
                    "Image generated"
                }
                .into(),
            );
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
    ui.set_result_prompt(image.request().prompt().into());
    ui.set_result_settings(generation_settings_text(image.request().options()).into());
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
    ui.set_result_size_warning(dimension_integrity_warning(image).into());
}

fn generation_settings_text(options: &GenerationOptions) -> String {
    let compatibility = options.compatibility;
    format!(
        "Requested dimensions {}  ·  Quality {}  ·  Background {}  ·  API format {}",
        request_parameter(compatibility.send_size, options.size),
        request_parameter(compatibility.send_quality, options.quality),
        request_parameter(compatibility.send_background, options.background),
        request_parameter(compatibility.send_output_format, options.output_format),
    )
}

fn warn_if_dimensions_mismatch(
    input: &GenerationInput,
    received_width: u32,
    received_height: u32,
    request_id: Option<&str>,
) {
    let options = input.options();
    let requested = options
        .compatibility
        .send_size
        .then(|| options.size.explicit_dimensions())
        .flatten();
    if let Some(requested) = requested
        && (requested.width(), requested.height()) != (received_width, received_height)
    {
        tracing::warn!(
            requested_width = requested.width(),
            requested_height = requested.height(),
            received_width,
            received_height,
            request_id,
            "gateway returned image dimensions that do not match the request"
        );
    }
}

fn dimension_integrity_warning(image: &GeneratedImage) -> String {
    match (
        image.requested_dimensions(),
        image.dimensions_match_request(),
    ) {
        (Some(requested), Some(false)) => format!(
            "Size mismatch: requested {}×{}, but the gateway returned {}×{}. \
             The paid response was saved unchanged; no stretching or artificial upscaling was applied.",
            requested.width(),
            requested.height(),
            image.width(),
            image.height()
        ),
        _ => String::new(),
    }
}

fn request_parameter(enabled: bool, value: impl fmt::Display) -> String {
    if enabled {
        value.to_string()
    } else {
        "omitted".to_owned()
    }
}

fn present_selected_result(ui: &AppWindow, result: &GenerationResult) {
    present_generated_image(ui, &result.image);
    ui.set_generated_image(Image::from_rgba8(result.pixels.clone()));
}

fn present_recent_results(ui: &AppWindow, results: &ResultStore) {
    let recent = results
        .snapshot()
        .into_iter()
        .map(|(result, selected)| RecentGenerationItem {
            preview: Image::from_rgba8(result.pixels),
            label: format!("{}×{}", result.image.width(), result.image.height()).into(),
            selected,
        })
        .collect::<Vec<_>>();
    ui.set_recent_items(ModelRc::new(VecModel::from(recent)));
}

fn dimension_status(width: i32, height: i32) -> String {
    let width = u32::try_from(width).unwrap_or_default();
    let height = u32::try_from(height).unwrap_or_default();
    match ImageDimensions::new(width, height) {
        Ok(dimensions) if dimensions.is_experimental() => format!(
            "Valid · {} · experimental above 2560×1440",
            format_pixel_count(dimensions.pixel_count())
        ),
        Ok(dimensions) => format!(
            "Valid · {} · {:.2}:1",
            format_pixel_count(dimensions.pixel_count()),
            dimensions.width().max(dimensions.height()) as f64
                / dimensions.width().min(dimensions.height()) as f64
        ),
        Err(error) => error.to_string(),
    }
}

fn format_pixel_count(pixels: u64) -> String {
    if pixels >= 1_000_000 {
        format!("{:.2} MP", pixels as f64 / 1_000_000.0)
    } else {
        format!("{pixels} px")
    }
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
    use std::path::PathBuf;

    use super::*;
    use crate::generation::GenerationOptions;

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

    #[test]
    fn formats_sent_and_omitted_generation_settings() {
        let options = GenerationOptions {
            size: ImageSize::dimensions(2_048, 1_152).expect("test size should be valid"),
            compatibility: CompatibilitySettings {
                send_quality: false,
                ..CompatibilitySettings::default()
            },
            ..GenerationOptions::default()
        };

        assert_eq!(
            generation_settings_text(&options),
            "Requested dimensions 2048x1152  ·  Quality omitted  ·  Background auto  ·  API format PNG"
        );
    }

    #[test]
    fn reports_a_gateway_dimension_mismatch_without_claiming_the_requested_size() {
        let options = GenerationOptions {
            size: ImageSize::dimensions(3_840, 2_160).expect("test size should be valid"),
            ..GenerationOptions::default()
        };
        let image = GeneratedImage::new(
            PersistedImage {
                path: PathBuf::from("/tmp/mismatched.jpg"),
                width: 1_402,
                height: 1_122,
                file_size: 275_235,
                output_format: OutputFormat::Jpeg,
            },
            Duration::from_secs(219),
            ResponseMetadata::default(),
            GenerationInput::new("test prompt", options)
                .expect("test generation input should be valid"),
        );

        assert_eq!(image.dimensions_match_request(), Some(false));
        let warning = dimension_integrity_warning(&image);
        assert!(warning.contains("requested 3840×2160"));
        assert!(warning.contains("returned 1402×1122"));
        assert!(warning.contains("saved unchanged"));
    }

    #[test]
    fn omitting_or_automatically_selecting_size_has_no_integrity_warning() {
        let mut options = GenerationOptions {
            size: ImageSize::Auto,
            ..GenerationOptions::default()
        };
        let auto = generated_image_with_options(options.clone());
        assert_eq!(auto.dimensions_match_request(), None);
        assert!(dimension_integrity_warning(&auto).is_empty());

        options.size = ImageSize::dimensions(1_024, 1_024).expect("test size should be valid");
        options.compatibility.send_size = false;
        let omitted = generated_image_with_options(options);
        assert_eq!(omitted.dimensions_match_request(), None);
        assert!(dimension_integrity_warning(&omitted).is_empty());
    }

    #[test]
    fn gui_event_loop_scope_has_a_tokio_reactor() {
        let runtime = build_worker_runtime().expect("worker runtime should build");

        let value = with_runtime_context(&runtime, |_| {
            let task = tokio::task::spawn_blocking(|| 42);
            runtime
                .block_on(task)
                .expect("blocking task should run in the entered runtime")
        });

        assert_eq!(value, 42);
    }

    #[test]
    fn dimension_status_distinguishes_standard_experimental_and_invalid_sizes() {
        assert!(dimension_status(2_048, 1_152).starts_with("Valid · 2.36 MP"));
        assert!(dimension_status(3_840, 2_160).contains("experimental"));
        assert!(dimension_status(1_001, 1_024).contains("divisible by 16"));
    }

    #[test]
    fn recent_results_are_bounded_and_selectable() {
        let store = ResultStore::default();
        for index in 0..8 {
            store.push(generation_result(index));
        }

        let snapshot = store.snapshot();
        assert_eq!(snapshot.len(), MAX_RECENT_RESULTS);
        assert_eq!(
            snapshot[0].0.image.path(),
            PathBuf::from("/tmp/generated-7.png")
        );
        assert!(snapshot[0].1);

        let selected = store.select(2).expect("third recent result should exist");
        assert_eq!(selected.image.path(), PathBuf::from("/tmp/generated-5.png"));
        assert!(store.snapshot()[2].1);
        assert!(store.select(MAX_RECENT_RESULTS).is_none());
    }

    fn generation_result(index: u32) -> GenerationResult {
        let image = GeneratedImage::new(
            PersistedImage {
                path: PathBuf::from(format!("/tmp/generated-{index}.png")),
                width: 1_024,
                height: 1_024,
                file_size: 24,
                output_format: OutputFormat::Png,
            },
            Duration::from_millis(250),
            ResponseMetadata::default(),
            GenerationInput::new("test prompt", GenerationOptions::default())
                .expect("test generation input should be valid"),
        );
        GenerationResult {
            image,
            pixels: SharedPixelBuffer::new(1, 1),
        }
    }

    fn generated_image_with_options(options: GenerationOptions) -> GeneratedImage {
        GeneratedImage::new(
            PersistedImage {
                path: PathBuf::from("/tmp/generated.png"),
                width: 1_254,
                height: 1_254,
                file_size: 24,
                output_format: OutputFormat::Png,
            },
            Duration::from_millis(250),
            ResponseMetadata::default(),
            GenerationInput::new("test prompt", options)
                .expect("test generation input should be valid"),
        )
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
