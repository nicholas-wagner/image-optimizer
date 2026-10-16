use crate::model::WebpMode;
use crate::settings::Settings;
use std::path::Path;

pub fn jpegoptim_args(settings: &Settings, input: &Path, dest_dir: &Path) -> Vec<String> {
    let mut args = vec![
        format!("--dest={}", dest_dir.display()),
        "--overwrite".to_string(),
    ];
    if !settings.jpeg_lossless {
        args.push(format!("--max={}", settings.jpeg_quality.min(100)));
    }
    if settings.strip_metadata {
        args.push("--strip-all".to_string());
    }
    args.push(input.display().to_string());
    args
}

pub fn pngcrush_args(settings: &Settings, input: &Path, output: &Path) -> Vec<String> {
    let mut args = vec!["-q".to_string(), "-reduce".to_string()];
    if settings.strip_metadata {
        args.push("-rem".to_string());
        args.push("alla".to_string());
    }
    if settings.png_brute {
        args.push("-brute".to_string());
    }
    args.push(input.display().to_string());
    args.push(output.display().to_string());
    args
}

pub fn cwebp_args(settings: &Settings, mode: WebpMode, input: &Path, output: &Path) -> Vec<String> {
    let mut args = Vec::new();
    match mode {
        WebpMode::Lossless => {
            args.extend([
                "-lossless".to_string(),
                "-z".to_string(),
                "6".to_string(),
                "-exact".to_string(),
            ]);
        }
        WebpMode::Lossy => {
            // -preset overwrites other options except -q, so it comes first.
            args.extend([
                "-preset".to_string(),
                "photo".to_string(),
                "-q".to_string(),
                settings.webp_quality.min(100).to_string(),
            ]);
        }
    }
    args.push("-mt".to_string());
    if !settings.strip_metadata {
        args.push("-metadata".to_string());
        args.push("all".to_string());
    }
    args.push(input.display().to_string());
    args.push("-o".to_string());
    args.push(output.display().to_string());
    args
}

pub fn display_command(program: &Path, args: &[String]) -> String {
    let mut parts = vec![program.display().to_string()];
    for arg in args {
        if arg.chars().any(char::is_whitespace) {
            parts.push(format!("\"{arg}\""));
        } else {
            parts.push(arg.clone());
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn paths() -> (PathBuf, PathBuf) {
        (PathBuf::from("in.jpg"), PathBuf::from("out"))
    }

    #[test]
    fn jpeg_defaults_to_max_90() {
        let (input, dest) = paths();
        let args = jpegoptim_args(&Settings::default(), &input, &dest);
        assert!(args.iter().any(|arg| arg == "--max=90"));
        assert!(!args.iter().any(|arg| arg == "--strip-all"));
        assert!(args.iter().any(|arg| arg == "--overwrite"));
    }

    #[test]
    fn jpeg_quality_is_overridable() {
        let (input, dest) = paths();
        let mut settings = Settings::default();
        settings.jpeg_quality = 75;
        let args = jpegoptim_args(&settings, &input, &dest);
        assert!(args.iter().any(|arg| arg == "--max=75"));
    }

    #[test]
    fn jpeg_lossless_omits_max() {
        let (input, dest) = paths();
        let mut settings = Settings::default();
        settings.jpeg_lossless = true;
        let args = jpegoptim_args(&settings, &input, &dest);
        assert!(!args.iter().any(|arg| arg.starts_with("--max")));
    }

    #[test]
    fn strip_metadata_flags() {
        let (input, dest) = paths();
        let mut settings = Settings::default();
        settings.strip_metadata = true;
        let jpeg = jpegoptim_args(&settings, &input, &dest);
        assert!(jpeg.iter().any(|arg| arg == "--strip-all"));
        let png = pngcrush_args(&settings, &input, &dest);
        assert!(png.windows(2).any(|pair| pair == ["-rem", "alla"]));
        let webp = cwebp_args(&settings, WebpMode::Lossy, &input, &dest);
        assert!(!webp.iter().any(|arg| arg == "-metadata"));
    }

    #[test]
    fn keep_metadata_passes_webp_metadata_all() {
        let (input, dest) = paths();
        let args = cwebp_args(&Settings::default(), WebpMode::Lossy, &input, &dest);
        assert!(args.windows(2).any(|pair| pair == ["-metadata", "all"]));
        assert!(args.windows(2).any(|pair| pair == ["-q", "80"]));
    }

    #[test]
    fn png_brute_is_opt_in() {
        let (input, dest) = paths();
        let fast = pngcrush_args(&Settings::default(), &input, &dest);
        assert!(fast.iter().any(|arg| arg == "-reduce"));
        assert!(!fast.iter().any(|arg| arg == "-brute"));
        let mut settings = Settings::default();
        settings.png_brute = true;
        let slow = pngcrush_args(&settings, &input, &dest);
        assert!(slow.iter().any(|arg| arg == "-brute"));
    }

    #[test]
    fn lossless_webp_uses_z6_exact() {
        let (input, dest) = paths();
        let args = cwebp_args(&Settings::default(), WebpMode::Lossless, &input, &dest);
        assert!(args.iter().any(|arg| arg == "-lossless"));
        assert!(args.windows(2).any(|pair| pair == ["-z", "6"]));
        assert!(args.iter().any(|arg| arg == "-exact"));
    }
}
