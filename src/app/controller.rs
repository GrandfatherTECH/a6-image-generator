//! Slint event-loop orchestration and asynchronous desktop workers.
//!
//! UI callbacks perform validation and state transitions only; network,
//! keyring, decoding, and filesystem work is dispatched away from the event
//! loop and returned through weak component handles.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
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
use crate::config::{Config, DEFAULT_IMAGE_MODEL};
use crate::diagnostics;
use crate::domain::{GeneratedImage, PersistedImage};
use crate::generation::{
    CompatibilitySettings, GenerationInput, GenerationOptions, GenerationValidationError,
    ImageBackground, ImageDimensions, ImageQuality, ImageSize, OutputFormat,
};
use crate::history::{
    ErrorLogEntry, HistoryEntry, HistoryRepository, display_timestamp, error_matches,
    history_matches, new_session_id,
};
use crate::preferences::{AppPreferences, PreferencesStore};
use crate::secrets::{self, ApiKeySource, ResolvedApiKey};
use crate::storage::{self, SavedImagePreview};
use crate::xdg::{APP_ID, AppPaths};
use crate::{
    AppWindow, ErrorLogItem, HistoryGenerationItem, HistorySessionItem, RecentGenerationItem,
};

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

/// Desktop bootstrap, runtime, or Slint platform failure.
#[derive(Debug, Error)]
pub enum GuiError {
    #[error("failed to create the asynchronous worker runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("failed to initialize or run the desktop interface: {0}")]
    Platform(#[from] slint::PlatformError),
    #[error("failed to initialize desktop data paths: {0}")]
    Bootstrap(String),
}

#[derive(Clone)]
struct Backend {
    client: ApiClient,
    model: String,
    endpoint: String,
    output_directory: PathBuf,
}

#[derive(Clone)]
struct BackendStore {
    inner: Arc<Mutex<Result<Backend, String>>>,
}

impl BackendStore {
    fn new(backend: Result<Backend, String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(backend)),
        }
    }

    fn current(&self) -> Result<Backend, String> {
        match self.inner.lock() {
            Ok(backend) => backend.clone(),
            Err(poisoned) => {
                tracing::error!("backend store lock was poisoned; recovering");
                poisoned.into_inner().clone()
            }
        }
    }

    fn replace(&self, backend: Result<Backend, String>) {
        match self.inner.lock() {
            Ok(mut current) => *current = backend,
            Err(poisoned) => {
                tracing::error!("backend store lock was poisoned; replacing backend");
                *poisoned.into_inner() = backend;
            }
        }
    }
}

#[derive(Clone)]
struct DesktopServices {
    preferences_store: PreferencesStore,
    preferences: Arc<Mutex<AppPreferences>>,
    credential: Arc<Mutex<Option<ResolvedApiKey>>>,
    backend: BackendStore,
    history: HistoryRepository,
    session_id: String,
    selection: Arc<Mutex<BrowserSelection>>,
}

#[derive(Clone, Debug, Default)]
struct BrowserSelection {
    history_session: Option<String>,
    history_entry: Option<String>,
    error_entry: Option<String>,
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

    fn try_begin(&self, kind: OperationKind) -> Option<(OperationId, ApplicationState)> {
        let mut coordinator = self.lock();
        if matches!(
            coordinator.state.state(),
            ApplicationState::Connecting { .. } | ApplicationState::Generating { .. }
        ) {
            return None;
        }
        let operation = coordinator.state.begin(kind);
        Some((operation, coordinator.state.state().clone()))
    }

