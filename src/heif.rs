//! Decode an iPhone HEIC/HEIF photo to a temporary PNG.
//!
//! The image crate cannot read HEIC. `heif-dec` (libheif + libde265) writes a
//! PNG with the pixels turned upright, the color profile kept, and the Exif
//! orientation tag reset so a later viewer does not rotate the photo again.
//! GPS stays in that PNG's Exif until a style that strips metadata removes it.
//! This app never asks heif-dec to write a HEIC file.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn decode_args(src: &Path, dest: &Path) -> Vec<String> {
    vec![
        "--quiet".to_string(),
        "--png-compression-level".to_string(),
        "1".to_string(),
        "-o".to_string(),
        dest.display().to_string(),
        src.display().to_string(),
    ]
}

/// Decode `src` to `dest`. Used when adding a file, before a job exists.
pub fn decode_to_png(program: &Path, src: &Path, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let args = decode_args(src, dest);
    let mut cmd = Command::new(program);
    cmd.args(&args)
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
    let output = cmd
        .output()
        .map_err(|err| format!("failed to start {}: {err}", program.display()))?;
    if output.status.success() && dest.is_file() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = format!("{stderr}{stdout}");
    let detail = detail.trim();
    let clipped: String = detail.chars().take(400).collect();
    Err(format!(
        "could not read this HEIC photo ({})",
        if clipped.is_empty() {
            output.status.to_string()
        } else {
            clipped
        }
    ))
}

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
