//! Application domain types shared by presentation and orchestration code.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::api::ResponseMetadata;

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
    preview_rgba: Arc<[u8]>,
}

impl GeneratedImage {
    pub(crate) fn new(
        path: PathBuf,
        width: u32,
        height: u32,
        file_size: u64,
        elapsed: Duration,
        response_metadata: ResponseMetadata,
        preview_rgba: Vec<u8>,
    ) -> Self {
        Self {
            path,
            width,
            height,
            file_size,
            elapsed,
            response_metadata,
            preview_rgba: preview_rgba.into(),
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

    pub(crate) fn preview_rgba(&self) -> &[u8] {
        &self.preview_rgba
    }
}