    fn is_busy(&self) -> bool {
        matches!(
            self.lock().state.state(),
            ApplicationState::Connecting { .. } | ApplicationState::Generating { .. }
        )
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

    fn shutdown(&self) {
        let mut coordinator = self.lock();
        let _ = coordinator.state.cancel();
        if let Some(active) = coordinator.active.take() {
            active.abort.abort();
        }
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
    history_warning: Option<String>,
}

pub fn run() -> Result<(), GuiError> {
    let runtime = build_worker_runtime()?;
    let paths = AppPaths::discover().map_err(|error| GuiError::Bootstrap(error.to_string()))?;
    let defaults = AppPreferences::defaults(&paths);
    let preferences_store =
        PreferencesStore::discover().map_err(|error| GuiError::Bootstrap(error.to_string()))?;
    let (preferences, preferences_warning) = match preferences_store.load(&defaults) {
        Ok(preferences) => (preferences, None),
        Err(error) => {
            tracing::error!(%error, "preferences could not be loaded; using defaults");
            (
                defaults,
                Some(format!(
                    "Settings could not be loaded, so safe defaults are active: {error}"
                )),
            )
        }
    };
    let (history, history_warning) = match HistoryRepository::discover() {
        Ok(history) => (history, None),
        Err(error) => {
            tracing::error!(%error, "history could not be loaded; starting with an empty view");
            (
                HistoryRepository::in_memory()
                    .map_err(|fallback| GuiError::Bootstrap(fallback.to_string()))?,
                Some(format!(
                    "The SQLite history database could not be loaded. New entries will remain in memory for this run: {error}"
                )),
            )
        }
    };
    let (credential, credential_warning) = resolve_initial_credential(&runtime);
    let session_id = new_session_id().map_err(|error| GuiError::Bootstrap(error.to_string()))?;

    with_runtime_context(&runtime, |runtime_handle| {
        select_desktop_backend()?;
        slint::set_xdg_app_id(APP_ID)?;
        let ui = AppWindow::new()?;
        present_preferences(&ui, &preferences);
        if let Some(warning) = preferences_warning {
            ui.set_settings_status(warning.into());
        }
        if let Some(warning) = history_warning {
            ui.set_history_status(warning.into());
        }
        if let Some(warning) = credential_warning {
            ui.set_settings_status(warning.into());
        }
        let (backend, initial_state) = load_backend(&ui, &preferences, credential.as_ref());
        let backend = BackendStore::new(backend);
        let services = DesktopServices {
            preferences_store,
            preferences: Arc::new(Mutex::new(preferences)),
            credential: Arc::new(Mutex::new(credential)),
            backend,
            history,
            session_id,
            selection: Arc::new(Mutex::new(BrowserSelection::default())),
        };
        let operations = OperationControl::new(initial_state);
        let results = ResultStore::default();
        present_state(&ui, &operations.state());
        present_history_browser(&ui, &services);
        present_error_browser(&ui, &services);
        ui.window().on_close_requested({
            let operations = operations.clone();
            move || {
                operations.shutdown();
                slint::CloseRequestResponse::HideWindow
            }
        });
        bind_callbacks(&ui, runtime_handle, services, operations, results);
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
        return slint::BackendSelector::new()
            .with_winit_window_attributes_hook(|attributes| {
                attributes.with_transparent(true).with_blur(true)
            })
            .select();
    }

    match slint::BackendSelector::new()
        .backend_name("winit".to_owned())
        .with_winit_window_attributes_hook(|attributes| {
            attributes.with_transparent(true).with_blur(true)
        })
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

fn run_blocking_on_runtime<T, F>(
    runtime: &Runtime,
    operation: F,
) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    runtime.block_on(async move { tokio::task::spawn_blocking(operation).await })
}

fn resolve_initial_credential(runtime: &Runtime) -> (Option<ResolvedApiKey>, Option<String>) {
    match secrets::environment_key() {
        Ok(Some(key)) => return (Some(key), None),
        Ok(None) => {}
        Err(error) => return (None, Some(error.to_string())),
    }

    match run_blocking_on_runtime(runtime, secrets::load_keyring_key) {
        Ok(Ok(key)) => (key, None),
        Ok(Err(error)) => (
            None,
            Some(format!(
                "The system keyring is unavailable. Set A6API_KEY or unlock Secret Service/KWallet: {error}"
            )),
        ),
        Err(error) => (
            None,
            Some(format!("The system keyring worker failed: {error}")),
        ),
    }
}

fn load_backend(
    ui: &AppWindow,
    preferences: &AppPreferences,
    credential: Option<&ResolvedApiKey>,
) -> (Result<Backend, String>, ApplicationState) {
    let Some(credential) = credential else {
        let message =
            "No API key is available. Set A6API_KEY or store one securely in Settings.".to_owned();
        let (base_url, model, note) = effective_endpoint_and_model(preferences);
        ui.set_endpoint_text(fallback_endpoint_label(&base_url).into());
        ui.set_api_key_status("Not available".into());
        ui.set_model_text(model.into());
        ui.set_settings_api_key_source("none".into());
        ui.set_settings_api_key_mask(SharedString::default());
        ui.set_settings_override_note(note.into());
        ui.set_configured(false);
        return (
            Err(message.clone()),
            ApplicationState::Error {
                operation: None,
                message,
            },
        );
    };

    let (base_url, model, note) = effective_endpoint_and_model(preferences);
    let config = match Config::with_timeout(
        credential.key.clone(),
        &base_url,
        &model,
        Duration::from_secs(preferences.request_timeout_seconds),
    ) {
        Ok(config) => config,
        Err(error) => {
            let message = error.to_string();
            ui.set_endpoint_text(fallback_endpoint_label(&base_url).into());
            ui.set_api_key_status(
                format!(
                    "{} ({})",
                    credential.key.masked(),
                    credential.source.label()
                )
                .into(),
            );
            ui.set_model_text(model.into());
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
    ui.set_api_key_status(
        format!(
            "{} ({})",
            config.api_key().masked(),
            credential.source.label()
        )
        .into(),
    );
    ui.set_model_text(config.model().into());
    ui.set_settings_api_key_source(credential.source.label().into());
    ui.set_settings_api_key_mask(config.api_key().masked().into());
    ui.set_settings_override_note(note.into());

    match ApiClient::new(config.clone()) {
        Ok(client) => {
            ui.set_configured(true);
            (
                Ok(Backend {
                    client,
                    model: config.model().to_owned(),
                    endpoint: config.sanitized_base_url(),
                    output_directory: preferences.output_directory.clone(),
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

fn fallback_endpoint_label(base_url: &str) -> String {
    Config::new("display-only-placeholder", base_url, DEFAULT_IMAGE_MODEL)
        .map(|config| config.sanitized_base_url())
        .unwrap_or_else(|_| "Invalid A6API_BASE_URL".to_owned())
}

fn effective_endpoint_and_model(preferences: &AppPreferences) -> (String, String, String) {
    let mut overrides = Vec::new();
    let base_url = match std::env::var("A6API_BASE_URL") {
        Ok(value) => {
            overrides.push("A6API_BASE_URL");
            value
        }
        Err(_) => preferences.base_url.clone(),
    };
    let model = match std::env::var("A6API_IMAGE_MODEL") {
        Ok(value) => {
            overrides.push("A6API_IMAGE_MODEL");
            value
        }
        Err(_) => preferences.model.clone(),
    };
    let note = if overrides.is_empty() {
        String::new()
    } else {
        format!(
            "Environment override active: {}. Saved values take effect after those variables are unset.",
            overrides.join(", ")
        )
    };
    (base_url, model, note)
}

fn present_preferences(ui: &AppWindow, preferences: &AppPreferences) {
    ui.set_settings_base_url(preferences.base_url.clone().into());
    ui.set_settings_model_id(preferences.model.clone().into());
    ui.set_settings_default_size(preferences.default_size.clone().into());
    ui.set_settings_default_quality(preferences.default_quality.clone().into());
    ui.set_settings_default_format(preferences.default_format.clone().into());
    ui.set_settings_output_directory(preferences.output_directory.display().to_string().into());
    ui.set_settings_request_timeout(preferences.request_timeout_seconds as i32);
    ui.set_settings_retain_history(preferences.retain_history);
    ui.set_size_value(preferences.default_size.clone().into());
    ui.set_quality_value(preferences.default_quality.clone().into());
    ui.set_output_format_value(preferences.default_format.clone().into());
}

fn bind_callbacks(
    ui: &AppWindow,
    runtime: Handle,
    services: DesktopServices,
    operations: OperationControl,
    results: ResultStore,
) {
    bind_connection_callback(ui, &runtime, &services, &operations, &results);
    bind_generation_callbacks(ui, &runtime, &services, &operations, &results);
    bind_result_actions(ui, &runtime, &results);
    bind_dimension_callbacks(ui);
    bind_workspace_callbacks(ui, &runtime, &services);

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
    services: &DesktopServices,
    operations: &OperationControl,
    results: &ResultStore,
) {
    ui.on_test_connection({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        let operations = operations.clone();
        let results = results.clone();
        move || {
            let Ok(backend) = services.backend.current() else {
                return;
            };
            let Some((operation, state)) = operations.try_begin(OperationKind::Connecting) else {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_action_status("A request is already in progress".into());
                }
                return;
            };
            present_weak_state(&ui_weak, &state);

            let worker_operations = operations.clone();
            let worker_results = results.clone();
            let worker_ui = ui_weak.clone();
            let worker_services = services.clone();
            let task = runtime.spawn(async move {
                let result = backend
                    .client
                    .check_models()
                    .await
                    .map(OperationResult::Connection)
                    .map_err(OperationError::from);
                if let Err(error) = &result {
                    record_operation_error(
                        &worker_services,
                        "connection test",
                        None,
                        &backend,
                        error,
                    );
                }
                finish_operation(
                    worker_ui,
                    worker_operations,
                    worker_results,
                    worker_services,
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
    services: &DesktopServices,
    operations: &OperationControl,
    results: &ResultStore,
) {
    ui.on_generate_image({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        let operations = operations.clone();
        let results = results.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            if operations.is_busy() {
                ui.set_action_status("A request is already in progress".into());
                return;
            }
            let input = match generation_input_from_ui(&ui) {
                Ok(input) => input,
                Err(error) => {
                    let message = error.to_string();
                    let state = operations.reject(message.clone());
                    present_state(&ui, &state);
                    let worker_services = services.clone();
                    let worker_ui = ui_weak.clone();
                    let prompt = ui.get_prompt().to_string();
                    runtime.spawn(async move {
                        record_local_error(
                            &worker_services,
                            "request validation",
                            Some(prompt),
                            "local validation",
                            message,
                        );
                        let _ = worker_ui.upgrade_in_event_loop(move |ui| {
                            present_error_browser(&ui, &worker_services);
                        });
                    });
                    return;
                }
            };
            drop(ui);
            start_generation(&runtime, &services, &operations, &results, &ui_weak, input);
        }
    });

    ui.on_regenerate({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
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
            start_generation(&runtime, &services, &operations, &results, &ui_weak, input);
        }
    });
}

fn start_generation(
    runtime: &Handle,
    services: &DesktopServices,
    operations: &OperationControl,
    results: &ResultStore,
    ui_weak: &slint::Weak<AppWindow>,
    input: GenerationInput,
) {
    let Ok(backend) = services.backend.current() else {
        return;
    };
    let Some((operation, state)) = operations.try_begin(OperationKind::Generating) else {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_action_status("A request is already in progress".into());
        }
        return;
    };
    present_weak_state(ui_weak, &state);
    if let Some(ui) = ui_weak.upgrade() {
        ui.set_action_status(SharedString::default());
    }

    let worker_operations = operations.clone();
    let worker_results = results.clone();
    let worker_ui = ui_weak.clone();
    let worker_services = services.clone();
    let error_input = input.clone();
    let task = runtime.spawn(async move {
        let result = generate(
            backend.clone(),
            input,
            Instant::now(),
            worker_services.clone(),
        )
        .await;
        if let Err(error) = &result {
            record_operation_error(
                &worker_services,
                "image generation",
                Some(error_input.prompt().to_owned()),
                &backend,
                error,
            );
        }
        finish_operation(
            worker_ui,
            worker_operations,
            worker_results,
            worker_services,
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

fn bind_workspace_callbacks(ui: &AppWindow, runtime: &Handle, services: &DesktopServices) {
    ui.on_navigation_changed({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        move |section| {
            if let Some(ui) = ui_weak.upgrade() {
                present_history_browser(&ui, &services);
                present_error_browser(&ui, &services);
            }
            if section == "A6 Image Studio · History" {
                load_selected_history_preview(&ui_weak, &runtime, &services);
            }
        }
    });

    ui.on_history_search_changed({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        move |query| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_history_search(query);
                present_history_browser(&ui, &services);
            }
            load_selected_history_preview(&ui_weak, &runtime, &services);
        }
    });

    ui.on_history_select_session({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        move |session_id| {
            {
                let mut selection = lock_selection(&services);
                selection.history_session = Some(session_id.to_string());
                selection.history_entry = None;
            }
            if let Some(ui) = ui_weak.upgrade() {
                present_history_browser(&ui, &services);
            }
            load_selected_history_preview(&ui_weak, &runtime, &services);
        }
    });

    ui.on_history_select_generation({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        move |entry_id| {
            let entry_id = entry_id.to_string();
            {
                let mut selection = lock_selection(&services);
                selection.history_entry = Some(entry_id.clone());
            }
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            present_history_browser(&ui, &services);
            load_selected_history_preview(&ui_weak, &runtime, &services);
        }
    });

    ui.on_history_use_selected({
        let ui_weak = ui.as_weak();
        let services = services.clone();
        move || {
            let selected = lock_selection(&services).history_entry.clone();
            let Some(entry) = selected.and_then(|id| services.history.history_entry(&id)) else {
                return;
            };
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            match entry.settings.to_input(&entry.prompt) {
                Ok(input) => {
                    present_generation_input(&ui, &input);
                    ui.set_navigation_value("A6 Image Studio · Create".into());
                    ui.set_action_status(
                        if entry.image_exists() {
                            "Loaded prompt and settings from history"
                        } else {
                            "The old image is missing; its prompt and settings were restored"
                        }
                        .into(),
                    );
                }
                Err(error) => ui.set_history_status(
                    format!(
                        "This history entry contains settings that can no longer be used: {error}"
                    )
                    .into(),
                ),
            }
        }
    });

    ui.on_history_open_selected_folder({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        move || {
            let selected = lock_selection(&services).history_entry.clone();
            let Some(entry) = selected.and_then(|id| services.history.history_entry(&id)) else {
                return;
            };
            let ui_weak = ui_weak.clone();
            runtime.spawn(async move {
                let result = actions::open_containing_path(&entry.output_path).await;
                let _ = ui_weak.upgrade_in_event_loop(move |ui| match result {
                    Ok(()) => ui.set_history_status("Opened containing folder".into()),
                    Err(error) => {
                        ui.set_history_status(format!("Could not open folder: {error}").into())
                    }
                });
            });
        }
    });

    ui.on_clear_history({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_history_status("Clearing history metadata…".into());
            }
            let worker_ui = ui_weak.clone();
            let worker_services = services.clone();
            runtime.spawn(async move {
                let repository = worker_services.history.clone();
                let result = tokio::task::spawn_blocking(move || repository.clear_history()).await;
                let _ = worker_ui.upgrade_in_event_loop(move |ui| match result {
                    Ok(Ok(())) => {
                        let mut selection = lock_selection(&worker_services);
                        selection.history_session = None;
                        selection.history_entry = None;
                        drop(selection);
                        present_history_browser(&ui, &worker_services);
                        ui.set_history_status(
                            "History metadata cleared. Generated image files were kept.".into(),
                        );
                    }
                    Ok(Err(error)) => {
                        ui.set_history_status(format!("Could not clear history: {error}").into())
                    }
                    Err(error) => ui.set_history_status(
                        format!("History cleanup worker failed: {error}").into(),
                    ),
                });
            });
        }
    });

    ui.on_error_search_changed({
        let ui_weak = ui.as_weak();
        let services = services.clone();
        move |query| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_error_search(query);
                present_error_browser(&ui, &services);
            }
        }
    });

    ui.on_error_select_entry({
        let ui_weak = ui.as_weak();
        let services = services.clone();
        move |entry_id| {
            lock_selection(&services).error_entry = Some(entry_id.to_string());
            if let Some(ui) = ui_weak.upgrade() {
                present_error_browser(&ui, &services);
            }
        }
    });

    ui.on_clear_errors({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_error_log_status("Clearing the local error log…".into());
            }
            let worker_ui = ui_weak.clone();
            let worker_services = services.clone();
            runtime.spawn(async move {
                let repository = worker_services.history.clone();
                let result = tokio::task::spawn_blocking(move || repository.clear_errors()).await;
                let _ = worker_ui.upgrade_in_event_loop(move |ui| match result {
                    Ok(Ok(())) => {
                        lock_selection(&worker_services).error_entry = None;
                        present_error_browser(&ui, &worker_services);
                        ui.set_error_log_status("Error log cleared".into());
                    }
                    Ok(Err(error)) => ui
                        .set_error_log_status(format!("Could not clear error log: {error}").into()),
                    Err(error) => ui.set_error_log_status(
                        format!("Error-log cleanup worker failed: {error}").into(),
                    ),
                });
            });
        }
    });

    ui.on_export_diagnostics({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        move || {
            let api_key = lock_credential(&services)
                .as_ref()
                .map(|credential| credential.key.expose().to_owned());
            let report = match diagnostics::export_report(
                &services.session_id,
                services.history.errors(),
                api_key.as_deref(),
            ) {
                Ok(report) => report,
                Err(error) => {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_error_log_status(
                            format!("Could not prepare diagnostic report: {error}").into(),
                        );
                    }
                    return;
                }
            };
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_error_log_status("Opening diagnostic export dialog…".into());
            }
            let worker_ui = ui_weak.clone();
            runtime.spawn(async move {
                let result = actions::export_diagnostics(report).await;
                let _ = worker_ui.upgrade_in_event_loop(move |ui| match result {
                    Ok(Some(path)) => ui.set_error_log_status(
                        format!("Sanitized diagnostics saved to {}", path.display()).into(),
                    ),
                    Ok(None) => ui.set_error_log_status("Diagnostic export cancelled".into()),
                    Err(error) => ui.set_error_log_status(
                        format!("Could not export diagnostics: {error}").into(),
                    ),
                });
            });
        }
    });

    bind_settings_callbacks(ui, runtime, services);
}

fn load_selected_history_preview(
    ui_weak: &slint::Weak<AppWindow>,
    runtime: &Handle,
    services: &DesktopServices,
) {
    let Some(entry_id) = lock_selection(services).history_entry.clone() else {
        return;
    };
    let Some(entry) = services.history.history_entry(&entry_id) else {
        return;
    };
    let Some(ui) = ui_weak.upgrade() else {
        return;
    };
    if !entry.image_exists() {
        ui.set_history_preview(Image::default());
        ui.set_history_file_available(false);
        ui.set_history_status(
            "The selected image file was deleted or moved; prompt and metadata remain available."
                .into(),
        );
        return;
    }
    ui.set_history_status("Loading image preview…".into());
    let worker_ui = ui_weak.clone();
    let worker_services = services.clone();
    runtime.spawn(async move {
        let result = storage::load_preview(&entry.output_path).await;
        let scheduled = worker_ui.upgrade_in_event_loop(move |ui| {
            let still_selected =
                lock_selection(&worker_services).history_entry.as_deref() == Some(entry_id.as_str());
            if !still_selected {
                return;
            }
            match result {
                Ok(preview) => {
                    let pixels = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                        &preview.rgba,
                        preview.width,
                        preview.height,
                    );
                    ui.set_history_preview(Image::from_rgba8(pixels));
                    ui.set_history_file_available(true);
                    ui.set_history_status(SharedString::default());
                }
                Err(error) => {
                    ui.set_history_preview(Image::default());
                    ui.set_history_file_available(false);
                    ui.set_history_status(
                        format!(
                            "The image file can no longer be read. Prompt and metadata are still available: {error}"
                        )
                        .into(),
                    );
                }
            }
        });
        if let Err(error) = scheduled {
            tracing::debug!(%error, "history preview dropped after event loop exit");
        }
    });
}

fn bind_settings_callbacks(ui: &AppWindow, runtime: &Handle, services: &DesktopServices) {
    ui.on_choose_output_directory({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_settings_busy(true);
                ui.set_settings_status("Opening folder chooser…".into());
            }
            let worker_ui = ui_weak.clone();
            runtime.spawn(async move {
                let selected = actions::choose_output_directory().await;
                let _ = worker_ui.upgrade_in_event_loop(move |ui| {
                    ui.set_settings_busy(false);
                    match selected {
                        Some(path) => {
                            ui.set_settings_output_directory(path.display().to_string().into());
                            ui.set_settings_status(
                                "Directory selected. Save and apply settings to persist it.".into(),
                            );
                        }
                        None => ui.set_settings_status("Folder selection cancelled".into()),
                    }
                });
            });
        }
    });

    ui.on_save_settings({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let preferences = match preferences_from_ui(&ui) {
                Ok(preferences) => preferences,
                Err(error) => {
                    ui.set_settings_status(error.into());
                    return;
                }
            };
            ui.set_settings_busy(true);
            ui.set_settings_status("Saving and applying settings…".into());
            drop(ui);
            let worker_ui = ui_weak.clone();
            let worker_services = services.clone();
            runtime.spawn(async move {
                let store = worker_services.preferences_store.clone();
                let to_save = preferences.clone();
                let save_result = tokio::task::spawn_blocking(move || store.save(&to_save)).await;
                let outcome = match save_result {
                    Ok(Ok(())) => {
                        *lock_preferences_mut(&worker_services) = preferences.clone();
                        let credential = lock_credential(&worker_services).clone();
                        let backend = build_backend(&preferences, credential.as_ref());
                        worker_services.backend.replace(backend.clone());
                        Ok((preferences, credential, backend))
                    }
                    Ok(Err(error)) => Err(format!("Could not save settings: {error}")),
                    Err(error) => Err(format!("Settings worker failed: {error}")),
                };
                let _ = worker_ui.upgrade_in_event_loop(move |ui| {
                    ui.set_settings_busy(false);
                    match outcome {
                        Ok((preferences, credential, backend)) => {
                            present_preferences(&ui, &preferences);
                            present_backend_configuration(
                                &ui,
                                &preferences,
                                credential.as_ref(),
                                &backend,
                            );
                            ui.set_settings_status(
                                "Settings saved and applied. New requests use these values.".into(),
                            );
                        }
                        Err(error) => ui.set_settings_status(error.into()),
                    }
                });
            });
        }
    });

    ui.on_store_api_key({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let secret = ui.get_settings_api_key_entry().to_string();
            ui.set_settings_api_key_entry(SharedString::default());
            ui.set_settings_busy(true);
            ui.set_settings_status("Storing key in the system keyring…".into());
            drop(ui);
            let worker_ui = ui_weak.clone();
            let worker_services = services.clone();
            runtime.spawn(async move {
                let stored =
                    tokio::task::spawn_blocking(move || secrets::store_keyring_key(secret)).await;
                let outcome = match stored {
                    Ok(Ok(stored_key)) => {
                        let active = match secrets::environment_key() {
                            Ok(Some(environment)) => environment,
                            Ok(None) => ResolvedApiKey {
                                key: stored_key,
                                source: ApiKeySource::Keyring,
                            },
                            Err(error) => {
                                return schedule_settings_error(
                                    worker_ui,
                                    format!("Key stored, but the active environment key is invalid: {error}"),
                                );
                            }
                        };
                        *lock_credential_mut(&worker_services) = Some(active.clone());
                        let preferences = lock_preferences(&worker_services).clone();
                        let backend = build_backend(&preferences, Some(&active));
                        worker_services.backend.replace(backend.clone());
                        Ok((preferences, active, backend))
                    }
                    Ok(Err(error)) => Err(format!("Could not store key securely: {error}")),
                    Err(error) => Err(format!("Keyring worker failed: {error}")),
                };
                let _ = worker_ui.upgrade_in_event_loop(move |ui| {
                    ui.set_settings_busy(false);
                    match outcome {
                        Ok((preferences, active, backend)) => {
                            present_backend_configuration(
                                &ui,
                                &preferences,
                                Some(&active),
                                &backend,
                            );
                            ui.set_settings_status(
                                if active.source == ApiKeySource::Environment {
                                    "Key stored securely. A6API_KEY remains the active source until it is unset."
                                } else {
                                    "Key stored securely and activated from the system keyring."
                                }
                                .into(),
                            );
                        }
                        Err(error) => ui.set_settings_status(error.into()),
                    }
                });
            });
        }
    });

    ui.on_forget_api_key({
        let ui_weak = ui.as_weak();
        let runtime = runtime.clone();
        let services = services.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_settings_busy(true);
                ui.set_settings_status("Removing the stored key from the system keyring…".into());
            }
            let worker_ui = ui_weak.clone();
            let worker_services = services.clone();
            runtime.spawn(async move {
                let deleted =
                    tokio::task::spawn_blocking(secrets::forget_keyring_key).await;
                let outcome = match deleted {
                    Ok(Ok(was_present)) => {
                        match secrets::environment_key() {
                            Ok(active) => {
                                *lock_credential_mut(&worker_services) = active.clone();
                                let preferences = lock_preferences(&worker_services).clone();
                                let backend = build_backend(&preferences, active.as_ref());
                                worker_services.backend.replace(backend.clone());
                                Ok((preferences, active, backend, was_present))
                            }
                            Err(error) => Err(error.to_string()),
                        }
                    }
                    Ok(Err(error)) => Err(format!("Could not forget stored key: {error}")),
                    Err(error) => Err(format!("Keyring worker failed: {error}")),
                };
                let _ = worker_ui.upgrade_in_event_loop(move |ui| {
                    ui.set_settings_busy(false);
                    match outcome {
                        Ok((preferences, active, backend, was_present)) => {
                            present_backend_configuration(
                                &ui,
                                &preferences,
                                active.as_ref(),
                                &backend,
                            );
                            ui.set_settings_status(
                                match (was_present, active.is_some()) {
                                    (true, true) => {
                                        "Stored key forgotten. A6API_KEY is still active."
                                    }
                                    (true, false) => {
                                        "Stored key forgotten. No API key is currently available."
                                    }
                                    (false, true) => {
                                        "No stored key existed. A6API_KEY remains active."
                                    }
                                    (false, false) => {
                                        "No stored key existed and no API key is currently available."
                                    }
                                }
                                .into(),
                            );
                        }
                        Err(error) => ui.set_settings_status(error.into()),
                    }
                });
            });
        }
    });
}

