//! Runs jpegoptim, pngcrush, and cwebp. Originals are never overwritten.
//!
//! jpegoptim does not copy a file into `--dest` when it cannot shrink it.
//! The output is staged in a temp directory and copied into place, and the
//! untouched input is copied when jpegoptim skips the file.

use crate::analyze::{self, ImageInfo};
use crate::commands::{self, display_command};
use crate::model::{self, Decision, Encode, Recommendation, SourceFormat, WebpMode};
use crate::settings::Settings;
use crate::tools::ToolPaths;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder};
use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

static WORK_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct CancelFlag {
    flag: Arc<AtomicBool>,
    pids: Arc<Mutex<Vec<u32>>>,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            pids: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
        let pids = self.pids.lock().map(|guard| guard.clone()).unwrap_or_default();
        for pid in pids {
            kill_pid(pid);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

impl Default for CancelFlag {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct OptimizeOutcome {
    pub output_path: PathBuf,
    pub output_bytes: u64,
    pub commands: Vec<String>,
    pub note: String,
}

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Result<Self, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = WORK_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "image-optimizer-{}-{label}-{n}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).map_err(|err| err.to_string())?;
        Ok(Self { path })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn recommend_file(
    tools: &ToolPaths,
    settings: &Settings,
    src: &Path,
    info: &ImageInfo,
    cancel: &CancelFlag,
) -> Result<Recommendation, String> {
    let (has_alpha, color_count) = if info.format == SourceFormat::Jpeg {
        (false, None)
    } else {
        let image = image::open(src).map_err(|err| err.to_string())?;
        let (alpha, count) = analyze::scan_image(&image, 256);
        (alpha || info.has_alpha, Some(count))
    };
    let (photo, encodes) = model::encodes_for(
        info.format,
        has_alpha,
        color_count,
        info.width,
        info.height,
        settings.webp_lossless,
        settings.webp_allowed,
    );
    let scratch = Scratch::new("recommend")?;
    let mut sizes = Vec::new();
    for encode in encodes {
        if cancel.is_cancelled() {
            return Err("Cancelled".to_string());
        }
        let out_dir = scratch.path.join(encode.label().replace(' ', "-"));
        fs::create_dir_all(&out_dir).map_err(|err| err.to_string())?;
        let outcome = encode_one(tools, settings, src, info, encode, &out_dir, cancel)?;
        sizes.push((encode, outcome.output_bytes));
    }
    let decision = model::pick(info.bytes, photo, &sizes);
    Ok(Recommendation {
        decision,
        sizes,
        photo,
    })
}

pub fn optimize_file(
    tools: &ToolPaths,
    settings: &Settings,
    src: &Path,
    info: &ImageInfo,
    encode: Encode,
    output_dir: &Path,
    cancel: &CancelFlag,
) -> Result<OptimizeOutcome, String> {
    encode_one(tools, settings, src, info, encode, output_dir, cancel)
}

/// Encode WebP in a temp directory and return its size. The file is deleted.
pub fn measure_webp(
    tools: &ToolPaths,
    settings: &Settings,
    src: &Path,
    info: &ImageInfo,
    mode: WebpMode,
    cancel: &CancelFlag,
) -> Result<u64, String> {
    let scratch = Scratch::new("webp-hint")?;
    let out_dir = scratch.path.join("webp");
    fs::create_dir_all(&out_dir).map_err(|err| err.to_string())?;
    let outcome = encode_one(
        tools,
        settings,
        src,
        info,
        Encode::Webp(mode),
        &out_dir,
        cancel,
    )?;
    Ok(outcome.output_bytes)
}

fn encode_one(
    tools: &ToolPaths,
    settings: &Settings,
    src: &Path,
    info: &ImageInfo,
    encode: Encode,
    output_dir: &Path,
    cancel: &CancelFlag,
) -> Result<OptimizeOutcome, String> {
    if cancel.is_cancelled() {
        return Err("Cancelled".to_string());
    }
    fs::create_dir_all(output_dir).map_err(|err| err.to_string())?;
    let final_path = output_dir.join(output_name(src, encode));
    if same_file(src, &final_path) {
        return Err("Refusing to overwrite the original".to_string());
    }
    let scratch = Scratch::new("encode")?;
    let mut note = String::new();
    let input = materialize_input(src, info, settings, encode, &scratch, &mut note)?;
    if same_file(src, &final_path) {
        return Err("Refusing to overwrite the original".to_string());
    }
    if final_path.exists() {
        fs::remove_file(&final_path).map_err(|err| err.to_string())?;
    }

    let mut commands = Vec::new();
    let output_bytes = match encode {
        Encode::Jpeg => {
            let staging = scratch.path.join("jpeg-out");
            fs::create_dir_all(&staging).map_err(|err| err.to_string())?;
            let args = commands::jpegoptim_args(settings, &input, &staging);
            commands.push(display_command(&tools.jpegoptim, &args));
            run_tool(cancel, &tools.jpegoptim, &args)?;
            let produced = staging.join(input.file_name().unwrap_or_default());
            copy_file(if produced.is_file() { &produced } else { &input }, &final_path)?
        }
        Encode::Png => {
            let args = commands::pngcrush_args(settings, &input, &final_path);
            commands.push(display_command(&tools.pngcrush, &args));
            run_tool(cancel, &tools.pngcrush, &args)?;
            file_len(&final_path)?
        }
        Encode::Webp(mode) => {
            let args = commands::cwebp_args(settings, mode, &input, &final_path);
            commands.push(display_command(&tools.cwebp, &args));
            run_tool(cancel, &tools.cwebp, &args)?;
            file_len(&final_path)?
        }
    };
    if !final_path.is_file() {
        return Err(format!("{} did not write an output file", encode.label()));
    }
    Ok(OptimizeOutcome {
        output_path: final_path,
        output_bytes,
        commands,
        note,
    })
}

fn materialize_input(
    src: &Path,
    info: &ImageInfo,
    settings: &Settings,
    encode: Encode,
    scratch: &Scratch,
    note: &mut String,
) -> Result<PathBuf, String> {
    let resize = settings.resize_enabled;
    let pass_through = !resize
        && match encode {
            Encode::Jpeg => info.format == SourceFormat::Jpeg,
            Encode::Png => info.format == SourceFormat::Png,
            Encode::Webp(_) => true,
        };
    if pass_through {
        return Ok(src.to_path_buf());
    }

    let (image, profile) = load_image(src)?;
    let (width, height) = (image.width(), image.height());
    let image = if resize {
        let (nw, nh) = model::fit(width, height, settings.resize_max_edge);
        if nw != width || nh != height {
            note.push_str(&format!("resized to {nw}x{nh}"));
            image.resize(nw, nh, image::imageops::FilterType::Lanczos3)
        } else {
            image
        }
    } else {
        image
    };

    match encode {
        Encode::Jpeg => {
            if image.color().has_alpha() {
                if !note.is_empty() {
                    note.push_str("; ");
                }
                note.push_str("alpha flattened onto white");
            }
            let path = scratch.path.join("input.jpg");
            write_jpeg(&image, &path, 100, profile.as_deref())?;
            Ok(path)
        }
        Encode::Png | Encode::Webp(_) => {
            let path = scratch.path.join("input.png");
            write_png(&image, &path, profile.as_deref())?;
            Ok(path)
        }
    }
}

/// Decode pixels, apply the Exif orientation, and keep the ICC profile.
/// Stripping metadata later can drop the orientation tag. The pixels are
/// turned upright first, and the profile stays with them so colors do not shift.
fn load_image(path: &Path) -> Result<(DynamicImage, Option<Vec<u8>>), String> {
    let reader = image::ImageReader::open(path)
        .map_err(|err| err.to_string())?
        .with_guessed_format()
        .map_err(|err| err.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|err| err.to_string())?;
    let profile = decoder
        .icc_profile()
        .ok()
        .flatten()
        .filter(|bytes| !bytes.is_empty());
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(|err| err.to_string())?;
    image.apply_orientation(orientation);
    Ok((image, profile))
}

fn write_jpeg(
    image: &DynamicImage,
    path: &Path,
    quality: u8,
    icc: Option<&[u8]>,
) -> Result<(), String> {
    // JpegEncoder has no subsampling control. It writes 4:4:4. A later
    // lossy jpegoptim --max recompresses that to libjpeg's default 4:2:0.
    let rgb = flatten_rgb(image);
    let file = fs::File::create(path).map_err(|err| err.to_string())?;
    let mut encoder = JpegEncoder::new_with_quality(BufWriter::new(file), quality);
    if let Some(profile) = icc {
        encoder
            .set_icc_profile(profile.to_vec())
            .map_err(|err| err.to_string())?;
    }
    encoder
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|err| err.to_string())
}

fn write_png(image: &DynamicImage, path: &Path, icc: Option<&[u8]>) -> Result<(), String> {
    let file = fs::File::create(path).map_err(|err| err.to_string())?;
    let mut encoder = image::codecs::png::PngEncoder::new(BufWriter::new(file));
    if let Some(profile) = icc {
        encoder
            .set_icc_profile(profile.to_vec())
            .map_err(|err| err.to_string())?;
    }
    image
        .write_with_encoder(encoder)
        .map_err(|err| err.to_string())
}

fn flatten_rgb(image: &DynamicImage) -> image::RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let pixel = rgba.get_pixel(x, y);
        let alpha = pixel[3] as u32;
        let blend = |channel: u8| ((channel as u32 * alpha + 255 * (255 - alpha)) / 255) as u8;
        image::Rgb([blend(pixel[0]), blend(pixel[1]), blend(pixel[2])])
    })
}

