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
use image::{DynamicImage, ExtendedColorType, ImageEncoder, ImageFormat};
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
        settings.webp_lossless,
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

    let image = image::open(src).map_err(|err| err.to_string())?;
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
            write_jpeg(&image, &path, 100)?;
            Ok(path)
        }
        Encode::Png | Encode::Webp(_) => {
            let path = scratch.path.join("input.png");
            image
                .save_with_format(&path, ImageFormat::Png)
                .map_err(|err| err.to_string())?;
            Ok(path)
        }
    }
}

fn write_jpeg(image: &DynamicImage, path: &Path, quality: u8) -> Result<(), String> {
    let rgb = flatten_rgb(image);
    let file = fs::File::create(path).map_err(|err| err.to_string())?;
    let encoder = JpegEncoder::new_with_quality(BufWriter::new(file), quality);
    encoder
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
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
) -> Result<Option<Encode>, String> {
    use crate::model::RowChoice;
    match choice {
        RowChoice::Skip => Ok(None),
        RowChoice::Jpeg => Ok(Some(Encode::Jpeg)),
        RowChoice::Png => Ok(Some(Encode::Png)),
        RowChoice::Webp => {
            let mode = if has_alpha || webp_lossless {
                WebpMode::Lossless
            } else {
                WebpMode::Lossy
            };
            Ok(Some(Encode::Webp(mode)))
        }
        RowChoice::Recommended => match recommendation.map(|rec| rec.decision) {
            Some(Decision::Keep) => Ok(None),
            Some(Decision::Use(encode)) => Ok(Some(encode)),
            None => Ok(Some(match format {
                SourceFormat::Jpeg => Encode::Jpeg,
                SourceFormat::Png => Encode::Png,
                SourceFormat::Webp => Encode::Webp(if has_alpha || webp_lossless {
                    WebpMode::Lossless
                } else {
                    WebpMode::Lossy
                }),
            })),
        },
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
        image.save_with_format(path, ImageFormat::Png).unwrap();
    }

    fn tools() -> Option<ToolPaths> {
        ToolPaths::from_dir(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin")).ok()
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
}