fn schedule_settings_error(ui_weak: slint::Weak<AppWindow>, message: String) {
    let _ = ui_weak.upgrade_in_event_loop(move |ui| {
        ui.set_settings_busy(false);
        ui.set_settings_status(message.into());
    });
}

fn preferences_from_ui(ui: &AppWindow) -> Result<AppPreferences, String> {
    let timeout = u64::try_from(ui.get_settings_request_timeout())
        .map_err(|_| "Request timeout must be a positive number".to_owned())?;
    let preferences = AppPreferences {
        base_url: ui.get_settings_base_url().trim().to_owned(),
        model: ui.get_settings_model_id().trim().to_owned(),
        default_size: ui.get_settings_default_size().to_string(),
        default_quality: ui.get_settings_default_quality().to_string(),
        default_format: ui.get_settings_default_format().to_string(),
        output_directory: PathBuf::from(ui.get_settings_output_directory().as_str()),
        request_timeout_seconds: timeout,
        retain_history: ui.get_settings_retain_history(),
    };
    preferences.validate().map_err(|error| error.to_string())?;
    Ok(preferences)
}

fn build_backend(
    preferences: &AppPreferences,
    credential: Option<&ResolvedApiKey>,
) -> Result<Backend, String> {
    let credential = credential.ok_or_else(|| {
        "No API key is available. Set A6API_KEY or store one securely in Settings.".to_owned()
    })?;
    let (base_url, model, _) = effective_endpoint_and_model(preferences);
    let config = Config::with_timeout(
        credential.key.clone(),
        &base_url,
        &model,
        Duration::from_secs(preferences.request_timeout_seconds),
    )
    .map_err(|error| error.to_string())?;
    let client =
        ApiClient::new(config.clone()).map_err(|error| format!("{}: {error}", error.category()))?;
    Ok(Backend {
        client,
        model: config.model().to_owned(),
        endpoint: config.sanitized_base_url(),
        output_directory: preferences.output_directory.clone(),
    })
}

