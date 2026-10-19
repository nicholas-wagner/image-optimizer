use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Preset {
    Web,
    Compress,
    Lossless,
    Custom,
}

impl Default for Preset {
    fn default() -> Self {
        Self::Compress
    }
}

impl Preset {
    pub const ALL: [Self; 4] = [Self::Web, Self::Compress, Self::Lossless, Self::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Self::Web => "Web",
            Self::Compress => "Compress",
            Self::Lossless => "Lossless",
            Self::Custom => "Custom",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Self::Web => "Shrink the long side to 1600 px, then pick the smaller format.",
            Self::Compress => "Keep the full size. JPEG max quality 90.",
            Self::Lossless => "No quality loss. Best still picks JPEG, PNG, or WebP.",
            Self::Custom => "The controls under Advanced options.",
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
            Self::Lossless => {
                settings.jpeg_lossless = true;
                settings.webp_lossless = true;
                settings.resize_enabled = false;
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
    #[serde(default = "default_true")]
    pub choose_best_format: bool,
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
            webp_quality: default_webp_quality(),
            webp_lossless: false,
            strip_metadata: false,
            resize_enabled: false,
            resize_max_edge: default_max_edge(),
            png_brute: false,
            choose_best_format: true,
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
            && self.webp_quality == other.webp_quality
            && self.webp_lossless == other.webp_lossless
            && self.strip_metadata == other.strip_metadata
            && self.resize_enabled == other.resize_enabled
            && self.resize_max_edge == other.resize_max_edge
            && self.png_brute == other.png_brute
            && self.choose_best_format == other.choose_best_format
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
        for candidate in [Preset::Lossless, Preset::Web, Preset::Compress] {
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
