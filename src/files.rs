//! Finding images on disk.
//!
//! Folder walks remember directories they have already entered. A symlink
//! cycle (easy to hit under WSL and in home directories) cannot recurse
//! forever and overflow the stack.

use std::collections::HashSet;
use std::fs;
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
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("jpg" | "jpeg" | "png" | "webp")
    )
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
    path.parent().filter(|parent| *parent != path).map(Path::to_path_buf)
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
        if !is_dir && !is_image_ext(&path) {
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
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    walk(path, &mut out, &mut seen, 0, max);
    out
}

fn walk(path: &Path, out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, depth: u32, max: usize) {
    if out.len() >= max || depth > MAX_DEPTH {
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
            read_children(&target, out, seen, depth + 1, max);
        } else if is_image_ext(&target) {
            out.push(target);
        }
        return;
    }
    if meta.is_dir() {
        let key = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !seen.insert(key) {
            return;
        }
        read_children(path, out, seen, depth + 1, max);
        return;
    }
    if is_image_ext(path) {
        out.push(path.to_path_buf());
    }
}

fn read_children(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
    depth: u32,
    max: usize,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if out.len() >= max {
            return;
        }
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        walk(&entry.path(), out, seen, depth, max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "io-files-{name}-{}",
            std::process::id()
        ));
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
}