fn present_backend_configuration(
    ui: &AppWindow,
    preferences: &AppPreferences,
    credential: Option<&ResolvedApiKey>,
    backend: &Result<Backend, String>,
) {
    let (base_url, model, note) = effective_endpoint_and_model(preferences);
    ui.set_endpoint_text(fallback_endpoint_label(&base_url).into());
    ui.set_model_text(model.into());
    ui.set_settings_override_note(note.into());
    match credential {
        Some(credential) => {
            let masked = credential.key.masked();
            ui.set_api_key_status(format!("{masked} ({})", credential.source.label()).into());
            ui.set_settings_api_key_source(credential.source.label().into());
            ui.set_settings_api_key_mask(masked.into());
        }
        None => {
            ui.set_api_key_status("Not available".into());
            ui.set_settings_api_key_source("none".into());
            ui.set_settings_api_key_mask(SharedString::default());
        }
    }
    match backend {
        Ok(backend) => {
            ui.set_endpoint_text(backend.endpoint.clone().into());
            ui.set_model_text(backend.model.clone().into());
            ui.set_configured(true);
            ui.set_connection_status("Ready · settings applied".into());
            ui.set_error_text(SharedString::default());
        }
        Err(error) => {
            ui.set_configured(false);
            ui.set_connection_status("Configuration required".into());
            ui.set_error_text(error.clone().into());
        }
    }
}

