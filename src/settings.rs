use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// JPEG cap for For email. On four photos resized to a 1200 px long side,
/// quality 85 was about 21% smaller than 90. A portrait lost 0.020 SSIM
/// against 90, versus 0.036 at quality 80.
pub const EMAIL_JPEG_QUALITY: u8 = 85;
pub const EMAIL_MAX_EDGE: u32 = 1200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Preset {
    Web,
    /// Photos become progressive JPEG. Graphics and transparency stay PNG.
    Email,
    Compress,
    Lossless,
    /// Every file is saved as PNG, with maximum compression.
    Png,
    Custom,
}

impl Default for Preset {
    fn default() -> Self {
        Self::Compress
    }
}

impl Preset {
    pub const ALL: [Self; 6] = [
        Self::Web,
        Self::Email,
        Self::Compress,
        Self::Lossless,
        Self::Png,
        Self::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Web => "Web",
            Self::Email => "Email",
            Self::Compress => "Compress",
            Self::Lossless => "Lossless",
            Self::Png => "PNG",
            Self::Custom => "Custom",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Self::Web => "Shrink the long side to 1600 px, then pick the smaller format.",
            Self::Email => {
                "Shrink the long side to 1200 px. Photos save as progressive JPEG. Graphics and transparency stay PNG. WebP is never used."
            }
            Self::Compress => {
                "Photos and large images save as JPEG quality 90, 4:2:0. Small art stays PNG. WebP is used when it is at least 8% smaller."
            }
            Self::Lossless => "No quality loss. Best still picks JPEG, PNG, or WebP.",
            Self::Png => {
                "Every file is saved as PNG. Maximum compression is on, so large images take longer. Pick another style to let Best choose the format again."
            }
            Self::Custom => {
                "Opens the controls for quality, size, and PNG. This card is selected when a style no longer matches."
            }
        }
    }

    pub fn baseline(self) -> Settings {
        let mut settings = Settings {
            preset: self,
            ..Settings::default()
        };
        match self {
            Self::Web => {
                settings.resize_enabled = true;
                settings.resize_max_edge = default_max_edge();
            }
            Self::Email => {
                settings.jpeg_quality = EMAIL_JPEG_QUALITY;
                settings.jpeg_progressive = true;
                settings.strip_metadata = true;
                settings.resize_enabled = true;
                settings.resize_max_edge = EMAIL_MAX_EDGE;
                settings.webp_allowed = false;
                settings.png_brute = false;
            }
            Self::Lossless => {
                settings.jpeg_lossless = true;
                settings.webp_lossless = true;
                settings.resize_enabled = false;
            }
            Self::Png => {
                settings.png_brute = true;
                settings.png_only = true;
            }
            Self::Compress | Self::Custom => {}
        }
        settings
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    #[serde(default = "default_jpeg_quality")]
    pub jpeg_quality: u8,
    #[serde(default)]
    pub jpeg_lossless: bool,
    /// jpegoptim --all-progressive. On for For email.
    #[serde(default)]
    pub jpeg_progressive: bool,
    #[serde(default = "default_webp_quality")]
    pub webp_quality: u8,
    #[serde(default)]
    pub webp_lossless: bool,
    #[serde(default)]
    pub strip_metadata: bool,
    #[serde(default)]
    pub resize_enabled: bool,
    #[serde(default = "default_max_edge")]
    pub resize_max_edge: u32,
    #[serde(default)]
    pub png_brute: bool,
    /// Smallest PNG. New files are saved as PNG until another style is picked.
    #[serde(default)]
    pub png_only: bool,
    #[serde(default = "default_true")]
    pub choose_best_format: bool,
    /// Best may choose WebP. For email turns this off.
    #[serde(default = "default_true")]
    pub webp_allowed: bool,
    #[serde(default)]
    pub output_dir: Option<PathBuf>,
    #[serde(default)]
    pub preset: Preset,
}

fn default_true() -> bool {
    true
}

fn default_jpeg_quality() -> u8 {
    90
}

fn default_webp_quality() -> u8 {
    80
}