pub fn output_name(src: &Path, encode: Encode) -> String {
    let stem = src
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("image");
    let source_ext = src.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    format!("{stem}.{}", encode.extension(source_ext))
}

pub fn effective_encode(
    choice: crate::model::RowChoice,
    recommendation: Option<&Recommendation>,
    format: SourceFormat,
    has_alpha: bool,
    webp_lossless: bool,
    allow_webp: bool,
) -> Result<Option<Encode>, String> {
    use crate::model::RowChoice;
    let encode = match choice {
        RowChoice::Skip => return Ok(None),
        RowChoice::Jpeg => Encode::Jpeg,
        RowChoice::Png => Encode::Png,
        RowChoice::Webp => Encode::Webp(model::webp_chip_mode(has_alpha, webp_lossless)),
        RowChoice::Recommended => match recommendation.map(|rec| rec.decision) {
            Some(Decision::Keep) => return Ok(None),
            Some(Decision::Use(encode)) => encode,
            None => match format {
                SourceFormat::Jpeg => Encode::Jpeg,
                SourceFormat::Png => Encode::Png,
                SourceFormat::Webp => Encode::Webp(model::webp_chip_mode(has_alpha, webp_lossless)),
            },
        },
    };
    Ok(Some(if allow_webp {
        encode
    } else {
        without_webp(encode, format, has_alpha)
    }))
}

