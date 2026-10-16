//! Locates jpegoptim, pngcrush, and cwebp.
//!
//! Release builds embed the three binaries and extract them into the cache
//! directory. They call `exit()` and keep process-global state, so they have
//! to run as child processes. The OS can only exec a real file.

use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ToolPaths {
    pub jpegoptim: PathBuf,
    pub pngcrush: PathBuf,
    pub cwebp: PathBuf,
}

impl ToolPaths {
    pub fn from_dir(dir: &Path) -> Result<Self, String> {
        let tools = Self {
            jpegoptim: dir.join(exe_name("jpegoptim")),
            pngcrush: dir.join(exe_name("pngcrush")),
            cwebp: dir.join(exe_name("cwebp")),
        };
        for path in [&tools.jpegoptim, &tools.pngcrush, &tools.cwebp] {
            if !path.is_file() {
                return Err(format!("missing {}", path.display()));
            }
        }
        Ok(tools)
    }
}

pub fn locate() -> Result<ToolPaths, String> {
    if let Some(dir) = std::env::var_os("IMAGE_OPTIMIZER_BIN") {
        if let Ok(tools) = ToolPaths::from_dir(Path::new(&dir)) {
            return Ok(tools);
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin");
    if let Ok(tools) = ToolPaths::from_dir(&manifest) {
        if !cfg!(embed_tools) {
            return Ok(tools);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Ok(tools) = ToolPaths::from_dir(&dir.join("bin")) {
                if !cfg!(embed_tools) {
                    return Ok(tools);
                }
            }
        }
    }
    #[cfg(embed_tools)]
    {
        let cache = cache_bin_dir()?;
        install_embedded(&cache)?;
        return ToolPaths::from_dir(&cache);
    }
    #[cfg(not(embed_tools))]
    {
        Err("jpegoptim, pngcrush, and cwebp were not found in bin/. Run scripts/build-tools.sh".into())
    }
}

#[allow(dead_code)]
pub fn cache_bin_dir() -> Result<PathBuf, String> {
    let dir = if cfg!(windows) {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| std::env::temp_dir().display().to_string());
        PathBuf::from(base).join("image-optimizer").join("bin")
    } else if cfg!(target_os = "macos") {
        let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
        PathBuf::from(home).join("Library/Caches/image-optimizer/bin")
    } else {
        let base = match std::env::var("XDG_CACHE_HOME") {
            Ok(path) => PathBuf::from(path),
            Err(_) => {
                let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
                PathBuf::from(home).join(".cache")
            }
        };
        base.join("image-optimizer/bin")
    };
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    Ok(dir)
}

#[cfg(all(embed_tools, windows_tools))]
fn install_embedded(dir: &Path) -> Result<(), String> {
    install_tool(dir, &exe_name("jpegoptim"), include_bytes!("../bin/jpegoptim.exe"))?;
    install_tool(dir, &exe_name("pngcrush"), include_bytes!("../bin/pngcrush.exe"))?;
    install_tool(dir, &exe_name("cwebp"), include_bytes!("../bin/cwebp.exe"))?;
    Ok(())
}

#[cfg(all(embed_tools, not(windows_tools)))]
fn install_embedded(dir: &Path) -> Result<(), String> {
    install_tool(dir, &exe_name("jpegoptim"), include_bytes!("../bin/jpegoptim"))?;
    install_tool(dir, &exe_name("pngcrush"), include_bytes!("../bin/pngcrush"))?;
    install_tool(dir, &exe_name("cwebp"), include_bytes!("../bin/cwebp"))?;
    Ok(())
}

pub fn install_tool(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    let dest = dir.join(name);
    let digest = sha256(bytes);
    if dest.is_file() {
        if let Ok(existing) = fs::read(&dest) {
            if sha256(&existing) == digest {
                make_executable(&dest)?;
                return Ok(dest);
            }
        }
    }
    let tmp = dir.join(format!(".{name}.partial"));
    fs::write(&tmp, bytes).map_err(|err| err.to_string())?;
    make_executable(&tmp)?;
    fs::rename(&tmp, &dest).map_err(|err| err.to_string())?;
    Ok(dest)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn make_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).map_err(|err| err.to_string())?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).map_err(|err| err.to_string())?;
    }
    let _ = path;
    Ok(())
}

fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_writes_bytes_and_skips_same_content() {
        let dir = std::env::temp_dir().join(format!("io-tools-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = install_tool(&dir, "sample-tool", b"hello").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        let again = install_tool(&dir, "sample-tool", b"hello").unwrap();
        assert_eq!(again, path);
        install_tool(&dir, "sample-tool", b"changed").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"changed");
        let _ = fs::remove_dir_all(&dir);
    }
}
