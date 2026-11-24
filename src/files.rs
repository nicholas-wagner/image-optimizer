//! Finding images on disk.
//!
//! Folder walks remember directories they have already entered. A symlink
//! cycle (easy to hit under WSL and in home directories) cannot recurse
//! forever and overflow the stack.

use crate::analyze;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const MAX_IMAGES: usize = 2000;
const MAX_DEPTH: u32 = 32;

/// Linux draws its own file list. The XDG portal dialog panics on WSL
/// (`there is no reactor running, must be called from the context of a Tokio 1.x runtime`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilePrompt {
    InApp,
    Native,
}

pub fn file_prompt() -> FilePrompt {
    if cfg!(windows) {
        FilePrompt::Native
    } else {
        FilePrompt::InApp
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirItem {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

pub fn is_image_ext(path: &Path) -> bool {
    matches!(
        extension(path).as_deref(),
        Some("jpg" | "jpeg" | "png" | "webp" | "heic" | "heif")
    )
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Intake {
    pub images: Vec<PathBuf>,
    pub skipped: BTreeMap<String, usize>,
}

pub fn collect_intake(path: &Path) -> Intake {
    collect_intake_limited(path, MAX_IMAGES)
}

pub fn collect_intake_limited(path: &Path, max: usize) -> Intake {
    let mut intake = Intake::default();
    let mut seen = HashSet::new();
    walk(path, &mut intake, &mut seen, 0, max);
    intake
}

/// One line such as `Skipped 3 files: 2 GIF, 1 video.`
pub fn skipped_summary(skipped: &BTreeMap<String, usize>) -> Option<String> {
    let total: usize = skipped.values().copied().sum();
    if total == 0 {
        return None;
    }
    let mut rows: Vec<(&str, usize)> = skipped
        .iter()
        .map(|(label, count)| (label.as_str(), *count))
        .collect();
    rows.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    let parts: Vec<String> = rows
        .iter()
        .map(|(label, count)| format!("{count} {label}"))
        .collect();
    let noun = if total == 1 { "file" } else { "files" };
    Some(format!("Skipped {total} {noun}: {}.", parts.join(", ")))
}

/// Status line after an add. `found_images` is true when a supported file was seen,
/// including one that was already in the list.
pub fn intake_notice(
    added: usize,
    found_images: bool,
    skipped: &BTreeMap<String, usize>,
) -> String {
    let mut parts = Vec::new();
    if added > 0 {
        let noun = if added == 1 { "image" } else { "images" };
        parts.push(format!("Added {added} {noun}."));
    } else if found_images || skipped.is_empty() {
        parts.push("Those images are already in the list.".to_string());
    }
    if let Some(line) = skipped_summary(skipped) {
        parts.push(line);
    }
    parts.join(" ")
}

pub fn starting_dir() -> PathBuf {
    for var in ["USERPROFILE", "HOME"] {
        if let Ok(value) = std::env::var(var) {
            let path = PathBuf::from(value);
            if path.is_dir() {
                return path;
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn shortcuts() -> Vec<(&'static str, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let path = PathBuf::from(home);
        if path.is_dir() {
            out.push(("Home", path));
        }
    }
    let windows_c = PathBuf::from("/mnt/c");
    if windows_c.is_dir() {
        out.push(("Windows C:", windows_c));
    }
    out
}

pub fn parent_dir(path: &Path) -> Option<PathBuf> {
    path.parent()
        .filter(|parent| *parent != path)
        .map(Path::to_path_buf)
}

pub fn list_dir(dir: &Path) -> Result<Vec<DirItem>, String> {
    let entries = fs::read_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut items = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let is_dir = path.is_dir();
        if !is_dir && !is_supported_file(&path) {
            continue;
        }
        items.push(DirItem {
            path,
            name: name.into_owned(),
            is_dir,
        });
    }
    items.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(items)
}

pub fn collect_images(path: &Path) -> Vec<PathBuf> {
    collect_images_limited(path, MAX_IMAGES)
}

pub fn collect_images_limited(path: &Path, max: usize) -> Vec<PathBuf> {
    collect_intake_limited(path, max).images
}

fn walk(path: &Path, intake: &mut Intake, seen: &mut HashSet<PathBuf>, depth: u32, max: usize) {
    if intake.images.len() >= max || depth > MAX_DEPTH {
        return;
    }
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return,
    };
    if meta.file_type().is_symlink() {
        let Ok(target) = fs::canonicalize(path) else {
            return;
        };
        if !seen.insert(target.clone()) {
            return;
        }
        if target.is_dir() {
            read_children(&target, intake, seen, depth + 1, max);
        } else {
            consider_file(path, intake, max);
        }
        return;
    }
    if meta.is_dir() {
        let key = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !seen.insert(key) {
            return;
        }
        read_children(path, intake, seen, depth + 1, max);
        return;
    }
    consider_file(path, intake, max);
}

fn read_children(
    dir: &Path,
    intake: &mut Intake,
    seen: &mut HashSet<PathBuf>,
    depth: u32,
    max: usize,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if intake.images.len() >= max {
            return;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        walk(&entry.path(), intake, seen, depth, max);
    }
}

fn consider_file(path: &Path, intake: &mut Intake, max: usize) {
    if is_supported_file(path) {
        if intake.images.len() < max {
            intake.images.push(path.to_path_buf());
        }
        return;
    }
    let label = skip_label(path);
    *intake.skipped.entry(label).or_default() += 1;
}

fn is_supported_file(path: &Path) -> bool {
    is_image_ext(path) || sniff_header(path).is_some()
}

fn sniff_header(path: &Path) -> Option<crate::model::SourceFormat> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 64];
    let read = file.read(&mut header).ok()?;
    analyze::sniff(&header[..read])
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
}

fn skip_label(path: &Path) -> String {
    if let Some(kind) = signature_kind(path) {
        return kind;
    }
    match extension(path).as_deref() {
        Some("gif") => "GIF".to_string(),
        Some(ext) if is_video_ext(ext) => "video".to_string(),
        Some(ext) if is_text_ext(ext) => "text".to_string(),
        Some(ext) if ext.len() <= 4 && ext.chars().all(|ch| ch.is_ascii_alphanumeric()) => {
            ext.to_ascii_uppercase()
        }
        _ => "other".to_string(),
    }
}

fn signature_kind(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 16];
    let read = file.read(&mut header).ok()?;
    let header = &header[..read];
    if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        return Some("GIF".to_string());
    }
    None
}

fn is_video_ext(ext: &str) -> bool {
    matches!(
        ext,
        "mp4"
            | "m4v"
            | "mov"
            | "avi"
            | "mkv"
            | "webm"
            | "wmv"
            | "mpg"
            | "mpeg"
            | "3gp"
            | "3g2"
            | "ogv"
            | "ts"
            | "mts"
            | "qt"
            | "flv"
    )
}

fn is_text_ext(ext: &str) -> bool {
    matches!(ext, "txt" | "text" | "md" | "csv" | "rtf" | "log")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("io-files-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn linux_uses_the_in_app_picker() {
        if cfg!(windows) {
            assert_eq!(file_prompt(), FilePrompt::Native);
        } else {
            assert_eq!(file_prompt(), FilePrompt::InApp);
        }
    }

    #[test]
    fn list_dir_shows_folders_and_images_only() {
        let dir = scratch("list");
        fs::create_dir(dir.join("nested")).unwrap();
        fs::write(dir.join("photo.JPG"), b"x").unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap();
        fs::write(dir.join(".hidden.png"), b"x").unwrap();
        let items = list_dir(&dir).unwrap();
        let names: Vec<_> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec!["nested", "photo.JPG"]);
        assert!(items[0].is_dir);
        assert!(!items[1].is_dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_finds_nested_images_and_skips_other_files() {
        let dir = scratch("nest");
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("a.jpg"), b"x").unwrap();
        fs::write(dir.join("sub").join("b.png"), b"x").unwrap();
        fs::write(dir.join("sub").join("c.txt"), b"x").unwrap();
        let found = collect_images(&dir);
        let mut names: Vec<_> = found
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["a.jpg", "b.png"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_stops_at_the_limit() {
        let dir = scratch("limit");
        for index in 0..5 {
            fs::write(dir.join(format!("{index}.jpg")), b"x").unwrap();
        }
        assert_eq!(collect_images_limited(&dir, 2).len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn collect_survives_a_symlink_cycle() {
        use std::os::unix::fs::symlink;
        let dir = scratch("cycle");
        fs::write(dir.join("only.webp"), b"x").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        symlink(&dir, dir.join("sub").join("back")).unwrap();
        let found = collect_images(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file_name().unwrap(), "only.webp");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn heic_extension_and_signature_are_images() {
        assert!(is_image_ext(Path::new("IMG_0001.HEIC")));
        assert!(is_image_ext(Path::new("photo.heif")));
        assert!(!is_image_ext(Path::new("clip.mp4")));
        let dir = scratch("heic-sig");
        let mut header = vec![0u8; 24];
        header[0..4].copy_from_slice(&24u32.to_be_bytes());
        header[4..8].copy_from_slice(b"ftyp");
        header[8..12].copy_from_slice(b"heic");
        header[16..20].copy_from_slice(b"mif1");
        fs::write(dir.join("phone.bin"), &header).unwrap();
        fs::write(dir.join("shot.HEIC"), &header).unwrap();
        let found = collect_images(&dir);
        let mut names: Vec<_> = found
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["phone.bin", "shot.HEIC"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn skipped_files_are_counted_by_type() {
        let dir = scratch("skip");
        fs::write(dir.join("a.jpg"), b"x").unwrap();
        fs::write(dir.join("b.gif"), b"GIF89a").unwrap();
        fs::write(dir.join("c.GIF"), b"GIF87a").unwrap();
        fs::write(dir.join("clip.mp4"), b"not-really").unwrap();
        fs::write(dir.join(".hidden.gif"), b"GIF89a").unwrap();
        let intake = collect_intake(&dir);
        assert_eq!(intake.images.len(), 1);
        assert_eq!(
            skipped_summary(&intake.skipped).as_deref(),
            Some("Skipped 3 files: 2 GIF, 1 video.")
        );
        assert_eq!(
            intake_notice(1, true, &intake.skipped),
            "Added 1 image. Skipped 3 files: 2 GIF, 1 video."
        );
        assert_eq!(
            intake_notice(0, false, &intake.skipped),
            "Skipped 3 files: 2 GIF, 1 video."
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
