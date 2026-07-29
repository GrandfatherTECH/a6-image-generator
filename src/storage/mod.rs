use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use image::{DynamicImage, GenericImageView, ImageFormat, Rgb, RgbImage};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::generation::OutputFormat;
use crate::xdg::{AppPaths, XdgPathError};

static OUTPUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const PREVIEW_MAX_EDGE: u32 = 1_600;

struct TemporaryOutput {
    path: PathBuf,
    committed: bool,
}

impl TemporaryOutput {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    fn mark_committed(&mut self) {
        self.committed = true;
    }
}

impl Drop for TemporaryOutput {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                %error,
                "failed to remove incomplete output file"
            );
        }
    }
}

#[derive(Debug)]
pub struct SavedImage {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
}

#[derive(Debug)]
pub struct SavedImagePreview {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub preview_width: u32,
    pub preview_height: u32,
    pub preview_rgba: Vec<u8>,
}

#[derive(Debug)]
pub struct LoadedImagePreview {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("the application output directory could not be determined")]
    OutputDirectoryUnavailable(#[from] XdgPathError),
    #[error("image validation or conversion failed: {0}")]
    InvalidImage(#[from] image::ImageError),
    #[error("background image processing task failed: {0}")]
    ProcessingTask(#[from] tokio::task::JoinError),
    #[error("failed to create, write, or commit output file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("system clock is earlier than the Unix epoch")]
    InvalidSystemTime,
}

pub async fn save_png(bytes: Vec<u8>) -> Result<SavedImage, StorageError> {
    let prepared =
        tokio::task::spawn_blocking(move || prepare_image(&bytes, OutputFormat::Png, false))
            .await??;
    let path = write_output(&prepared.encoded, "smoke-test", OutputFormat::Png).await?;

    Ok(SavedImage {
        path,
        width: prepared.width,
        height: prepared.height,
        file_size: prepared.encoded.len() as u64,
    })
}

pub async fn save_with_preview(
    bytes: Vec<u8>,
    output_format: OutputFormat,
) -> Result<SavedImagePreview, StorageError> {
    let output_directory = output_directory()?;
    save_with_preview_in(bytes, output_format, &output_directory).await
}

pub async fn save_with_preview_in(
    bytes: Vec<u8>,
    output_format: OutputFormat,
    output_directory: &Path,
) -> Result<SavedImagePreview, StorageError> {
    let prepared =
        tokio::task::spawn_blocking(move || prepare_image(&bytes, output_format, true)).await??;
    let path = write_output_in(
        output_directory,
        &prepared.encoded,
        "generated",
        output_format,
    )
    .await?;

    Ok(SavedImagePreview {
        path,
        width: prepared.width,
        height: prepared.height,
        file_size: prepared.encoded.len() as u64,
        preview_width: prepared.preview.as_ref().map_or(0, |preview| preview.width),
        preview_height: prepared
            .preview
            .as_ref()
            .map_or(0, |preview| preview.height),
        preview_rgba: prepared
            .preview
            .map_or_else(Vec::new, |preview| preview.rgba),
    })
}

pub async fn copy_atomic(source: &Path, destination: &Path) -> Result<(), StorageError> {
    if source == destination {
        return Ok(());
    }
    let bytes = tokio::fs::read(source)
        .await
        .map_err(|source_error| StorageError::Io {
            path: source.to_owned(),
            source: source_error,
        })?;
    write_atomic(destination, &bytes).await
}

pub async fn load_preview(path: &Path) -> Result<LoadedImagePreview, StorageError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&path).map_err(|source| StorageError::Io {
            path: path.clone(),
            source,
        })?;
        let source_format = image::guess_format(&bytes)?;
        let image = image::load_from_memory_with_format(&bytes, source_format)?;
        let (width, height) = image.dimensions();
        let preview = prepare_preview(&image, width, height);
        Ok(LoadedImagePreview {
            width: preview.width,
            height: preview.height,
            rgba: preview.rgba,
        })
    })
    .await?
}

async fn write_output(
    encoded: &[u8],
    filename_prefix: &str,
    output_format: OutputFormat,
) -> Result<PathBuf, StorageError> {
    let output_dir = output_directory()?;
    write_output_in(&output_dir, encoded, filename_prefix, output_format).await
}