fn present_history_browser(ui: &AppWindow, services: &DesktopServices) {
    let query = ui.get_history_search().trim().to_lowercase();
    let sessions = services.history.sessions(&query);
    let selected_session = {
        let mut selection = lock_selection(services);
        let selected_is_visible = selection
            .history_session
            .as_ref()
            .is_some_and(|selected| sessions.iter().any(|session| &session.id == selected));
        if !selected_is_visible {
            selection.history_session = sessions.first().map(|session| session.id.clone());
            selection.history_entry = None;
        }
        selection.history_session.clone()
    };
    ui.set_history_sessions(ModelRc::new(VecModel::from(
        sessions
            .iter()
            .map(|session| HistorySessionItem {
                id: session.id.clone().into(),
                title: display_timestamp(&session.timestamp).into(),
                subtitle: format!(
                    "{} generation{}  ·  {}",
                    session.count,
                    if session.count == 1 { "" } else { "s" },
                    short_session_id(&session.id)
                )
                .into(),
                selected: selected_session.as_deref() == Some(session.id.as_str()),
            })
            .collect::<Vec<_>>(),
    )));

    let entries = services
        .history
        .history()
        .into_iter()
        .filter(|entry| {
            selected_session.as_deref() == Some(entry.session_id.as_str())
                && (query.is_empty() || history_matches(entry, &query))
        })
        .collect::<Vec<_>>();
    let selected_entry = {
        let mut selection = lock_selection(services);
        let selected_is_visible = selection
            .history_entry
            .as_ref()
            .is_some_and(|selected| entries.iter().any(|entry| &entry.id == selected));
        if !selected_is_visible {
            selection.history_entry = entries.first().map(|entry| entry.id.clone());
        }
        selection.history_entry.clone()
    };
    ui.set_history_generations(ModelRc::new(VecModel::from(
        entries
            .iter()
            .map(|entry| HistoryGenerationItem {
                id: entry.id.clone().into(),
                timestamp: display_timestamp(&entry.timestamp).into(),
                prompt: entry.prompt.clone().into(),
                model: entry.model.clone().into(),
                dimensions: format!("{}×{}", entry.width, entry.height).into(),
                available: entry.image_exists(),
                selected: selected_entry.as_deref() == Some(entry.id.as_str()),
            })
            .collect::<Vec<_>>(),
    )));

    match selected_entry.and_then(|id| services.history.history_entry(&id)) {
        Some(entry) => present_history_entry(ui, &entry),
        None => clear_history_selection(ui),
    }
}

