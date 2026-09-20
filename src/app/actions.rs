//! Desktop file-dialog, clipboard, and containing-folder actions.
//!
//! External clipboard and folder commands are invoked directly without a
//! shell, and file copies use the storage module's atomic write path.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use thiserror::Error;

use crate::domain::GeneratedImage;
use crate::generation::OutputFormat;
use crate::storage;
use crate::xdg::AppPaths;

#[derive(Debug, Error)]
pub(crate) enum ResultActionError {
    #[error("the generated image path has no containing folder")]
    MissingContainingFolder,
    #[error("the selected filename must use .{expected} for {format} output")]
    WrongExtension {
        expected: &'static str,
        format: &'static str,
    },
    #[error("clipboard support is unavailable; install wl-clipboard on Wayland or xclip on X11")]
    ClipboardUnavailable,
    #[error("{action} failed: {source}")]
    Io {
        action: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} exited unsuccessfully while {action}")]
    CommandFailed {
        program: &'static str,
        action: &'static str,
    },
    #[error(transparent)]
    Storage(#[from] storage::StorageError),
}

pub(crate) async fn save_as(image: &GeneratedImage) -> Result<Option<PathBuf>, ResultActionError> {
    let output_format = image.output_format();
    let mut dialog = rfd::AsyncFileDialog::new()
        .set_title("Save generated image as")
        .set_file_name(
            image
                .path()
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("generated.{}", output_format.extension())),
        )
        .add_filter(
            output_format.display_name(),
            format_extensions(output_format),
        );
    if let Ok(paths) = AppPaths::discover() {
        dialog = dialog.set_directory(paths.pictures_dir());
    } else if let Some(parent) = image.path().parent() {
        dialog = dialog.set_directory(parent);
    }

    let Some(handle) = dialog.save_file().await else {
        return Ok(None);
    };
    let destination = normalized_destination(handle.path(), output_format)?;
    storage::copy_atomic(image.path(), &destination).await?;
    Ok(Some(destination))
}

pub(crate) async fn copy_image(image: &GeneratedImage) -> Result<(), ResultActionError> {
    let bytes = tokio::fs::read(image.path())
        .await
        .map_err(|source| ResultActionError::Io {
            action: "reading the generated image",
            source,
        })?;
    let mime_type = image.output_format().mime_type();
    tokio::task::spawn_blocking(move || copy_bytes(bytes, mime_type))
        .await
        .map_err(|source| ResultActionError::Io {
            action: "waiting for the clipboard worker",
            source: std::io::Error::other(source),
        })?
}

pub(crate) async fn copy_prompt(prompt: String) -> Result<(), ResultActionError> {
    tokio::task::spawn_blocking(move || copy_bytes(prompt.into_bytes(), "text/plain;charset=utf-8"))
        .await
        .map_err(|source| ResultActionError::Io {
            action: "waiting for the clipboard worker",
            source: std::io::Error::other(source),
        })?
}

pub(crate) async fn open_containing_folder(
    image: &GeneratedImage,
) -> Result<(), ResultActionError> {
    open_containing_path(image.path()).await
}

pub(crate) async fn open_containing_path(path: &Path) -> Result<(), ResultActionError> {
    let directory = path
        .parent()
        .ok_or(ResultActionError::MissingContainingFolder)?
        .to_owned();
    tokio::task::spawn_blocking(move || open_directory(&directory))
        .await
        .map_err(|source| ResultActionError::Io {
            action: "waiting for the folder opener",
            source: std::io::Error::other(source),
        })?
}

pub(crate) async fn choose_output_directory() -> Option<PathBuf> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Choose default output directory");
    if let Ok(paths) = AppPaths::discover() {
        dialog = dialog.set_directory(paths.pictures_dir());
    }
    dialog
        .pick_folder()
        .await
        .map(|handle| handle.path().to_owned())
}

pub(crate) async fn export_diagnostics(
    report: Vec<u8>,
) -> Result<Option<PathBuf>, ResultActionError> {
    let mut dialog = rfd::AsyncFileDialog::new()
        .set_title("Export sanitized diagnostics")
        .set_file_name("a6-image-studio-diagnostics.json")
        .add_filter("JSON diagnostic report", &["json"]);
    if let Ok(paths) = AppPaths::discover() {
        dialog = dialog.set_directory(paths.pictures_dir());
    }

    let Some(handle) = dialog.save_file().await else {
        return Ok(None);
    };
    let destination = handle.path().to_owned();
    storage::write_bytes_atomic(&destination, &report).await?;
    Ok(Some(destination))
}

