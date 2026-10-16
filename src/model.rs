//! Formats, encode targets, and the recommend picker.
//!
//! WebP has to beat the compatible format (JPEG for photos, PNG for graphics)
//! by at least 8% or the picker stays with the compatible format. A chosen
//! result that saves under 3% versus the original is reported as "keep".

use serde::{Deserialize, Serialize};

pub const MIN_SAVINGS: f64 = 0.03;
pub const WEBP_BEAT_BY: f64 = 0.08;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFormat {
    Jpeg,
    Png,
    Webp,
}

impl SourceFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::Webp => "WebP",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebpMode {
    Lossy,
    Lossless,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encode {
    Jpeg,
    Png,
    Webp(WebpMode),
}

impl Encode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::Webp(WebpMode::Lossy) => "WebP",
            Self::Webp(WebpMode::Lossless) => "WebP lossless",
        }
    }

    pub fn extension(self, source_ext: &str) -> String {
        match self {
            Self::Jpeg => {
                if source_ext.eq_ignore_ascii_case("jpeg") {
                    "jpeg".to_string()
                } else {
                    "jpg".to_string()
                }
            }
            Self::Png => "png".to_string(),
            Self::Webp(_) => "webp".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Keep,
    Use(Encode),
}

impl Decision {
    pub fn label(self) -> &'static str {
        match self {
            Self::Keep => "Keep original",
            Self::Use(encode) => encode.label(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowChoice {
    Recommended,
    Jpeg,
    Png,
    Webp,
    Skip,
}

impl RowChoice {
    pub const ALL: [Self; 5] = [
        Self::Recommended,
        Self::Jpeg,
        Self::Png,
        Self::Webp,
        Self::Skip,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Recommended => "Best",
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::Webp => "WebP",
            Self::Skip => "Skip",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Recommendation {
    pub decision: Decision,
    pub sizes: Vec<(Encode, u64)>,
    pub photo: bool,
}

impl Recommendation {
    pub fn summary(&self) -> String {
        self.sizes
            .iter()
            .map(|(encode, bytes)| format!("{} {}", encode.label(), format_bytes(*bytes)))
            .collect::<Vec<_>>()
            .join(" · ")
    }

    pub fn chosen_bytes(&self) -> Option<u64> {
        match self.decision {
            Decision::Keep => None,
            Decision::Use(encode) => self
                .sizes
                .iter()
                .find(|(candidate, _)| *candidate == encode)
                .map(|(_, bytes)| *bytes),
        }
    }
}

/// Photos are JPEGs, and PNG/WebP images with no alpha and more than 256 colors.
/// A missing color count is treated as a photo so a large PNG is not pngcrushed.
pub fn is_photo(format: SourceFormat, has_alpha: bool, color_count: Option<u32>) -> bool {
    if has_alpha {
        return false;
    }
    match format {
        SourceFormat::Jpeg => true,
        SourceFormat::Png | SourceFormat::Webp => color_count.map(|count| count > 256).unwrap_or(true),
    }
}

pub fn encodes_for(
    format: SourceFormat,
    has_alpha: bool,
    color_count: Option<u32>,
    webp_lossless: bool,
) -> (bool, Vec<Encode>) {
    let photo = is_photo(format, has_alpha, color_count);
    // Graphics (and anything with alpha) are compared as lossless WebP.
    // Photos use lossy WebP unless the user turned lossless on.
    let webp_mode = if !photo || has_alpha || webp_lossless {
        WebpMode::Lossless
    } else {
        WebpMode::Lossy
    };
    let mut encodes = Vec::new();
    if photo {
        encodes.push(Encode::Jpeg);
    } else if format == SourceFormat::Png || has_alpha {
        encodes.push(Encode::Png);
    }
    encodes.push(Encode::Webp(webp_mode));
    (photo, encodes)
}

pub fn pick(source_bytes: u64, photo: bool, candidates: &[(Encode, u64)]) -> Decision {
    let kept: Vec<(Encode, u64)> = candidates
        .iter()
        .copied()
        .filter(|(_, bytes)| *bytes <= source_bytes)
        .collect();
    let compat = kept.iter().copied().find(|(encode, _)| {
        if photo {
            matches!(encode, Encode::Jpeg)
        } else {
            matches!(encode, Encode::Png)
        }
    });
    let webp = kept
        .iter()
        .copied()
        .find(|(encode, _)| matches!(encode, Encode::Webp(_)));

    let chosen = match (compat, webp) {
        (Some((compat_encode, compat_bytes)), Some((webp_encode, webp_bytes))) => {
            let beat = if compat_bytes == 0 {
                1.0
            } else {
                (compat_bytes as f64 - webp_bytes as f64) / compat_bytes as f64
            };
            if webp_bytes < compat_bytes && beat >= WEBP_BEAT_BY - 1e-9 {
                webp_encode
            } else {
                compat_encode
            }
        }
        (Some((encode, _)), None) => encode,
        (None, Some((encode, _))) => encode,
        (None, None) => match kept.iter().min_by_key(|(_, bytes)| *bytes) {
            Some((encode, _)) => *encode,
            None => return Decision::Keep,
        },
    };

    let Some(bytes) = kept
        .iter()
        .find(|(encode, _)| *encode == chosen)
        .map(|(_, bytes)| *bytes)
    else {
        return Decision::Keep;
    };
    if source_bytes == 0 {
        return Decision::Use(chosen);
    }
    let saved = (source_bytes as f64 - bytes as f64) / source_bytes as f64;
    if saved < MIN_SAVINGS {
        Decision::Keep
    } else {
        Decision::Use(chosen)
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let n = bytes as f64;
    if n < KIB {
        format!("{bytes} B")
    } else if n < KIB * KIB {
        format!("{:.1} KB", n / KIB)
    } else {
        format!("{:.2} MB", n / (KIB * KIB))
    }
}

pub fn saved_percent(before: u64, after: u64) -> f64 {
    if before == 0 {
        0.0
    } else {
        (before as f64 - after as f64) / before as f64 * 100.0
    }
}

pub fn fit(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let max_edge = max_edge.max(1);
    let long = width.max(height);
    if long <= max_edge || long == 0 {
        return (width.max(1), height.max(1));
    }
    let scale = max_edge as f64 / long as f64;
    let w = ((width as f64) * scale).round().max(1.0) as u32;
    let h = ((height as f64) * scale).round().max(1.0) as u32;
    (w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webp_must_beat_jpeg_by_eight_percent() {
        let source = 1000;
        // 5% smaller than the JPEG result, so JPEG wins. JPEG itself saves 20%.
        let slight = [(Encode::Jpeg, 800), (Encode::Webp(WebpMode::Lossy), 760)];
        assert_eq!(pick(source, true, &slight), Decision::Use(Encode::Jpeg));

        let clear = [(Encode::Jpeg, 1000), (Encode::Webp(WebpMode::Lossy), 900)];
        assert_eq!(
            pick(source, true, &clear),
            Decision::Use(Encode::Webp(WebpMode::Lossy))
        );

        let exact = [(Encode::Jpeg, 1000), (Encode::Webp(WebpMode::Lossy), 920)];
        assert_eq!(
            pick(source, true, &exact),
            Decision::Use(Encode::Webp(WebpMode::Lossy))
        );
    }

    #[test]
    fn tiny_savings_keep_the_original() {
        let candidates = [(Encode::Jpeg, 990), (Encode::Webp(WebpMode::Lossy), 980)];
        assert_eq!(pick(1000, true, &candidates), Decision::Keep);
    }

    #[test]
    fn dropped_jpeg_still_allows_webp() {
        let candidates = [(Encode::Jpeg, 1100), (Encode::Webp(WebpMode::Lossy), 500)];
        assert_eq!(
            pick(1000, true, &candidates),
            Decision::Use(Encode::Webp(WebpMode::Lossy))
        );
    }

    #[test]
    fn graphics_prefer_png_unless_webp_wins_clearly() {
        let close = [(Encode::Png, 700), (Encode::Webp(WebpMode::Lossless), 660)];
        assert_eq!(pick(1000, false, &close), Decision::Use(Encode::Png));

        let clear = [(Encode::Png, 700), (Encode::Webp(WebpMode::Lossless), 600)];
        assert_eq!(
            pick(1000, false, &clear),
            Decision::Use(Encode::Webp(WebpMode::Lossless))
        );
    }

    #[test]
    fn photo_detection() {
        assert!(is_photo(SourceFormat::Jpeg, false, None));
        assert!(!is_photo(SourceFormat::Png, true, Some(10_000)));
        assert!(is_photo(SourceFormat::Png, false, Some(257)));
        assert!(!is_photo(SourceFormat::Png, false, Some(256)));
        assert!(is_photo(SourceFormat::Webp, false, Some(1000)));
    }

    #[test]
    fn graphics_request_png_and_lossless_webp() {
        let (photo, encodes) = encodes_for(SourceFormat::Png, true, Some(10), false);
        assert!(!photo);
        assert_eq!(
            encodes,
            vec![Encode::Png, Encode::Webp(WebpMode::Lossless)]
        );
    }

    #[test]
    fn photos_request_jpeg_and_lossy_webp() {
        let (photo, encodes) = encodes_for(SourceFormat::Jpeg, false, None, false);
        assert!(photo);
        assert_eq!(
            encodes,
            vec![Encode::Jpeg, Encode::Webp(WebpMode::Lossy)]
        );
    }
}