fn present_history_entry(ui: &AppWindow, entry: &HistoryEntry) {
    let available = entry.image_exists();
    ui.set_history_has_selection(true);
    ui.set_history_file_available(available);
    ui.set_history_preview(Image::default());
    ui.set_history_selected_timestamp(display_timestamp(&entry.timestamp).into());
    ui.set_history_selected_session(short_session_id(&entry.session_id).into());
    ui.set_history_selected_prompt(entry.prompt.clone().into());
    ui.set_history_selected_model(entry.model.clone().into());
    ui.set_history_selected_settings(
        format!(
            "{} · {} · {} · {}",
            entry.settings.size,
            entry.settings.quality,
            entry.settings.background,
            entry.settings.output_format
        )
        .into(),
    );
    ui.set_history_selected_dimensions(format!("{} × {}", entry.width, entry.height).into());
    ui.set_history_selected_path(entry.output_path.display().to_string().into());
    ui.set_history_selected_request_id(
        entry.request_id.as_deref().unwrap_or("Not provided").into(),
    );
}

fn clear_history_selection(ui: &AppWindow) {
    ui.set_history_has_selection(false);
    ui.set_history_file_available(false);
    ui.set_history_preview(Image::default());
    ui.set_history_selected_timestamp(SharedString::default());
    ui.set_history_selected_session(SharedString::default());
    ui.set_history_selected_prompt(SharedString::default());
    ui.set_history_selected_model(SharedString::default());
    ui.set_history_selected_settings(SharedString::default());
    ui.set_history_selected_dimensions(SharedString::default());
    ui.set_history_selected_path(SharedString::default());
    ui.set_history_selected_request_id(SharedString::default());
}