fn default_max_edge() -> u32 {
    1600
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            jpeg_quality: default_jpeg_quality(),
            jpeg_lossless: false,
            jpeg_progressive: false,
            webp_quality: default_webp_quality(),
            webp_lossless: false,
            strip_metadata: false,
            resize_enabled: false,
            resize_max_edge: default_max_edge(),
            png_brute: false,
            png_only: false,
            choose_best_format: true,
            webp_allowed: true,
            output_dir: None,
            preset: Preset::Compress,
        }
    }
}

impl Settings {
    pub fn clamp(&mut self) {
        self.jpeg_quality = self.jpeg_quality.min(100);
        self.webp_quality = self.webp_quality.min(100);
        if self.resize_max_edge == 0 {
            self.resize_max_edge = 1;
        }
    }

    /// Encode choices and the output folder. The preset tag is not part of the job.
    pub fn same_job(&self, other: &Self) -> bool {
        self.jpeg_quality == other.jpeg_quality
            && self.jpeg_lossless == other.jpeg_lossless
            && self.jpeg_progressive == other.jpeg_progressive
            && self.webp_quality == other.webp_quality
            && self.webp_lossless == other.webp_lossless
            && self.strip_metadata == other.strip_metadata
            && self.resize_enabled == other.resize_enabled
            && self.resize_max_edge == other.resize_max_edge
            && self.png_brute == other.png_brute
            && self.png_only == other.png_only
            && self.choose_best_format == other.choose_best_format
            && self.webp_allowed == other.webp_allowed
            && self.output_dir == other.output_dir
    }

    pub fn apply_preset(&mut self, preset: Preset) {
        if preset == Preset::Custom {
            self.preset = Preset::Custom;
            return;
        }
        let output_dir = self.output_dir.clone();
        *self = preset.baseline();
        self.output_dir = output_dir;
        self.preset = preset;
    }