async fn write_output_in(
    output_dir: &Path,
    encoded: &[u8],
    filename_prefix: &str,
    output_format: OutputFormat,
) -> Result<PathBuf, StorageError> {
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(|source| StorageError::Io {
            path: output_dir.to_owned(),
            source,
        })?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageError::InvalidSystemTime)?
        .as_millis();
    let sequence = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let process = std::process::id();
    let extension = output_format.extension();
    let filename = format!("{filename_prefix}-{timestamp}-{process}-{sequence}.{extension}");
    let path = output_dir.join(&filename);
    write_atomic(&path, encoded).await?;
    Ok(path)
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let Some(output_dir) = path.parent() else {
        return Err(StorageError::Io {
            path: path.to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "destination has no parent directory",
            ),
        });
    };
    tokio::fs::create_dir_all(output_dir)
        .await
        .map_err(|source| StorageError::Io {
            path: output_dir.to_owned(),
            source,
        })?;

    let sequence = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "output".into());
    let temporary_path =
        output_dir.join(format!(".{filename}.{}-{sequence}.tmp", std::process::id()));
    let mut temporary_output = TemporaryOutput::new(temporary_path.clone());

    let write_result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
            .await?;
        file.write_all(bytes).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary_path, path).await?;
        temporary_output.mark_committed();
        Ok::<(), std::io::Error>(())
    }
    .await;

    if let Err(source) = write_result {
        return Err(StorageError::Io {
            path: path.to_owned(),
            source,
        });
    }

    Ok(())
}

fn output_directory() -> Result<PathBuf, StorageError> {
    Ok(AppPaths::discover()?.data_dir().join("outputs"))
}
#[derive(Debug)]
struct PreparedImage {
    encoded: Vec<u8>,
    width: u32,
    height: u32,
    preview: Option<PreparedPreview>,
}

#[derive(Debug)]
struct PreparedPreview {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn prepare_image(
    bytes: &[u8],
    output_format: OutputFormat,
    include_preview: bool,
) -> Result<PreparedImage, image::ImageError> {
    let source_format = image::guess_format(bytes)?;
    let image = image::load_from_memory_with_format(bytes, source_format)?;
    let desired_format = image_format(output_format);
    let image = if desired_format == ImageFormat::Jpeg && source_format != ImageFormat::Jpeg {
        flatten_transparency_onto_white(image)
    } else {
        image
    };
    let (width, height) = image.dimensions();
    let preview = include_preview.then(|| prepare_preview(&image, width, height));
    let encoded = if source_format == desired_format {
        bytes.to_vec()
    } else {
        let mut encoded = Cursor::new(Vec::new());
        image.write_to(&mut encoded, desired_format)?;
        encoded.into_inner()
    };
    Ok(PreparedImage {
        encoded,
        width,
        height,
        preview,
    })
}

fn prepare_preview(image: &DynamicImage, width: u32, height: u32) -> PreparedPreview {
    let preview = if width > PREVIEW_MAX_EDGE || height > PREVIEW_MAX_EDGE {
        image.thumbnail(PREVIEW_MAX_EDGE, PREVIEW_MAX_EDGE)
    } else {
        image.clone()
    };
    let (width, height) = preview.dimensions();
    PreparedPreview {
        width,
        height,
        rgba: preview.to_rgba8().into_raw(),
    }
}

fn flatten_transparency_onto_white(image: DynamicImage) -> DynamicImage {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let rgb = RgbImage::from_fn(width, height, |x, y| {
        let pixel = rgba.get_pixel(x, y);
        let alpha = u16::from(pixel[3]);
        let blend =
            |channel: u8| ((u16::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
        Rgb([blend(pixel[0]), blend(pixel[1]), blend(pixel[2])])
    });
    DynamicImage::ImageRgb8(rgb)
}

fn image_format(output_format: OutputFormat) -> ImageFormat {
    match output_format {
        OutputFormat::Png => ImageFormat::Png,
        OutputFormat::WebP => ImageFormat::WebP,
        OutputFormat::Jpeg => ImageFormat::Jpeg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_preserves_png_dimensions() {
        let image = image::DynamicImage::new_rgba8(2, 3);
        let mut encoded = Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, ImageFormat::Png)
            .expect("test PNG should encode");

        let prepared = prepare_image(&encoded.into_inner(), OutputFormat::Png, false)
            .expect("test PNG should validate");

        assert_eq!((prepared.width, prepared.height), (2, 3));
        assert_eq!(
            image::guess_format(&prepared.encoded).expect("format should be known"),
            ImageFormat::Png
        );
        assert!(prepared.preview.is_none());
    }

    #[test]
    fn prepares_rgba_pixels_for_desktop_preview() {
        let image = image::DynamicImage::new_rgba8(2, 3);
        let mut encoded = Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, ImageFormat::Png)
            .expect("test PNG should encode");

        let prepared = prepare_image(&encoded.into_inner(), OutputFormat::Png, true)
            .expect("test PNG should validate");

        let preview = prepared.preview.expect("preview should be prepared");
        assert_eq!((preview.width, preview.height), (2, 3));
        assert_eq!(preview.rgba.len(), 2 * 3 * 4);
    }

    #[test]
    fn bounds_large_desktop_previews_without_changing_saved_dimensions() {
        let image = image::DynamicImage::new_rgba8(3_840, 2_160);
        let mut encoded = Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, ImageFormat::Png)
            .expect("test PNG should encode");

        let prepared = prepare_image(&encoded.into_inner(), OutputFormat::Png, true)
            .expect("large PNG should validate");
        let preview = prepared.preview.expect("preview should be prepared");

        assert_eq!((prepared.width, prepared.height), (3_840, 2_160));
        assert_eq!((preview.width, preview.height), (1_600, 900));
        assert_eq!(
            preview.rgba.len(),
            preview.width as usize * preview.height as usize * 4
        );
    }

    #[test]
    fn rejects_non_image_data() {
        let error = prepare_image(b"not an image", OutputFormat::Png, false)
            .expect_err("invalid data should fail");

        assert!(matches!(error, image::ImageError::Unsupported(_)));
    }

    #[tokio::test]
    async fn atomically_commits_complete_output_without_leaving_a_temporary_file() {
        let test_id = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "a6-image-studio-storage-test-{}-{test_id}",
            std::process::id()
        ));
        let png = b"complete PNG payload";

