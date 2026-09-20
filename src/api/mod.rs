//! OpenAI-compatible gateway transport, request types, and error metadata.
//!
//! This module owns authenticated calls to the configured API root. Bearer
//! credentials are never attached to provider-returned image download URLs.

mod client;
mod error;
mod types;

pub use client::ApiClient;
pub use error::{ApiError, HttpFailure, NetworkFailure};
pub use types::{ImageGenerationOutput, ImageGenerationRequest, ModelsCheck, ResponseMetadata};
