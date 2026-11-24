use crate::heif;
use crate::model::SourceFormat;
use image::DynamicImage;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone, Debug)]
pub struct ImageInfo {
    pub format: SourceFormat,
    pub width: u32,
    pub height: u32,
    pub has_alpha: bool,
    pub bytes: u64,
}

pub fn analyze(path: &Path) -> Result<ImageInfo, String> {
    let mut file = File::open(path).map_err(|err| err.to_string())?;
    let bytes = file.metadata().map_err(|err| err.to_string())?.len();
    let mut header = vec![0u8; 64 * 1024];
    let n = file.read(&mut header).map_err(|err| err.to_string())?;
    header.truncate(n);
    if header.len() >= 6 && (&header[..6] == b"GIF87a" || &header[..6] == b"GIF89a") {
        return Err("GIF is not supported".to_string());
    }
    let format = sniff(&header).ok_or_else(|| "not a JPEG, PNG, WebP, or HEIC".to_string())?;
    match format {
        SourceFormat::Png => {
            let (animated, has_alpha) = png_flags(&header)?;
            if animated {
                return Err("animated PNG is not supported".to_string());
            }
            let (width, height) = dimensions(path)?;
            Ok(ImageInfo {
                format,
                width,
                height,
                has_alpha,
                bytes,
            })
        }
        SourceFormat::Webp => {
            let (animated, has_alpha) = webp_flags(&header);
            if animated {
                return Err("animated WebP is not supported".to_string());
            }
            let (width, height) = dimensions(path)?;
            Ok(ImageInfo {
                format,
                width,
                height,
                has_alpha,
                bytes,
            })
        }
        SourceFormat::Jpeg => {
            let (width, height) = dimensions(path)?;
            Ok(ImageInfo {
                format,
                width,
                height,
                has_alpha: false,
                bytes,
            })
        }
        SourceFormat::Heif => inspect_heif(path, bytes),
    }
}

/// Decode once so the list can show the upright size and whether there is transparency.
fn inspect_heif(path: &Path, bytes: u64) -> Result<ImageInfo, String> {
    let tools = crate::tools::locate()?;
    let scratch = TempDir::new("heif-info")?;
    let png = scratch.0.join("decoded.png");
    heif::decode_to_png(&tools.heif_dec, path, &png)?;
    let mut file = File::open(&png).map_err(|err| err.to_string())?;
    let mut data = Vec::new();
    file.read_to_end(&mut data).map_err(|err| err.to_string())?;
    let (_animated, has_alpha) = png_flags(&data)?;
    let (width, height) = dimensions(&png)?;
    Ok(ImageInfo {
        format: SourceFormat::Heif,
        width,
        height,
        has_alpha,
        bytes,
    })
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "image-optimizer-{}-{label}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).map_err(|err| err.to_string())?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn sniff(header: &[u8]) -> Option<SourceFormat> {
    if header.len() >= 3 && header[0] == 0xff && header[1] == 0xd8 && header[2] == 0xff {
        Some(SourceFormat::Jpeg)
    } else if header.len() >= 8 && &header[..8] == PNG_SIG {
        Some(SourceFormat::Png)
    } else if header.len() >= 12 && &header[..4] == b"RIFF" && &header[8..12] == b"WEBP" {
        Some(SourceFormat::Webp)
    } else if is_heif(header) {
        Some(SourceFormat::Heif)
    } else {
        None
    }
}

/// ISO-BMFF `ftyp` brands used by iPhone HEIC and other still HEIF photos.
/// AVIF also lists `mif1`, so a file that names `avif` or `avis` is left alone.
fn is_heif(header: &[u8]) -> bool {
    if header.len() < 16 || &header[4..8] != b"ftyp" {
        return false;
    }
    let box_size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if box_size < 16 {
        return false;
    }
    let end = box_size.min(header.len());
    let mut offset = 8usize;
    let mut brands = Vec::new();
    while offset + 4 <= end {
        // Byte 12 is the minor version, not a brand.
        if offset != 12 {
            brands.push([
                header[offset],
                header[offset + 1],
                header[offset + 2],
                header[offset + 3],
            ]);
        }
        offset += 4;
    }
    if brands
        .iter()
        .any(|brand| brand == b"avif" || brand == b"avis")
    {
        return false;
    }
    brands.iter().any(|brand| {
        matches!(
            brand,
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1"
        )
    })
}