fn without_webp(encode: Encode, format: SourceFormat, has_alpha: bool) -> Encode {
    if !matches!(encode, Encode::Webp(_)) {
        return encode;
    }
    if has_alpha || format == SourceFormat::Png {
        Encode::Png
    } else {
        Encode::Jpeg
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    let left_name = left.file_name();
    let right_name = right.file_name();
    if left_name.is_none() || left_name != right_name {
        return false;
    }
    let left_parent = left.parent().and_then(|path| fs::canonicalize(path).ok());
    let right_parent = right.parent().and_then(|path| fs::canonicalize(path).ok());
    match (left_parent, right_parent) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn copy_file(from: &Path, to: &Path) -> Result<u64, String> {
    fs::copy(from, to).map_err(|err| format!("copy to {}: {err}", to.display()))?;
    file_len(to)
}

fn file_len(path: &Path) -> Result<u64, String> {
    fs::metadata(path)
        .map(|meta| meta.len())
        .map_err(|err| err.to_string())
}

/// jpegoptim, pngcrush, and cwebp are console programs. On Windows, spawning
/// one opens a console window unless this flag is set.
#[cfg_attr(not(windows), allow(dead_code))]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

fn run_tool(cancel: &CancelFlag, program: &Path, args: &[String]) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Err("Cancelled".to_string());
    }
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut cmd);
    if let Some(dir) = program.parent() {
        let mut paths = vec![dir.to_path_buf()];
        if let Ok(current) = std::env::var("LD_LIBRARY_PATH") {
            paths.push(PathBuf::from(current));
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            cmd.env("LD_LIBRARY_PATH", joined);
        }
    }
    let child = cmd.spawn().map_err(|err| {
        format!("failed to start {}: {err}", program.display())
    })?;
    let pid = child.id();
    if let Ok(mut pids) = cancel.pids.lock() {
        pids.push(pid);
    }
    let output = child.wait_with_output().map_err(|err| err.to_string())?;
    if let Ok(mut pids) = cancel.pids.lock() {
        pids.retain(|running| *running != pid);
    }
    if cancel.is_cancelled() {
        return Err("Cancelled".to_string());
    }
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = format!("{stderr}{stdout}");
    let detail = detail.trim();
    let clipped: String = detail.chars().take(400).collect();
    Err(format!(
        "{} exited {} {}",
        program.file_name().and_then(|n| n.to_str()).unwrap_or("tool"),
        output.status,
        clipped
    ))
}

