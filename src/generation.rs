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

/// Required alignment for each explicit image edge.
pub const IMAGE_DIMENSION_STEP: u32 = 16;
/// Largest accepted width or height in pixels.
pub const IMAGE_MAX_EDGE: u32 = 3_840;
/// Smallest accepted total pixel count for an explicit image size.
pub const IMAGE_MIN_PIXELS: u64 = 655_360;
/// Largest accepted total pixel count for an explicit image size.
pub const IMAGE_MAX_PIXELS: u64 = 8_294_400;
/// Pixel count above which the UI labels a request as experimental.
pub const IMAGE_EXPERIMENTAL_PIXELS: u64 = 2_560 * 1_440;

/// Validated explicit image dimensions accepted by the configured model.
///
/// Construction enforces edge limits, 16-pixel alignment, a maximum 3:1 aspect
/// ratio, and total-pixel bounds before any request reaches the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageDimensions {
    width: u32,
    height: u32,
}

impl ImageDimensions {
    pub fn new(width: u32, height: u32) -> Result<Self, GenerationValidationError> {
        if width == 0 || height == 0 || width > IMAGE_MAX_EDGE || height > IMAGE_MAX_EDGE {
            return Err(GenerationValidationError::ImageEdge {
                width,
                height,
                maximum: IMAGE_MAX_EDGE,
            });
        }
        if !width.is_multiple_of(IMAGE_DIMENSION_STEP)
            || !height.is_multiple_of(IMAGE_DIMENSION_STEP)
        {
            return Err(GenerationValidationError::ImageAlignment {
                width,
                height,
                step: IMAGE_DIMENSION_STEP,
            });
        }

        let long_edge = width.max(height);
        let short_edge = width.min(height);
        if long_edge > short_edge * 3 {
            return Err(GenerationValidationError::ImageAspectRatio { width, height });
        }

        let pixels = u64::from(width) * u64::from(height);
        if !(IMAGE_MIN_PIXELS..=IMAGE_MAX_PIXELS).contains(&pixels) {
            return Err(GenerationValidationError::ImagePixelCount {
                width,
                height,
                pixels,
                minimum: IMAGE_MIN_PIXELS,
                maximum: IMAGE_MAX_PIXELS,
            });
        }

        Ok(Self { width, height })
    }

    pub const fn width(self) -> u32 {
        self.width
    }

    pub const fn height(self) -> u32 {
        self.height
    }

    pub const fn pixel_count(self) -> u64 {
        self.width as u64 * self.height as u64
    }

    pub const fn is_experimental(self) -> bool {
        self.pixel_count() > IMAGE_EXPERIMENTAL_PIXELS
    }
}

impl fmt::Display for ImageDimensions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}x{}", self.width, self.height)
    }
}

/// Either provider-selected dimensions or validated explicit dimensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageSize {
    Auto,
    Dimensions(ImageDimensions),
}

impl Default for ImageSize {
    fn default() -> Self {
        Self::Dimensions(ImageDimensions {
            width: 1_024,
            height: 1_024,
        })
    }
}

impl ImageSize {
    pub fn dimensions(width: u32, height: u32) -> Result<Self, GenerationValidationError> {
        ImageDimensions::new(width, height).map(Self::Dimensions)
    }

    pub fn api_value(self) -> String {
        self.to_string()
    }

    pub const fn explicit_dimensions(self) -> Option<ImageDimensions> {
        match self {
            Self::Auto => None,
            Self::Dimensions(dimensions) => Some(dimensions),
        }
    }
}

impl fmt::Display for ImageSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => formatter.write_str("auto"),
            Self::Dimensions(dimensions) => dimensions.fmt(formatter),
        }
    }
}

impl FromStr for ImageSize {
    type Err = GenerationValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "auto" {
            return Ok(Self::Auto);
        }
        let (width, height) = value
            .split_once('x')
            .ok_or_else(|| GenerationValidationError::InvalidImageSize(value.to_owned()))?;
        let width = width
            .parse::<u32>()
            .map_err(|_| GenerationValidationError::InvalidImageSize(value.to_owned()))?;
        let height = height
            .parse::<u32>()
            .map_err(|_| GenerationValidationError::InvalidImageSize(value.to_owned()))?;
        Self::dimensions(width, height)
    }
}