fn dimensions(path: &Path) -> Result<(u32, u32), String> {
    image::image_dimensions(path).map_err(|err| err.to_string())
}

/// `(animated, has_alpha)`.
fn png_flags(data: &[u8]) -> Result<(bool, bool), String> {
    if data.len() < 26 || &data[..8] != PNG_SIG {
        return Err("truncated PNG".to_string());
    }
    let color_type = data[25];
    let mut has_alpha = color_type == 4 || color_type == 6;
    let mut animated = false;
    let mut index = 8usize;
    while index + 8 <= data.len() {
        let len = u32::from_be_bytes(data[index..index + 4].try_into().unwrap()) as usize;
        let kind = &data[index + 4..index + 8];
        if kind == b"acTL" {
            animated = true;
        }
        if kind == b"tRNS" {
            has_alpha = true;
        }
        if kind == b"IDAT" || kind == b"IEND" {
            break;
        }
        let next = index.saturating_add(12).saturating_add(len);
        if next <= index {
            break;
        }
        index = next;
    }
    Ok((animated, has_alpha))
}

/// VP8X feature bits used by libwebp: animation = 0x02, alpha = 0x10.
fn webp_flags(data: &[u8]) -> (bool, bool) {
    if data.len() < 12 || &data[..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return (false, false);
    }
    let mut index = 12usize;
    let mut animated = false;
    let mut alpha = false;
    while index + 8 <= data.len() {
        let kind = &data[index..index + 4];
        let len = u32::from_le_bytes(data[index + 4..index + 8].try_into().unwrap()) as usize;
        if kind == b"VP8X" && index + 9 <= data.len() {
            let flags = data[index + 8];
            if flags & 0x02 != 0 {
                animated = true;
            }
            if flags & 0x10 != 0 {
                alpha = true;
            }
        }
        if kind == b"ALPH" {
            alpha = true;
        }
        if kind == b"ANIM" {
            animated = true;
        }
        let padded = len + (len % 2);
        let next = index.saturating_add(8).saturating_add(padded);
        if next <= index {
            break;
        }
        index = next;
    }
    (animated, alpha)
}

/// Stops once more than `cap` colors are seen. Alpha is true when any pixel is translucent.
pub fn scan_image(img: &DynamicImage, cap: u32) -> (bool, u32) {
    let rgba = img.to_rgba8();
    let mut colors = HashSet::new();
    let mut alpha = false;
    for pixel in rgba.pixels() {
        if pixel[3] < 255 {
            alpha = true;
        }
        if (colors.len() as u32) <= cap {
            colors.insert((pixel[0], pixel[1], pixel[2]));
        }
        if alpha && colors.len() as u32 > cap {
            break;
        }
    }
    (alpha, colors.len() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_magic() {
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0x00]), Some(SourceFormat::Jpeg));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some(SourceFormat::Png));
        assert_eq!(sniff(b"RIFF0000WEBPmore"), Some(SourceFormat::Webp));
        assert_eq!(sniff(b"GIF89a"), None);
        assert_eq!(sniff(&ftyp(b"heic", &[b"mif1"])), Some(SourceFormat::Heif));
        assert_eq!(sniff(&ftyp(b"heix", &[])), Some(SourceFormat::Heif));
        assert_eq!(sniff(&ftyp(b"mif1", &[b"heic"])), Some(SourceFormat::Heif));
        assert_eq!(sniff(&ftyp(b"msf1", &[])), Some(SourceFormat::Heif));
        assert_eq!(sniff(&ftyp(b"avif", &[b"mif1", b"avif"])), None);
    }

    fn ftyp(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
        let size = 16 + 4 * compatible.len();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(size as u32).to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(major);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        for brand in compatible {
            bytes.extend_from_slice(*brand);
        }
        bytes
    }
}
