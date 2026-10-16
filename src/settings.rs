use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
}