        let path = write_output_in(&directory, png, "generated", OutputFormat::Png)
            .await
            .expect("atomic write should succeed");
        let saved = tokio::fs::read(&path)
            .await
            .expect("committed output should be readable");
        let mut entries = tokio::fs::read_dir(&directory)
            .await
            .expect("test output directory should be readable");
        let mut filenames = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .expect("directory entries should be readable")
        {
            filenames.push(entry.file_name());
        }

        assert_eq!(saved, png);
        assert_eq!(filenames, vec![path.file_name().expect("path has a name")]);
        assert!(
            filenames
                .iter()
                .all(|name| !name.to_string_lossy().ends_with(".tmp"))
        );

        tokio::fs::remove_dir_all(&directory)
            .await
            .expect("test output should be removable");
    }

    #[tokio::test]
    async fn save_as_copy_atomically_replaces_the_selected_file() {
        let test_id = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "a6-image-studio-copy-test-{}-{test_id}",
            std::process::id()
        ));
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("test directory should be created");
        let source = directory.join("source.webp");
        let destination = directory.join("copy.webp");
        tokio::fs::write(&source, b"new complete image")
            .await
            .expect("source should be written");
        tokio::fs::write(&destination, b"old image")
            .await
            .expect("existing destination should be written");

        copy_atomic(&source, &destination)
            .await
            .expect("atomic copy should succeed");

        assert_eq!(
            tokio::fs::read(&destination)
                .await
                .expect("destination should be readable"),
            b"new complete image"
        );
        let mut entries = tokio::fs::read_dir(&directory)
            .await
            .expect("directory should be readable");
        while let Some(entry) = entries
            .next_entry()
            .await
            .expect("directory entry should be readable")
        {
            assert!(!entry.file_name().to_string_lossy().ends_with(".tmp"));
        }

        tokio::fs::remove_dir_all(&directory)
            .await
            .expect("test output should be removable");
    }

    #[test]
    fn converts_png_to_each_supported_output_format() {
        let image = image::DynamicImage::new_rgb8(2, 3);
        let mut png = Cursor::new(Vec::new());
        image
            .write_to(&mut png, ImageFormat::Png)
            .expect("test PNG should encode");
        let png = png.into_inner();

        for (output_format, expected) in [
            (OutputFormat::Png, ImageFormat::Png),
            (OutputFormat::WebP, ImageFormat::WebP),
            (OutputFormat::Jpeg, ImageFormat::Jpeg),
        ] {
            let prepared =
                prepare_image(&png, output_format, true).expect("requested output should encode");
            assert_eq!(
                image::guess_format(&prepared.encoded).expect("format should be known"),
                expected
            );
            assert_eq!((prepared.width, prepared.height), (2, 3));
            assert_eq!(
                prepared.preview.as_ref().map(|preview| preview.rgba.len()),
                Some(24)
            );
        }
    }

    #[test]
    fn jpeg_conversion_flattens_transparency_onto_white() {
        let transparent = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([1, 2, 3, 0]),
        ));
        let flattened = flatten_transparency_onto_white(transparent).to_rgb8();

        assert_eq!(flattened.get_pixel(0, 0), &Rgb([255, 255, 255]));
    }

    #[test]
    fn dropped_temporary_output_removes_an_incomplete_file() {
        let test_id = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "a6-image-studio-guard-test-{}-{test_id}",
            std::process::id()
        ));
        let temporary_path = directory.join(".generated.png.tmp");
        std::fs::create_dir_all(&directory).expect("test directory should be created");
        std::fs::write(&temporary_path, b"incomplete").expect("test file should be written");

        {
            let _temporary_output = TemporaryOutput::new(temporary_path.clone());
        }

        assert!(!temporary_path.exists());
        std::fs::remove_dir_all(&directory).expect("test directory should be removable");
    }
}