fn present_error_browser(ui: &AppWindow, services: &DesktopServices) {
    let query = ui.get_error_search().to_string();
    let entries = services
        .history
        .errors()
        .into_iter()
        .filter(|entry| error_matches(entry, &query))
        .collect::<Vec<_>>();
    let selected = {
        let mut selection = lock_selection(services);
        let selected_is_visible = selection
            .error_entry
            .as_ref()
            .is_some_and(|selected| entries.iter().any(|entry| &entry.id == selected));
        if !selected_is_visible {
            selection.error_entry = entries.first().map(|entry| entry.id.clone());
        }
        selection.error_entry.clone()
    };
    ui.set_error_log_items(ModelRc::new(VecModel::from(
        entries
            .iter()
            .map(|entry| ErrorLogItem {
                id: entry.id.clone().into(),
                timestamp: display_timestamp(&entry.timestamp).into(),
                category: entry.category.clone().into(),
                summary: entry.summary.clone().into(),
                selected: selected.as_deref() == Some(entry.id.as_str()),
            })
            .collect::<Vec<_>>(),
    )));
    match selected.and_then(|id| services.history.error_entry(&id)) {
        Some(entry) => {
            ui.set_error_has_selection(true);
            ui.set_error_selected_timestamp(display_timestamp(&entry.timestamp).into());
            ui.set_error_selected_session(short_session_id(&entry.session_id).into());
            ui.set_error_selected_operation(entry.operation.into());
            ui.set_error_selected_category(entry.category.into());
            ui.set_error_selected_prompt(
                entry.prompt.as_deref().unwrap_or("Not applicable").into(),
            );
            ui.set_error_selected_model(entry.model.into());
            ui.set_error_selected_endpoint(entry.endpoint.into());
            ui.set_error_selected_request_id(
                entry.request_id.as_deref().unwrap_or("Not provided").into(),
            );
            ui.set_error_selected_summary(entry.summary.into());
            ui.set_error_selected_full_response(entry.full_response.unwrap_or_default().into());
            ui.set_error_selected_response_truncated(entry.response_truncated);
        }
        None => {
            ui.set_error_has_selection(false);
            ui.set_error_selected_full_response(SharedString::default());
        }
    }
}

fn short_session_id(session_id: &str) -> String {
    session_id
        .strip_prefix("session-")
        .unwrap_or(session_id)
        .chars()
        .take(22)
        .collect()
}