    /// Highlight Web, Compress, or Lossless when the knobs match. Anything else is Custom.
    pub fn sync_preset(&mut self) {
        self.clamp();
        for candidate in [
            Preset::Lossless,
            Preset::Web,
            Preset::Email,
            Preset::Png,
            Preset::Compress,
        ] {
            let mut base = candidate.baseline();
            base.output_dir = self.output_dir.clone();
            if self.same_job(&base) {
                self.preset = candidate;
                return;
            }
        }
        self.preset = Preset::Custom;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_is_the_old_defaults() {
        let compress = Preset::Compress.baseline();
        assert!(!compress.resize_enabled);
        assert_eq!(compress.jpeg_quality, 90);
        assert_eq!(compress.webp_quality, 80);
        assert!(compress.choose_best_format);
        assert!(compress.same_job(&Settings::default()));
        assert_eq!(compress.preset, Preset::Compress);
    }

    #[test]
    fn web_turns_resize_on_and_keeps_the_output_folder() {
        let mut settings = Settings::default();
        settings.output_dir = Some(PathBuf::from("/tmp/out"));
        settings.apply_preset(Preset::Web);
        assert!(settings.resize_enabled);
        assert_eq!(settings.resize_max_edge, 1600);
        assert_eq!(settings.jpeg_quality, 90);
        assert!(!settings.jpeg_lossless);
        assert!(!settings.webp_lossless);
        assert_eq!(settings.output_dir, Some(PathBuf::from("/tmp/out")));
        assert_eq!(settings.preset, Preset::Web);
    }

    #[test]
    fn lossless_locks_jpeg_and_webp() {
        let mut settings = Settings::default();
        settings.resize_enabled = true;
        settings.apply_preset(Preset::Lossless);
        assert!(settings.jpeg_lossless);
        assert!(settings.webp_lossless);
        assert!(!settings.resize_enabled);
        assert!(settings.choose_best_format);
        assert_eq!(settings.preset, Preset::Lossless);
    }

    #[test]
    fn a_slider_edit_becomes_custom() {
        let mut settings = Preset::Web.baseline();
        settings.jpeg_quality = 70;
        settings.sync_preset();
        assert_eq!(settings.preset, Preset::Custom);
    }

    #[test]
    fn the_output_folder_does_not_change_the_preset() {
        let mut settings = Preset::Web.baseline();
        settings.output_dir = Some(PathBuf::from("/tmp/out"));
        settings.sync_preset();
        assert_eq!(settings.preset, Preset::Web);
    }

    #[test]
    fn a_saved_compress_tag_with_custom_knobs_does_not_stay_compress() {
        let mut settings = Settings::default();
        settings.jpeg_quality = 70;
        settings.preset = Preset::Compress;
        settings.sync_preset();
        assert_eq!(settings.preset, Preset::Custom);
    }

    #[test]
    fn smallest_png_is_maximum_compression_and_png_output() {
        let png = Preset::Png.baseline();
        assert!(png.png_brute);
        assert!(png.png_only);
        assert!(!png.resize_enabled);
        assert!(!png.jpeg_lossless);
        assert!(!png.webp_lossless);
        assert!(png.choose_best_format);
        assert_eq!(png.jpeg_quality, 90);
        assert!(!png.same_job(&Preset::Compress.baseline()));
        let mut synced = png.clone();
        synced.sync_preset();
        assert_eq!(synced.preset, Preset::Png);

        let mut job = Preset::Compress.baseline();
        job.png_brute = true;
        assert!(!job.same_job(&png));
        job.png_only = true;
        assert!(job.same_job(&png));
    }

    #[test]
    fn turning_off_maximum_compression_keeps_png_output_as_your_settings() {
        let mut settings = Preset::Png.baseline();
        settings.png_brute = false;
        settings.sync_preset();
        assert_eq!(settings.preset, Preset::Custom);
        assert!(settings.png_only);
    }

    #[test]
    fn png_output_without_maximum_compression_is_not_the_png_style() {
        let mut settings = Preset::Compress.baseline();
        settings.png_only = true;
        settings.sync_preset();
        assert_eq!(settings.preset, Preset::Custom);
    }

    #[test]
    fn email_is_jpeg_or_png_at_1200_with_metadata_stripped() {
        let email = Preset::Email.baseline();
        assert_eq!(email.jpeg_quality, EMAIL_JPEG_QUALITY);
        assert!(email.jpeg_progressive);
        assert!(email.strip_metadata);
        assert!(email.resize_enabled);
        assert_eq!(email.resize_max_edge, EMAIL_MAX_EDGE);
        assert!(!email.webp_allowed);
        assert!(!email.png_brute);
        assert!(!email.png_only);
        assert!(!email.jpeg_lossless);
        assert!(!email.webp_lossless);
        assert!(email.choose_best_format);
        assert!(!email.same_job(&Preset::Web.baseline()));
        assert!(!email.same_job(&Preset::Compress.baseline()));
        let mut synced = email.clone();
        synced.sync_preset();
        assert_eq!(synced.preset, Preset::Email);
    }

    #[test]
    fn changing_email_quality_becomes_your_settings_and_repicking_keeps_the_folder() {
        let mut settings = Preset::Email.baseline();
        settings.output_dir = Some(PathBuf::from("/tmp/out"));
        settings.jpeg_quality = 70;
        settings.sync_preset();
        assert_eq!(settings.preset, Preset::Custom);
        assert!(!settings.webp_allowed);
        assert!(settings.jpeg_progressive);
        settings.apply_preset(Preset::Email);
        assert_eq!(settings.preset, Preset::Email);
        assert_eq!(settings.jpeg_quality, EMAIL_JPEG_QUALITY);
        assert_eq!(settings.resize_max_edge, EMAIL_MAX_EDGE);
        assert_eq!(settings.output_dir, Some(PathBuf::from("/tmp/out")));
    }

    #[test]
    fn saved_settings_without_the_new_fields_stay_on_compress() {
        let settings: Settings =
            serde_json::from_str(r#"{"jpeg_quality":90,"preset":"Compress"}"#).unwrap();
        assert!(settings.webp_allowed);
        assert!(!settings.jpeg_progressive);
        assert!(!settings.strip_metadata);
        let mut synced = settings;
        synced.sync_preset();
        assert_eq!(synced.preset, Preset::Compress);
    }

    #[test]
    fn same_job_ignores_the_preset_tag() {
        let mut a = Preset::Compress.baseline();
        let mut b = a.clone();
        b.preset = Preset::Custom;
        assert!(a.same_job(&b));
        b.output_dir = Some(PathBuf::from("/tmp/out"));
        assert!(!a.same_job(&b));
        a.output_dir = Some(PathBuf::from("/tmp/out"));
        assert!(a.same_job(&b));
    }
}