fn normalized_destination(
    selected: &Path,
    output_format: OutputFormat,
) -> Result<PathBuf, ResultActionError> {
    let mut destination = selected.to_owned();
    match destination
        .extension()
        .and_then(|extension| extension.to_str())
    {
        None => {
            destination.set_extension(output_format.extension());
            Ok(destination)
        }
        Some(extension) if extension_matches(extension, output_format) => Ok(destination),
        Some(_) => Err(ResultActionError::WrongExtension {
            expected: output_format.extension(),
            format: output_format.display_name(),
        }),
    }
}

fn extension_matches(extension: &str, output_format: OutputFormat) -> bool {
    if output_format == OutputFormat::Jpeg {
        extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg")
    } else {
        extension.eq_ignore_ascii_case(output_format.extension())
    }
}

fn format_extensions(output_format: OutputFormat) -> &'static [&'static str] {
    match output_format {
        OutputFormat::Png => &["png"],
        OutputFormat::WebP => &["webp"],
        OutputFormat::Jpeg => &["jpg", "jpeg"],
    }
}

fn copy_bytes(bytes: Vec<u8>, mime_type: &'static str) -> Result<(), ResultActionError> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut attempted = false;

    if wayland {
        match write_command(
            "wl-copy",
            &["--type", mime_type],
            &bytes,
            "copying data to the Wayland clipboard",
        ) {
            Ok(()) => return Ok(()),
            Err(CommandAttempt::Missing) => {}
            Err(CommandAttempt::Failed(error)) => return Err(error),
        }
        attempted = true;
    }

    match write_command(
        "xclip",
        &["-selection", "clipboard", "-target", mime_type, "-in"],
        &bytes,
        "copying data to the X11 clipboard",
    ) {
        Ok(()) => return Ok(()),
        Err(CommandAttempt::Missing) => {}
        Err(CommandAttempt::Failed(error)) => return Err(error),
    }

    if !wayland && !attempted {
        match write_command(
            "wl-copy",
            &["--type", mime_type],
            &bytes,
            "copying data to the Wayland clipboard",
        ) {
            Ok(()) => return Ok(()),
            Err(CommandAttempt::Missing) => {}
            Err(CommandAttempt::Failed(error)) => return Err(error),
        }
    }

    Err(ResultActionError::ClipboardUnavailable)
}

enum CommandAttempt {
    Missing,
    Failed(ResultActionError),
}

fn write_command(
    program: &'static str,
    arguments: &[&str],
    bytes: &[u8],
    action: &'static str,
) -> Result<(), CommandAttempt> {
    let mut child = match Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(CommandAttempt::Missing);
        }
        Err(source) => {
            return Err(CommandAttempt::Failed(ResultActionError::Io {
                action,
                source,
            }));
        }
    };

    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| {
            CommandAttempt::Failed(ResultActionError::Io {
                action,
                source: std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "clipboard command did not provide standard input",
                ),
            })
        })
        .and_then(|mut stdin| {
            stdin
                .write_all(bytes)
                .map_err(|source| CommandAttempt::Failed(ResultActionError::Io { action, source }))
        });
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    let status = child
        .wait()
        .map_err(|source| CommandAttempt::Failed(ResultActionError::Io { action, source }))?;
    if status.success() {
        Ok(())
    } else {
        Err(CommandAttempt::Failed(ResultActionError::CommandFailed {
            program,
            action,
        }))
    }
}

fn open_directory(directory: &Path) -> Result<(), ResultActionError> {
    let status = Command::new("xdg-open")
        .arg(directory)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| ResultActionError::Io {
            action: "opening the containing folder",
            source,
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(ResultActionError::CommandFailed {
            program: "xdg-open",
            action: "opening the containing folder",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_as_adds_a_missing_extension() {
        assert_eq!(
            normalized_destination(Path::new("/tmp/generated"), OutputFormat::Png)
                .expect("missing extension should be supplied"),
            PathBuf::from("/tmp/generated.png")
        );
    }

    #[test]
    fn save_as_rejects_a_misleading_extension() {
        assert!(matches!(
            normalized_destination(Path::new("/tmp/generated.jpg"), OutputFormat::Png),
            Err(ResultActionError::WrongExtension { .. })
        ));
    }

    #[test]
    fn save_as_accepts_both_jpeg_extensions_case_insensitively() {
        assert!(
            normalized_destination(Path::new("/tmp/generated.jpg"), OutputFormat::Jpeg).is_ok()
        );
        assert!(
            normalized_destination(Path::new("/tmp/generated.JPEG"), OutputFormat::Jpeg).is_ok()
        );
    }
}