fn lock_preferences(services: &DesktopServices) -> MutexGuard<'_, AppPreferences> {
    match services.preferences.lock() {
        Ok(value) => value,
        Err(poisoned) => {
            tracing::error!("preferences lock was poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

fn lock_preferences_mut(services: &DesktopServices) -> MutexGuard<'_, AppPreferences> {
    lock_preferences(services)
}

fn lock_credential(services: &DesktopServices) -> MutexGuard<'_, Option<ResolvedApiKey>> {
    match services.credential.lock() {
        Ok(value) => value,
        Err(poisoned) => {
            tracing::error!("credential lock was poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

fn lock_credential_mut(services: &DesktopServices) -> MutexGuard<'_, Option<ResolvedApiKey>> {
    lock_credential(services)
}

fn lock_selection(services: &DesktopServices) -> MutexGuard<'_, BrowserSelection> {
    match services.selection.lock() {
        Ok(value) => value,
        Err(poisoned) => {
            tracing::error!("browser selection lock was poisoned; recovering");
            poisoned.into_inner()
        }
    }
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
    services: DesktopServices,
) -> Result<OperationResult, OperationError> {
    let request =
        ImageGenerationRequest::configured(&backend.model, input.prompt(), input.options());
    let generated = backend.client.generate_image(&request).await?;
    let output_format = input.options().output_format;
    let saved =
        storage::save_with_preview_in(generated.bytes, output_format, &backend.output_directory)
            .await?;
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
    let retain_history = lock_preferences(&services).retain_history;
    let history_warning = if retain_history {
        match HistoryEntry::from_generated(&services.session_id, &backend.model, &image)
            .and_then(|entry| services.history.append_history(entry))
        {
            Ok(()) => None,
            Err(error) => {
                tracing::error!(%error, "generated image was saved but history recording failed");
                Some(format!(
                    "Image saved, but local history could not be updated: {error}"
                ))
            }
        }
    } else {
        None
    };

    Ok(OperationResult::Generation(GenerationResult {
        image,
        pixels,
        history_warning,
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

impl OperationError {
    fn category(&self) -> String {
        match self {
            Self::Api { category, .. } => (*category).to_owned(),
            Self::Storage(_) => "local output error".to_owned(),
        }
    }

    fn response_details(&self) -> (Option<String>, bool, Option<String>) {
        match self {
            Self::Api { source, .. } => {
                let (body, truncated) = source
                    .full_response()
                    .map_or((None, false), |(body, truncated)| {
                        (Some(body.to_owned()), truncated)
                    });
                let request_id = source
                    .metadata()
                    .and_then(|metadata| metadata.request_id.clone());
                (body, truncated, request_id)
            }
            Self::Storage(_) => (None, false, None),
        }
    }
}

fn record_operation_error(
    services: &DesktopServices,
    operation: &str,
    prompt: Option<String>,
    backend: &Backend,
    error: &OperationError,
) {
    let (full_response, response_truncated, request_id) = error.response_details();
    let entry = ErrorLogEntry::new(
        &services.session_id,
        operation,
        prompt,
        backend.model.clone(),
        backend.endpoint.clone(),
        error.category(),
        error.to_string(),
        full_response,
        response_truncated,
        request_id,
    );
    match entry.and_then(|entry| services.history.append_error(entry)) {
        Ok(()) => {}
        Err(log_error) => {
            tracing::error!(%log_error, original_error = %error, "failed to persist error log entry")
        }
    }
}

fn record_local_error(
    services: &DesktopServices,
    operation: &str,
    prompt: Option<String>,
    category: &str,
    summary: String,
) {
    let preferences = lock_preferences(services).clone();
    let (endpoint, model, _) = effective_endpoint_and_model(&preferences);
    let entry = ErrorLogEntry::new(
        &services.session_id,
        operation,
        prompt,
        model,
        endpoint,
        category.to_owned(),
        summary,
        None,
        false,
        None,
    );
    if let Err(error) = entry.and_then(|entry| services.history.append_error(entry)) {
        tracing::error!(%error, "failed to persist local error log entry");
    }
}

fn finish_operation(
    ui_weak: slint::Weak<AppWindow>,
    operations: OperationControl,
    results: ResultStore,
    services: DesktopServices,
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
            let history_warning = generated.history_warning.clone();
            let success = SuccessResult::Generation(generated.image.clone());
            if let Some(state) = operations.succeed(operation, success) {
                results.push(generated);
                present_state(&ui, &state);
                if let Some(selected) = results.selected() {
                    present_selected_result(&ui, &selected);
                }
                present_recent_results(&ui, &results);
                present_history_browser(&ui, &services);
                if let Some(warning) = history_warning {
                    ui.set_action_status(warning.into());
                }
            }
        }
        Err(error) => {
            if let Some(state) = operations.fail(operation, error.to_string()) {
                present_state(&ui, &state);
                present_error_browser(&ui, &services);
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
                    "Image generated · provider-adjusted size"
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
        if same_aspect_ratio(
            requested.width(),
            requested.height(),
            received_width,
            received_height,
        ) {
            tracing::info!(
                requested_width = requested.width(),
                requested_height = requested.height(),
                received_width,
                received_height,
                request_id,
                "provider adjusted image resolution while preserving aspect ratio"
            );
        } else {
            tracing::warn!(
                requested_width = requested.width(),
                requested_height = requested.height(),
                received_width,
                received_height,
                request_id,
                "provider returned a different image size and aspect ratio"
            );
        }
    }
}

fn dimension_integrity_warning(image: &GeneratedImage) -> String {
    match (
        image.requested_dimensions(),
        image.dimensions_match_request(),
    ) {
        (Some(requested), Some(false))
            if same_aspect_ratio(
                requested.width(),
                requested.height(),
                image.width(),
                image.height(),
            ) =>
        {
            format!(
                "Provider-adjusted resolution: requested {}×{}, received {}×{}. \
                 The requested aspect ratio was preserved and the response was saved unchanged.",
                requested.width(),
                requested.height(),
                image.width(),
                image.height()
            )
        }
        (Some(requested), Some(false)) => {
            format!(
                "Provider changed size and aspect ratio: requested {}×{}, received {}×{}. \
                 The response was saved unchanged; no stretching or artificial upscaling was applied.",
                requested.width(),
                requested.height(),
                image.width(),
                image.height()
            )
        }
        _ => String::new(),
    }
}

fn same_aspect_ratio(
    left_width: u32,
    left_height: u32,
    right_width: u32,
    right_height: u32,
) -> bool {
    let left = f64::from(left_width) / f64::from(left_height);
    let right = f64::from(right_width) / f64::from(right_height);
    ((left - right).abs() / left).is_finite() && (left - right).abs() / left <= 0.002
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
        assert!(warning.contains("received 1402×1122"));
        assert!(warning.contains("saved unchanged"));
    }

    #[test]
    fn treats_provider_rounding_as_a_preserved_aspect_ratio() {
        assert!(same_aspect_ratio(3_840, 2_160, 1_672, 941));
        assert!(!same_aspect_ratio(3_840, 2_160, 1_402, 1_122));
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
    fn startup_blocking_work_enters_the_runtime_before_spawning() {
        let runtime = build_worker_runtime().expect("worker runtime should build");

        let value = run_blocking_on_runtime(&runtime, || 42)
            .expect("startup blocking work should run without a prior runtime context");

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
            history_warning: None,
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
        let (operation, _) = operations
            .try_begin(OperationKind::Generating)
            .expect("idle controller should begin an operation");
        let task = tokio::spawn(future::pending::<()>());
        operations.attach(operation, task.abort_handle());

        let state = operations.cancel().expect("active operation should cancel");
        assert_eq!(state, ApplicationState::Cancelled { operation });

        let join_error = task
            .await
            .expect_err("cancelled worker task should not complete");
        assert!(join_error.is_cancelled());
    }

    #[tokio::test]
    async fn duplicate_operation_is_rejected_without_replacing_the_active_worker() {
        let operations = OperationControl::new(ApplicationState::Idle);
        let (operation, _) = operations
            .try_begin(OperationKind::Generating)
            .expect("idle controller should begin an operation");
        let task = tokio::spawn(future::pending::<()>());
        operations.attach(operation, task.abort_handle());

        assert!(operations.try_begin(OperationKind::Connecting).is_none());
        assert!(!task.is_finished());

        operations.shutdown();
        assert!(
            task.await
                .expect_err("shutdown should abort the active worker")
                .is_cancelled()
        );
    }
}