fn kill_pid(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/F"]);
        hide_console(&mut cmd);
        let _ = cmd.status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_tools_do_not_get_their_own_window() {
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
    }

    #[test]
    fn output_name_keeps_jpeg_extension() {
        assert_eq!(
            output_name(Path::new("/tmp/Photo.JPEG"), Encode::Jpeg),
            "Photo.jpeg"
        );
        assert_eq!(
            output_name(Path::new("/tmp/Photo.png"), Encode::Jpeg),
            "Photo.jpg"
        );
        assert_eq!(
            output_name(Path::new("/tmp/Photo.jpg"), Encode::Webp(WebpMode::Lossy)),
            "Photo.webp"
        );
    }

    fn write_jpeg_fixture(path: &Path, quality: u8) {
        let image = image::RgbImage::from_fn(48, 48, |x, y| {
            image::Rgb([((x * 13 + y * 7) % 256) as u8, (x * 3) as u8, (y * 5) as u8])
        });
        let file = fs::File::create(path).unwrap();
        let encoder = JpegEncoder::new_with_quality(BufWriter::new(file), quality);
        encoder
            .write_image(image.as_raw(), 48, 48, ExtendedColorType::Rgb8)
            .unwrap();
    }

    fn write_png_fixture(path: &Path) {
        let image = image::RgbaImage::from_fn(32, 32, |x, y| {
            image::Rgba([x as u8, y as u8, 40, if x < 4 { 0 } else { 255 }])
        });
        image.save_with_format(path, image::ImageFormat::Png).unwrap();
    }

    fn tools() -> Option<ToolPaths> {
        ToolPaths::from_dir(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin")).ok()
    }

    fn jpeg_sampling(path: &Path) -> Vec<(u8, u8)> {
        let bytes = fs::read(path).unwrap();
        let mut i = 0;
        while i + 4 < bytes.len() {
            if bytes[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            // SOI, EOI, and restart markers have no length field.
            if marker == 0x00
                || marker == 0x01
                || marker == 0xD8
                || marker == 0xD9
                || (0xD0..=0xD7).contains(&marker)
            {
                i += 2;
                continue;
            }
            if marker == 0xDA {
                break;
            }
            let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            if marker == 0xC0 || marker == 0xC2 {
                let count = bytes[i + 9] as usize;
                let mut samples = Vec::new();
                let mut at = i + 10;
                for _ in 0..count {
                    let factor = bytes[at + 1];
                    samples.push((factor >> 4, factor & 0x0F));
                    at += 3;
                }
                return samples;
            }
            i += 2 + len;
        }
        panic!("JPEG has no SOF marker");
    }

    #[test]
    fn lossy_jpeg_uses_420_sampling() {
        let Some(tools) = tools() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("io-420-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.jpg");
        write_jpeg_fixture(&src, 95);
        let info = analyze::analyze(&src).unwrap();
        let out = dir.join("optimized");
        let outcome = optimize_file(
            &tools,
            &Settings::default(),
            &src,
            &info,
            Encode::Jpeg,
            &out,
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(jpeg_sampling(&outcome.output_path), vec![(2, 2), (1, 1), (1, 1)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn jpeg_optimize_does_not_change_the_source() {
        let Some(tools) = tools() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("io-jpeg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.jpg");
        write_jpeg_fixture(&src, 95);
        let before = fs::read(&src).unwrap();
        let info = analyze::analyze(&src).unwrap();
        let out = dir.join("optimized");
        let outcome = optimize_file(
            &tools,
            &Settings::default(),
            &src,
            &info,
            Encode::Jpeg,
            &out,
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(fs::read(&src).unwrap(), before);
        assert!(outcome.output_path.is_file());
        assert!(
            outcome.commands.iter().any(|cmd| cmd.contains("--max=90")),
            "{:?}",
            outcome.commands
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn measure_webp_leaves_the_source_and_drops_the_temp_file() {
        let Some(tools) = tools() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("io-measure-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.jpg");
        write_jpeg_fixture(&src, 95);
        let before = fs::read(&src).unwrap();
        let info = analyze::analyze(&src).unwrap();
        let bytes = measure_webp(
            &tools,
            &Settings::default(),
            &src,
            &info,
            WebpMode::Lossy,
            &CancelFlag::new(),
        )
        .unwrap();
        assert!(bytes > 0);
        assert_eq!(fs::read(&src).unwrap(), before);
        assert!(dir.read_dir().unwrap().all(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_none_or(|ext| ext != "webp")
        }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recommend_jpeg_runs_jpegoptim_and_cwebp() {
        let Some(tools) = tools() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("io-rec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.jpg");
        write_jpeg_fixture(&src, 95);
        let before = fs::read(&src).unwrap();
        let info = analyze::analyze(&src).unwrap();
        let rec = recommend_file(
            &tools,
            &Settings::default(),
            &src,
            &info,
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(fs::read(&src).unwrap(), before);
        assert!(rec.sizes.iter().any(|(encode, _)| *encode == Encode::Jpeg));
        assert!(rec
            .sizes
            .iter()
            .any(|(encode, _)| matches!(encode, Encode::Webp(_))));
        assert!(rec.photo);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn png_optimize_does_not_change_the_source() {
        let Some(tools) = tools() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("io-png-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("mark.png");
        write_png_fixture(&src);
        let before = fs::read(&src).unwrap();
        let info = analyze::analyze(&src).unwrap();
        assert!(info.has_alpha);
        let out = dir.join("optimized");
        let outcome = optimize_file(
            &tools,
            &Settings::default(),
            &src,
            &info,
            Encode::Png,
            &out,
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(fs::read(&src).unwrap(), before);
        assert!(outcome.output_path.is_file());
        assert!(outcome.commands.iter().any(|cmd| cmd.contains("-reduce")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_to_overwrite_the_original() {
        let Some(tools) = tools() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("io-guard-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("photo.jpg");
        write_jpeg_fixture(&src, 90);
        let info = analyze::analyze(&src).unwrap();
        let err = optimize_file(
            &tools,
            &Settings::default(),
            &src,
            &info,
            Encode::Jpeg,
            &dir,
            &CancelFlag::new(),
        )
        .unwrap_err();
        assert!(err.contains("overwrite"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    fn jpeg_frame(path: &Path) -> (bool, Vec<(u8, u8)>) {
        let bytes = fs::read(path).unwrap();
        let mut i = 0;
        while i + 4 < bytes.len() {
            if bytes[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            if marker == 0x00
                || marker == 0x01
                || marker == 0xD8
                || marker == 0xD9
                || (0xD0..=0xD7).contains(&marker)
            {
                i += 2;
                continue;
            }
            if marker == 0xDA {
                break;
            }
            let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            if marker == 0xC0 || marker == 0xC2 {
                let count = bytes[i + 9] as usize;
                let mut samples = Vec::new();
                let mut at = i + 10;
                for _ in 0..count {
                    let factor = bytes[at + 1];
                    samples.push((factor >> 4, factor & 0x0F));
                    at += 3;
                }
                return (marker == 0xC2, samples);
            }
            i += 2 + len;
        }
        panic!("JPEG has no SOF marker");
    }

    fn icc_profile(path: &Path) -> Option<Vec<u8>> {
        let mut decoder = image::ImageReader::open(path)
            .unwrap()
            .into_decoder()
            .unwrap();
        decoder.icc_profile().ok().flatten()
    }

    fn orientation_exif() -> Vec<u8> {
        // Little-endian TIFF, one Orientation tag, value 6 (rotate 90 CW).
        vec![
            0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, 0x01, 0x00, 0x12, 0x01, 0x03, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]
    }

    fn insert_com(jpeg: &mut Vec<u8>, text: &str) {
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
        let payload = text.as_bytes();
        let mut com = vec![0xFF, 0xFE];
        com.extend_from_slice(&(payload.len() as u16 + 2).to_be_bytes());
        com.extend_from_slice(payload);
        jpeg.splice(2..2, com);
    }

    fn write_marked_jpeg(
        path: &Path,
        width: u32,
        height: u32,
        quality: u8,
        icc: &[u8],
        exif: &[u8],
    ) {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            if width != height && x < width / 2 {
                image::Rgb([255, 0, 0])
            } else if width != height {
                image::Rgb([0, 0, 255])
            } else {
                image::Rgb([((x * 3) % 256) as u8, ((y * 5) % 256) as u8, 40])
            }
        });
        let file = fs::File::create(path).unwrap();
        let mut encoder = JpegEncoder::new_with_quality(BufWriter::new(file), quality);
        encoder.set_icc_profile(icc.to_vec()).unwrap();
        if !exif.is_empty() {
            encoder.set_exif_metadata(exif.to_vec()).unwrap();
        }
        encoder
            .write_image(image.as_raw(), width, height, ExtendedColorType::Rgb8)
            .unwrap();
        if exif.is_empty() {
            let mut bytes = fs::read(path).unwrap();
            insert_com(&mut bytes, "GPSLatitude secret");
            fs::write(path, bytes).unwrap();
        }
    }

    #[test]
    fn email_effective_encode_never_returns_webp() {
        use crate::model::RowChoice;
        let webp = Encode::Webp(WebpMode::Lossy);
        let rec = Recommendation {
            decision: Decision::Use(webp),
            sizes: vec![(Encode::Jpeg, 80), (webp, 40)],
            photo: true,
        };
        let encode = effective_encode(
            RowChoice::Recommended,
            Some(&rec),
            SourceFormat::Jpeg,
            false,
            false,
            false,
        )
        .unwrap();
        assert_eq!(encode, Some(Encode::Jpeg));
        let alpha =
            effective_encode(RowChoice::Webp, None, SourceFormat::Png, true, false, false).unwrap();
        assert_eq!(alpha, Some(Encode::Png));
        let fallback = effective_encode(
            RowChoice::Recommended,
            None,
            SourceFormat::Webp,
            true,
            false,
            false,
        )
        .unwrap();
        assert_eq!(fallback, Some(Encode::Png));
    }

    #[test]
    fn email_photo_is_a_progressive_jpeg_at_1200_and_does_not_upscale() {
        let Some(tools) = tools() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("io-email-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let settings = crate::settings::Preset::Email.baseline();

        let large = dir.join("wide.jpg");
        let wide = image::RgbImage::from_fn(1800, 900, |x, y| {
            image::Rgb([((x / 7) % 256) as u8, ((y / 3) % 256) as u8, 80])
        });
        let file = fs::File::create(&large).unwrap();
        let encoder = JpegEncoder::new_with_quality(BufWriter::new(file), 95);
        encoder
            .write_image(wide.as_raw(), 1800, 900, ExtendedColorType::Rgb8)
            .unwrap();
        let info = analyze::analyze(&large).unwrap();
        let out = dir.join("out-large");
        let outcome = optimize_file(
            &tools,
            &settings,
            &large,
            &info,
            Encode::Jpeg,
            &out,
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(
            outcome.output_path.extension().and_then(|ext| ext.to_str()),
            Some("jpg")
        );
        assert_eq!(
            image::image_dimensions(&outcome.output_path).unwrap(),
            (1200, 600)
        );
        assert!(
            outcome.note.contains("resized to 1200x600"),
            "{}",
            outcome.note
        );
        let (progressive, samples) = jpeg_frame(&outcome.output_path);
        assert!(progressive);
        assert_eq!(samples, vec![(2, 2), (1, 1), (1, 1)]);
        let command = outcome.commands.join(" ");
        assert!(command.contains("--max=85"), "{command}");
        assert!(command.contains("--all-progressive"), "{command}");
        assert!(command.contains("--strip-exif"), "{command}");
        assert!(command.contains("--keep-icc"), "{command}");
        assert!(!command.contains("cwebp"), "{command}");
        assert!(!command.contains("--strip-all"), "{command}");

        let small = dir.join("small.jpg");
        let tiny = image::RgbImage::from_fn(500, 400, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 20])
        });
        let file = fs::File::create(&small).unwrap();
        let encoder = JpegEncoder::new_with_quality(BufWriter::new(file), 95);
        encoder
            .write_image(tiny.as_raw(), 500, 400, ExtendedColorType::Rgb8)
            .unwrap();
        let info = analyze::analyze(&small).unwrap();
        let out = dir.join("out-small");
        let outcome = optimize_file(
            &tools,
            &settings,
            &small,
            &info,
            Encode::Jpeg,
            &out,
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(
            image::image_dimensions(&outcome.output_path).unwrap(),
            (500, 400)
        );
        assert!(!outcome.note.contains("resized"), "{}", outcome.note);
        assert!(jpeg_frame(&outcome.output_path).0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn email_keeps_the_color_profile_strips_location_and_turns_the_photo_upright() {
        let Some(tools) = tools() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("io-email-icc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let icc = b"EMAIL-ICC-PROFILE-KEEP-0123456789".to_vec();
        let src = dir.join("sideways.jpg");
        write_marked_jpeg(&src, 32, 16, 95, &icc, &orientation_exif());
        let mut bytes = fs::read(&src).unwrap();
        insert_com(&mut bytes, "GPSLatitude secret");
        fs::write(&src, &bytes).unwrap();
        assert!(fs::read(&src)
            .unwrap()
            .windows(11)
            .any(|w| w == b"GPSLatitude"));
        let info = analyze::analyze(&src).unwrap();
        assert_eq!((info.width, info.height), (32, 16));
        let out = dir.join("out");
        let outcome = optimize_file(
            &tools,
            &crate::settings::Preset::Email.baseline(),
            &src,
            &info,
            Encode::Jpeg,
            &out,
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(
            image::image_dimensions(&outcome.output_path).unwrap(),
            (16, 32)
        );
        let turned = image::open(&outcome.output_path).unwrap().to_rgb8();
        let pixel = turned.get_pixel(8, 4).0;
        assert!(pixel[0] > 200 && pixel[2] < 80, "{pixel:?}");
        assert_eq!(
            icc_profile(&outcome.output_path).as_deref(),
            Some(icc.as_slice())
        );
        let output = fs::read(&outcome.output_path).unwrap();
        assert!(!output.windows(11).any(|w| w == b"GPSLatitude"));
        let mut decoder = image::ImageReader::open(&outcome.output_path)
            .unwrap()
            .into_decoder()
            .unwrap();
        assert_eq!(
            decoder.orientation().unwrap(),
            image::metadata::Orientation::NoTransforms
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_keeps_icc_when_jpegoptim_sees_the_original() {
        let Some(tools) = tools() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("io-strip-icc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let icc = b"EMAIL-ICC-PROFILE-KEEP-0123456789".to_vec();
        let src = dir.join("photo.jpg");
        write_marked_jpeg(&src, 48, 48, 95, &icc, &[]);
        assert!(fs::read(&src)
            .unwrap()
            .windows(11)
            .any(|w| w == b"GPSLatitude"));
        let settings = Settings {
            strip_metadata: true,
            ..Settings::default()
        };
        let info = analyze::analyze(&src).unwrap();
        let out = dir.join("out");
        let outcome = optimize_file(
            &tools,
            &settings,
            &src,
            &info,
            Encode::Jpeg,
            &out,
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(
            icc_profile(&outcome.output_path).as_deref(),
            Some(icc.as_slice())
        );
        assert!(!fs::read(&outcome.output_path)
            .unwrap()
            .windows(11)
            .any(|w| w == b"GPSLatitude"));
        let command = outcome.commands.join(" ");
        assert!(command.contains("--keep-icc"), "{command}");
        assert!(!command.contains("--strip-icc"), "{command}");
        let _ = fs::remove_dir_all(&dir);
    }

    fn png_crc(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    fn insert_png_text(png: &mut Vec<u8>, keyword: &str, text: &str) {
        let iend = png.windows(4).position(|window| window == b"IEND").unwrap();
        let mut data = Vec::new();
        data.extend(keyword.as_bytes());
        data.push(0);
        data.extend(text.as_bytes());
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
        chunk.extend(b"tEXt");
        chunk.extend(&data);
        let crc = png_crc(&chunk[4..]);
        chunk.extend_from_slice(&crc.to_be_bytes());
        png.splice(iend - 4..iend - 4, chunk);
    }

    #[test]
    fn email_transparency_stays_png_and_never_considers_webp() {
        let Some(tools) = tools() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("io-email-png-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let icc = fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/fixtures/srgb.icc"))
            .unwrap();
        let src = dir.join("badge.png");
        // Large flat areas: lossless WebP beats pngcrush by much more than 8%.
        let image = image::RgbaImage::from_fn(900, 600, |x, y| {
            let clear = x < 40;
            let block = (200..500).contains(&x) && (80..400).contains(&y);
            let rgb = if block { [240, 80, 40] } else { [30, 90, 200] };
            image::Rgba([rgb[0], rgb[1], rgb[2], if clear { 0 } else { 255 }])
        });
        let file = fs::File::create(&src).unwrap();
        let mut encoder = image::codecs::png::PngEncoder::new(BufWriter::new(file));
        encoder.set_icc_profile(icc.clone()).unwrap();
        image::DynamicImage::ImageRgba8(image)
            .write_with_encoder(encoder)
            .unwrap();
        let mut bytes = fs::read(&src).unwrap();
        insert_png_text(&mut bytes, "Comment", "GPSLatitude secret");
        fs::write(&src, &bytes).unwrap();
        let info = analyze::analyze(&src).unwrap();
        assert!(info.has_alpha);

        let email = crate::settings::Preset::Email.baseline();
        let plain = Settings::default();
        let email_rec = recommend_file(&tools, &email, &src, &info, &CancelFlag::new()).unwrap();
        let plain_rec = recommend_file(&tools, &plain, &src, &info, &CancelFlag::new()).unwrap();
        assert!(plain_rec
            .sizes
            .iter()
            .any(|(encode, _)| matches!(encode, Encode::Webp(_))));
        assert!(email_rec
            .sizes
            .iter()
            .all(|(encode, _)| !matches!(encode, Encode::Webp(_))));
        assert_eq!(
            plain_rec.decision,
            Decision::Use(Encode::Webp(WebpMode::Lossless)),
            "transparency fixture should favor WebP: {:?}",
            plain_rec.sizes
        );
        assert!(!matches!(
            email_rec.decision,
            Decision::Use(Encode::Webp(_))
        ));
        assert!(matches!(
            email_rec.decision,
            Decision::Use(Encode::Png) | Decision::Keep
        ));

        let photo = dir.join("noise.jpg");
        let noise = image::RgbImage::from_fn(96, 96, |x, y| {
            image::Rgb([((x * 17 + y * 9) % 256) as u8, (x * 3) as u8, (y * 5) as u8])
        });
        let file = fs::File::create(&photo).unwrap();
        let encoder = JpegEncoder::new_with_quality(BufWriter::new(file), 95);
        encoder
            .write_image(noise.as_raw(), 96, 96, ExtendedColorType::Rgb8)
            .unwrap();
        let photo_info = analyze::analyze(&photo).unwrap();
        let email_photo =
            recommend_file(&tools, &email, &photo, &photo_info, &CancelFlag::new()).unwrap();
        let plain_photo =
            recommend_file(&tools, &plain, &photo, &photo_info, &CancelFlag::new()).unwrap();
        assert!(plain_photo
            .sizes
            .iter()
            .any(|(encode, _)| matches!(encode, Encode::Webp(_))));
        assert!(email_photo
            .sizes
            .iter()
            .all(|(encode, _)| !matches!(encode, Encode::Webp(_))));
        assert!(matches!(
            email_photo.decision,
            Decision::Keep | Decision::Use(Encode::Jpeg)
        ));
        let webp_bytes = plain_photo
            .sizes
            .iter()
            .find_map(|(encode, bytes)| matches!(encode, Encode::Webp(_)).then_some(*bytes))
            .expect("default compare includes WebP");
        let jpeg_bytes = plain_photo
            .sizes
            .iter()
            .find_map(|(encode, bytes)| (*encode == Encode::Jpeg).then_some(*bytes))
            .expect("default compare includes JPEG");
        assert!(
            webp_bytes * 100 + jpeg_bytes * 8 <= jpeg_bytes * 100,
            "fixture should favor WebP: webp {webp_bytes} jpeg {jpeg_bytes}"
        );
        assert_eq!(
            plain_photo.decision,
            Decision::Use(Encode::Webp(WebpMode::Lossy))
        );
        assert!(matches!(
            email_photo.decision,
            Decision::Keep | Decision::Use(Encode::Jpeg)
        ));

        let out = dir.join("png-out");
        let outcome = optimize_file(
            &tools,
            &email,
            &src,
            &info,
            Encode::Png,
            &out,
            &CancelFlag::new(),
        )
        .unwrap();
        assert_eq!(
            outcome.output_path.extension().and_then(|ext| ext.to_str()),
            Some("png")
        );
        assert!(!outcome
            .commands
            .iter()
            .any(|cmd| cmd.contains("-brute") || cmd.contains("cwebp")));
        assert!(outcome.commands.iter().any(|cmd| cmd.contains("-reduce")));
        let output = fs::read(&outcome.output_path).unwrap();
        assert!(!output.windows(11).any(|window| window == b"GPSLatitude"));
        assert_eq!(
            icc_profile(&outcome.output_path).as_deref(),
            Some(icc.as_slice())
        );
        let decoded = image::open(&outcome.output_path).unwrap().to_rgba8();
        assert!(decoded.pixels().any(|pixel| pixel[3] == 0));
        assert!(std::fs::read_dir(&out).unwrap().all(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_none_or(|ext| ext != "webp")
        }));
        let _ = fs::remove_dir_all(&dir);
    }
}