/// Provider quality requested for a generated image.
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

/// Background treatment requested from the gateway.
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

/// Durable image format and its corresponding API representation.
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

/// Controls which optional fields are present in the gateway request.
///
/// A disabled field is omitted entirely rather than serialized as `null`.
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

/// Validated generation controls shared by the desktop and transport layers.
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

/// A trimmed, non-empty prompt paired with validated generation options.
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

/// Local validation failures that prevent a generation request.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum GenerationValidationError {
    #[error("enter a prompt before generating an image")]
    EmptyPrompt,
    #[error("transparent background cannot be combined with JPEG output; choose PNG or WebP")]
    TransparentJpeg,
    #[error("invalid image size '{0}'; use auto or WIDTHxHEIGHT")]
    InvalidImageSize(String),
    #[error(
        "invalid image size {width}x{height}: width and height must be between 16 and {maximum} pixels"
    )]
    ImageEdge {
        width: u32,
        height: u32,
        maximum: u32,
    },
    #[error(
        "invalid image size {width}x{height}: width and height must both be divisible by {step}"
    )]
    ImageAlignment { width: u32, height: u32, step: u32 },
    #[error(
        "invalid image size {width}x{height}: the long edge cannot be more than 3 times the short edge"
    )]
    ImageAspectRatio { width: u32, height: u32 },
    #[error(
        "invalid image size {width}x{height}: {pixels} total pixels is outside {minimum}–{maximum}"
    )]
    ImagePixelCount {
        width: u32,
        height: u32,
        pixels: u64,
        minimum: u64,
        maximum: u64,
    },
    #[error("unsupported {field} value: {value}")]
    UnsupportedValue { field: &'static str, value: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_generation_control_value() {
        assert_eq!("auto".parse(), Ok(ImageSize::Auto));
        assert_eq!("1024x1024".parse(), ImageSize::dimensions(1_024, 1_024));
        assert_eq!("1024x1536".parse(), ImageSize::dimensions(1_024, 1_536));
        assert_eq!("1536x1024".parse(), ImageSize::dimensions(1_536, 1_024));
        assert_eq!("3840x2160".parse(), ImageSize::dimensions(3_840, 2_160));
        assert_eq!("high".parse(), Ok(ImageQuality::High));
        assert_eq!("transparent".parse(), Ok(ImageBackground::Transparent));
        assert_eq!("WebP".parse(), Ok(OutputFormat::WebP));
        assert_eq!("JPEG".parse(), Ok(OutputFormat::Jpeg));
    }

    #[test]
    fn accepts_every_documented_dimension_boundary() {
        let minimum = ImageDimensions::new(1_024, 640).expect("minimum pixel count is valid");
        assert_eq!(minimum.pixel_count(), IMAGE_MIN_PIXELS);

        let maximum = ImageDimensions::new(3_840, 2_160).expect("4K boundary is valid");
        assert_eq!(maximum.pixel_count(), IMAGE_MAX_PIXELS);
        assert!(maximum.is_experimental());

        let widest_ratio = ImageDimensions::new(1_536, 512).expect("an exact 3:1 ratio is valid");
        assert_eq!(widest_ratio.width(), widest_ratio.height() * 3);

        assert!(
            !ImageDimensions::new(2_048, 1_152)
                .expect("2K landscape is valid")
                .is_experimental()
        );
    }

    #[test]
    fn rejects_each_documented_dimension_constraint() {
        assert!(matches!(
            ImageDimensions::new(4_096, 1_024),
            Err(GenerationValidationError::ImageEdge { .. })
        ));
        assert!(matches!(
            ImageDimensions::new(1_000, 1_024),
            Err(GenerationValidationError::ImageAlignment { .. })
        ));
        assert!(matches!(
            ImageDimensions::new(3_840, 1_024),
            Err(GenerationValidationError::ImageAspectRatio { .. })
        ));
        assert!(matches!(
            ImageDimensions::new(640, 640),
            Err(GenerationValidationError::ImagePixelCount { .. })
        ));
        assert!(matches!(
            ImageDimensions::new(3_840, 2_176),
            Err(GenerationValidationError::ImagePixelCount { .. })
        ));
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
