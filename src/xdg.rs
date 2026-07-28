//! Linux desktop identity and XDG directory discovery.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use thiserror::Error;

pub const APP_ID: &str = "io.github.grandfathertech.a6-image-studio";
pub const APP_DIRECTORY: &str = "a6-image-studio";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    pictures_dir: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self, XdgPathError> {
        let home = std::env::var_os("HOME");
        let config_home = std::env::var_os("XDG_CONFIG_HOME");
        let data_home = std::env::var_os("XDG_DATA_HOME");
        let cache_home = std::env::var_os("XDG_CACHE_HOME");
        let pictures = std::env::var_os("XDG_PICTURES_DIR");
        Self::from_environment(home, config_home, data_home, cache_home, pictures)
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn pictures_dir(&self) -> &Path {
        &self.pictures_dir
    }

    fn from_environment(
        home: Option<OsString>,
        config_home: Option<OsString>,
        data_home: Option<OsString>,
        cache_home: Option<OsString>,
        pictures: Option<OsString>,
    ) -> Result<Self, XdgPathError> {
        let home = absolute_path(home).ok_or(XdgPathError::HomeUnavailable)?;
        let config_home = absolute_path(config_home).unwrap_or_else(|| home.join(".config"));
        let data_home = absolute_path(data_home).unwrap_or_else(|| home.join(".local/share"));
        let cache_home = absolute_path(cache_home).unwrap_or_else(|| home.join(".cache"));
        let pictures_dir = absolute_path(pictures)
            .or_else(|| read_pictures_directory(&config_home, &home))
            .unwrap_or_else(|| home.join("Pictures"));

        Ok(Self {
            config_dir: config_home.join(APP_DIRECTORY),
            data_dir: data_home.join(APP_DIRECTORY),
            cache_dir: cache_home.join(APP_DIRECTORY),
            pictures_dir,
        })
    }
}

#[derive(Debug, Error)]
pub enum XdgPathError {
    #[error("an absolute HOME or XDG directory could not be determined")]
    HomeUnavailable,
}

fn absolute_path(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

fn read_pictures_directory(config_home: &Path, home: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(config_home.join("user-dirs.dirs")).ok()?;
    parse_pictures_directory(&contents, home)
}

fn parse_pictures_directory(contents: &str, home: &Path) -> Option<PathBuf> {
    let value = contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("XDG_PICTURES_DIR=").map(str::trim))?;
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);

    if let Some(suffix) = value
        .strip_prefix("$HOME")
        .or_else(|| value.strip_prefix("${HOME}"))
    {
        return Some(home.join(suffix.trim_start_matches('/')));
    }

    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_all_standard_application_directories() {
        let paths = AppPaths::from_environment(
            Some("/home/tester".into()),
            Some("/var/test-config".into()),
            Some("/var/test-data".into()),
            Some("/var/test-cache".into()),
            Some("/srv/pictures".into()),
        )
        .expect("absolute directories should be accepted");

        assert_eq!(
            paths.config_dir(),
            Path::new("/var/test-config/a6-image-studio")
        );
        assert_eq!(
            paths.data_dir(),
            Path::new("/var/test-data/a6-image-studio")
        );
        assert_eq!(
            paths.cache_dir(),
            Path::new("/var/test-cache/a6-image-studio")
        );
        assert_eq!(paths.pictures_dir(), Path::new("/srv/pictures"));
    }

    #[test]
    fn relative_xdg_values_fall_back_to_home() {
        let paths = AppPaths::from_environment(
            Some("/home/tester".into()),
            Some("relative-config".into()),
            Some("relative-data".into()),
            Some("relative-cache".into()),
            Some("relative-pictures".into()),
        )
        .expect("home fallbacks should be available");

        assert_eq!(
            paths.config_dir(),
            Path::new("/home/tester/.config/a6-image-studio")
        );
        assert_eq!(
            paths.data_dir(),
            Path::new("/home/tester/.local/share/a6-image-studio")
        );
        assert_eq!(
            paths.cache_dir(),
            Path::new("/home/tester/.cache/a6-image-studio")
        );
        assert_eq!(paths.pictures_dir(), Path::new("/home/tester/Pictures"));
    }

    #[test]
    fn parses_freedesktop_user_pictures_directory() {
        let contents = r#"
            XDG_DESKTOP_DIR="$HOME/Desktop"
            XDG_PICTURES_DIR="${HOME}/Media/Pictures"
        "#;

        assert_eq!(
            parse_pictures_directory(contents, Path::new("/home/tester")),
            Some(PathBuf::from("/home/tester/Media/Pictures"))
        );
    }
}
