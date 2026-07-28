use std::ffi::OsString;
use std::io::Cursor;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use image::{GenericImageView, ImageFormat};
use thiserror::Error;

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
    #[error("failed to create or write output file {path}: {source}")]
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
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(|source| StorageError::Io {
            path: output_dir.clone(),
            source,
        })?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageError::InvalidSystemTime)?
        .as_millis();
    let path = output_dir.join(format!("{filename_prefix}-{timestamp}.png"));
    tokio::fs::write(&path, png)
        .await
        .map_err(|source| StorageError::Io {
            path: path.clone(),
            source,
        })?;

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
}
