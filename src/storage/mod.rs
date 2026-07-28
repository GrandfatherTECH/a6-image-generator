use std::ffi::OsString;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use image::{GenericImageView, ImageFormat};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

static OUTPUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    pub rgba: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("an XDG data directory could not be determined")]
    OutputDirectoryUnavailable,
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
    let prepared = tokio::task::spawn_blocking(move || prepare_png(&bytes, false)).await??;
    let path = write_png(&prepared.png, "smoke-test").await?;

    Ok(SavedImage {
        path,
        width: prepared.width,
        height: prepared.height,
        file_size: prepared.png.len() as u64,
    })
}

pub async fn save_png_with_preview(bytes: Vec<u8>) -> Result<SavedImagePreview, StorageError> {
    let prepared = tokio::task::spawn_blocking(move || prepare_png(&bytes, true)).await??;
    let path = write_png(&prepared.png, "generated").await?;

    Ok(SavedImagePreview {
        path,
        width: prepared.width,
        height: prepared.height,
        file_size: prepared.png.len() as u64,
        rgba: prepared.rgba.unwrap_or_default(),
    })
}

async fn write_png(png: &[u8], filename_prefix: &str) -> Result<PathBuf, StorageError> {
    let output_dir = output_directory()?;
    write_png_in(&output_dir, png, filename_prefix).await
}

async fn write_png_in(
    output_dir: &Path,
    png: &[u8],
    filename_prefix: &str,
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
    let filename = format!("{filename_prefix}-{timestamp}-{process}-{sequence}.png");
    let path = output_dir.join(&filename);
    let temporary_path = output_dir.join(format!(".{filename}.tmp"));
    let mut temporary_output = TemporaryOutput::new(temporary_path.clone());

    let write_result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
            .await?;
        file.write_all(png).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary_path, &path).await?;
        temporary_output.mark_committed();
        Ok::<(), std::io::Error>(())
    }
    .await;

    if let Err(source) = write_result {
        return Err(StorageError::Io {
            path: path.clone(),
            source,
        });
    }

    Ok(path)
}

fn output_directory() -> Result<PathBuf, StorageError> {
    output_directory_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

fn output_directory_from(
    xdg_data_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, StorageError> {
    let data_home = xdg_data_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(".local/share"))
        })
        .ok_or(StorageError::OutputDirectoryUnavailable)?;
    Ok(data_home.join("a6-image-studio/outputs"))
}
#[derive(Debug)]
struct PreparedPng {
    png: Vec<u8>,
    width: u32,
    height: u32,
    rgba: Option<Vec<u8>>,
}

fn prepare_png(bytes: &[u8], include_preview: bool) -> Result<PreparedPng, image::ImageError> {
    let format = image::guess_format(bytes)?;
    let image = image::load_from_memory_with_format(bytes, format)?;
    let (width, height) = image.dimensions();
    let rgba = include_preview.then(|| image.to_rgba8().into_raw());
    let png = if format == ImageFormat::Png {
        bytes.to_vec()
    } else {
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, ImageFormat::Png)?;
        png.into_inner()
    };
    Ok(PreparedPng {
        png,
        width,
        height,
        rgba,
    })
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

        let prepared = prepare_png(&encoded.into_inner(), false).expect("test PNG should validate");

        assert_eq!((prepared.width, prepared.height), (2, 3));
        assert_eq!(
            image::guess_format(&prepared.png).expect("format should be known"),
            ImageFormat::Png
        );
        assert!(prepared.rgba.is_none());
    }

    #[test]
    fn prepares_rgba_pixels_for_desktop_preview() {
        let image = image::DynamicImage::new_rgba8(2, 3);
        let mut encoded = Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, ImageFormat::Png)
            .expect("test PNG should encode");

        let prepared = prepare_png(&encoded.into_inner(), true).expect("test PNG should validate");

        assert_eq!(prepared.rgba.as_deref().map(<[u8]>::len), Some(2 * 3 * 4));
    }

    #[test]
    fn rejects_non_image_data() {
        let error = prepare_png(b"not an image", false).expect_err("invalid data should fail");

        assert!(matches!(error, image::ImageError::Unsupported(_)));
    }

    #[test]
    fn uses_absolute_xdg_data_home() {
        let path = output_directory_from(
            Some(OsString::from("/tmp/custom-data")),
            Some(OsString::from("/home/tester")),
        )
        .expect("absolute XDG path should be accepted");

        assert_eq!(
            path,
            PathBuf::from("/tmp/custom-data/a6-image-studio/outputs")
        );
    }

    #[test]
    fn ignores_relative_xdg_data_home() {
        let path = output_directory_from(
            Some(OsString::from("relative-data")),
            Some(OsString::from("/home/tester")),
        )
        .expect("home fallback should be used");

        assert_eq!(
            path,
            PathBuf::from("/home/tester/.local/share/a6-image-studio/outputs")
        );
    }

    #[tokio::test]
    async fn atomically_commits_complete_output_without_leaving_a_temporary_file() {
        let test_id = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "a6-image-studio-storage-test-{}-{test_id}",
            std::process::id()
        ));
        let png = b"complete PNG payload";

        let path = write_png_in(&directory, png, "generated")
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
