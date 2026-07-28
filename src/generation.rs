//! Validated image-generation options shared by the GUI and transport layer.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

macro_rules! string_enum {
    ($type:ty { $($variant:ident => $value:literal),+ $(,)? }) => {
        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(match self {
                    $(Self::$variant => $value),+
                })
            }
        }

        impl FromStr for $type {
            type Err = GenerationValidationError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($value => Ok(Self::$variant),)+
                    _ => Err(GenerationValidationError::UnsupportedValue {
                        field: stringify!($type),
                        value: value.to_owned(),
                    }),
                }
            }
        }
    };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageSize {
    Auto,
    #[default]
    Square,
    Portrait,
    Landscape,
}

impl ImageSize {
    pub const fn api_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Square => "1024x1024",
            Self::Portrait => "1024x1536",
            Self::Landscape => "1536x1024",
        }
    }
}

string_enum!(ImageSize {
    Auto => "auto",
    Square => "1024x1024",
    Portrait => "1024x1536",
    Landscape => "1536x1024",
});

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageQuality {
    Auto,
    #[default]
    Low,
    Medium,
    High,
}

impl ImageQuality {
    pub const fn api_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

string_enum!(ImageQuality {
    Auto => "auto",
    Low => "low",
    Medium => "medium",
    High => "high",
});

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageBackground {
    #[default]
    Auto,
    Opaque,
    Transparent,
}

impl ImageBackground {
    pub const fn api_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Opaque => "opaque",
            Self::Transparent => "transparent",
        }
    }
}

string_enum!(ImageBackground {
    Auto => "auto",
    Opaque => "opaque",
    Transparent => "transparent",
});

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Png,
    WebP,
    Jpeg,
}

impl OutputFormat {
    pub const fn api_value(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::WebP => "webp",
            Self::Jpeg => "jpeg",
        }
    }

    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::WebP => "webp",
            Self::Jpeg => "jpg",
        }
    }

    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::WebP => "image/webp",
            Self::Jpeg => "image/jpeg",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::WebP => "WebP",
            Self::Jpeg => "JPEG",
        }
    }
}

string_enum!(OutputFormat {
    Png => "PNG",
    WebP => "WebP",
    Jpeg => "JPEG",
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompatibilitySettings {
    pub send_size: bool,
    pub send_quality: bool,
    pub send_background: bool,
    pub send_output_format: bool,
}

impl Default for CompatibilitySettings {
    fn default() -> Self {
        Self {
            send_size: true,
            send_quality: true,
            send_background: true,
            send_output_format: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenerationOptions {
    pub size: ImageSize,
    pub quality: ImageQuality,
    pub background: ImageBackground,
    pub output_format: OutputFormat,
    pub compatibility: CompatibilitySettings,
}

impl GenerationOptions {
    pub fn validate(&self) -> Result<(), GenerationValidationError> {
        if self.compatibility.send_background
            && self.background == ImageBackground::Transparent
            && self.output_format == OutputFormat::Jpeg
        {
            return Err(GenerationValidationError::TransparentJpeg);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationInput {
    prompt: String,
    options: GenerationOptions,
}

impl GenerationInput {
    pub fn new(
        prompt: impl Into<String>,
        options: GenerationOptions,
    ) -> Result<Self, GenerationValidationError> {
        let prompt = prompt.into();
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(GenerationValidationError::EmptyPrompt);
        }
        options.validate()?;
        Ok(Self {
            prompt: prompt.to_owned(),
            options,
        })
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn options(&self) -> &GenerationOptions {
        &self.options
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum GenerationValidationError {
    #[error("enter a prompt before generating an image")]
    EmptyPrompt,
    #[error("transparent background cannot be combined with JPEG output; choose PNG or WebP")]
    TransparentJpeg,
    #[error("unsupported {field} value: {value}")]
    UnsupportedValue { field: &'static str, value: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_phase_three_control_value() {
        assert_eq!("auto".parse(), Ok(ImageSize::Auto));
        assert_eq!("1024x1024".parse(), Ok(ImageSize::Square));
        assert_eq!("1024x1536".parse(), Ok(ImageSize::Portrait));
        assert_eq!("1536x1024".parse(), Ok(ImageSize::Landscape));
        assert_eq!("high".parse(), Ok(ImageQuality::High));
        assert_eq!("transparent".parse(), Ok(ImageBackground::Transparent));
        assert_eq!("WebP".parse(), Ok(OutputFormat::WebP));
        assert_eq!("JPEG".parse(), Ok(OutputFormat::Jpeg));
    }

    #[test]
    fn trims_prompt_and_rejects_empty_input() {
        let input = GenerationInput::new("  useful prompt  ", GenerationOptions::default())
            .expect("trimmed prompt should be valid");
        assert_eq!(input.prompt(), "useful prompt");

        assert_eq!(
            GenerationInput::new(" \n ", GenerationOptions::default()),
            Err(GenerationValidationError::EmptyPrompt)
        );
    }

    #[test]
    fn rejects_transparent_jpeg_but_accepts_other_transparent_formats() {
        let mut options = GenerationOptions {
            background: ImageBackground::Transparent,
            output_format: OutputFormat::Jpeg,
            ..GenerationOptions::default()
        };
        assert_eq!(
            options.validate(),
            Err(GenerationValidationError::TransparentJpeg)
        );

        options.output_format = OutputFormat::WebP;
        assert_eq!(options.validate(), Ok(()));
    }

    #[test]
    fn disabled_background_parameter_does_not_trigger_transparency_conflict() {
        let options = GenerationOptions {
            background: ImageBackground::Transparent,
            output_format: OutputFormat::Jpeg,
            compatibility: CompatibilitySettings {
                send_background: false,
                ..CompatibilitySettings::default()
            },
            ..GenerationOptions::default()
        };

        assert_eq!(options.validate(), Ok(()));
    }
}
