//! Application domain types shared by presentation and orchestration code.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::api::ResponseMetadata;
use crate::generation::{GenerationInput, ImageDimensions, OutputFormat};

/// A validated image that was generated, persisted, and prepared for preview.
///
/// Transport response bytes are deliberately not exposed as the application
/// model. Once construction succeeds, this type records the durable output and
/// the metadata needed by both the state machine and the desktop presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedImage {
    path: PathBuf,
    width: u32,
    height: u32,
    file_size: u64,
    elapsed: Duration,
    response_metadata: ResponseMetadata,
    request: GenerationInput,
    output_format: OutputFormat,
}

/// Validated storage result used to construct the durable domain object.
pub(crate) struct PersistedImage {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub output_format: OutputFormat,
}

impl GeneratedImage {
    pub(crate) fn new(
        persisted: PersistedImage,
        elapsed: Duration,
        response_metadata: ResponseMetadata,
        request: GenerationInput,
    ) -> Self {
        Self {
            path: persisted.path,
            width: persisted.width,
            height: persisted.height,
            file_size: persisted.file_size,
            elapsed,
            response_metadata,
            request,
            output_format: persisted.output_format,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    pub fn response_metadata(&self) -> &ResponseMetadata {
        &self.response_metadata
    }

    pub fn request(&self) -> &GenerationInput {
        &self.request
    }

    pub fn output_format(&self) -> OutputFormat {
        self.output_format
    }

    /// Returns the exact dimensions sent to the gateway, when the request
    /// included an explicit `size` field.
    pub fn requested_dimensions(&self) -> Option<ImageDimensions> {
        self.request()
            .options()
            .compatibility
            .send_size
            .then(|| self.request().options().size.explicit_dimensions())
            .flatten()
    }

    /// Reports whether a gateway response honored an explicitly requested
    /// size. `None` means that the request used `auto` or omitted `size`.
    pub fn dimensions_match_request(&self) -> Option<bool> {
        self.requested_dimensions()
            .map(|requested| requested.width() == self.width && requested.height() == self.height)
    }
}
