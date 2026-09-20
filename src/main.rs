use std::future::Future;
use std::process::ExitCode;
use std::time::Instant;

use a6_image_studio::api::{ApiClient, ApiError, ImageGenerationRequest};
use a6_image_studio::app;
use a6_image_studio::config::{Config, ConfigError};
use a6_image_studio::storage::{self, StorageError};
use clap::{Parser, Subcommand};
use thiserror::Error;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "a6-image-studio",
    version,
    about = "Generate images through the A6API gateway"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Launch the Slint desktop interface (also the default with no command).
    Gui,
    /// Check gateway reachability, authentication, and model availability.
    Check,
    /// Make one billable, low-quality image-generation smoke test.
    SmokeGenerate {
        /// Confirm that a billable API request may be made.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Error)]
enum CliError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Gui(#[from] app::GuiError),
    #[error("failed to create the command-line async runtime: {0}")]
    Runtime(#[from] std::io::Error),
    #[error("confirmation is required; re-run with --yes to make the billable request")]
    ConfirmationRequired,
}

fn main() -> ExitCode {
    slint::init_translations!("/usr/share/locale");

    if let Err(error) = init_diagnostics() {
        eprintln!("failed to initialize diagnostics: {error}");
        return ExitCode::FAILURE;
    }

    let cli = Cli::parse();
    let result = match cli.command.unwrap_or(Command::Gui) {
        Command::Gui => app::run().map_err(CliError::from),
        Command::Check => run_cli_future(run_check()),
        Command::SmokeGenerate { yes } => run_cli_future(run_smoke_generate(yes)),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report_error(&error);
            if matches!(error, CliError::ConfirmationRequired) {
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn run_cli_future<F>(future: F) -> Result<(), CliError>
where
    F: Future<Output = Result<(), CliError>>,
{
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("a6-image-cli")
        .build()?
        .block_on(future)
}

fn init_diagnostics() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .try_init()
}

async fn run_check() -> Result<(), CliError> {
    let config = Config::from_env()?;
    tracing::debug!(config = ?config, "configuration loaded");
    println!("Base URL: {}", config.sanitized_base_url());
    println!("API key: {}", config.api_key().masked());
    println!("Model: {}", config.model());

    let client = ApiClient::new(config.clone())?;
    let result = client.check_models().await?;

    println!("HTTP result: {}", result.http_status);
    print_metadata(&result.metadata);
    if result.model_present {
        println!("Model availability: {} is listed", config.model());
    } else {
        println!(
            "Model availability: {} is not listed (generation may still be supported)",
            config.model()
        );
    }
    Ok(())
}

async fn run_smoke_generate(confirmed: bool) -> Result<(), CliError> {
    eprintln!("This command is about to make a billable image-generation request.");
    if !confirmed {
        return Err(CliError::ConfirmationRequired);
    }

    let config = Config::from_env()?;
    tracing::debug!(config = ?config, "configuration loaded");
    println!("Base URL: {}", config.sanitized_base_url());
    println!("Model: {}", config.model());

    let client = ApiClient::new(config.clone())?;
    let request = ImageGenerationRequest::smoke_test(config.model());
    let started = Instant::now();
    let generated = client.generate_image(&request).await?;
    let saved = storage::save_png(generated.bytes).await?;

    println!("Output path: {}", saved.path.display());
    println!("Dimensions: {}x{}", saved.width, saved.height);
    println!("File size: {} bytes", saved.file_size);
    print_metadata(&generated.metadata);
    println!("Elapsed time: {:.2?}", started.elapsed());
    Ok(())
}

fn print_metadata(metadata: &a6_image_studio::api::ResponseMetadata) {
    for line in metadata_lines(metadata) {
        println!("{line}");
    }
}

fn eprint_metadata(metadata: &a6_image_studio::api::ResponseMetadata) {
    for line in metadata_lines(metadata) {
        eprintln!("{line}");
    }
}

fn metadata_lines(metadata: &a6_image_studio::api::ResponseMetadata) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(request_id) = &metadata.request_id {
        lines.push(format!("Request ID: {request_id}"));
    }
    if let Some(retry_after) = &metadata.retry_after {
        lines.push(format!("Retry-After: {retry_after}"));
    }
    if metadata.rate_limit_limit.is_some()
        || metadata.rate_limit_remaining.is_some()
        || metadata.rate_limit_reset.is_some()
    {
        lines.push(format!(
            "Rate limit: limit={}, remaining={}, reset={}",
            metadata.rate_limit_limit.as_deref().unwrap_or("unknown"),
            metadata
                .rate_limit_remaining
                .as_deref()
                .unwrap_or("unknown"),
            metadata.rate_limit_reset.as_deref().unwrap_or("unknown")
        ));
    }
    lines
}

fn report_error(error: &CliError) {
    match error {
        CliError::Api(api_error) => {
            eprintln!("error [{}]: {api_error}", api_error.category());
            if let Some(metadata) = api_error.metadata() {
                eprint_metadata(metadata);
            }
            if matches!(api_error, ApiError::UnsupportedEndpoint { .. }) {
                eprintln!(
                    "The models endpoint is unsupported; this does not prove image generation is unavailable."
                );
            }
        }
        CliError::Config(config_error) => eprintln!("configuration error: {config_error}"),
        CliError::Storage(storage_error) => eprintln!("output error: {storage_error}"),
        CliError::Gui(gui_error) => eprintln!("desktop error: {gui_error}"),
        CliError::Runtime(runtime_error) => eprintln!("runtime error: {runtime_error}"),
        CliError::ConfirmationRequired => eprintln!("error: {error}"),
    }
}
