//! On Linux, winit loads `libxkbcommon-x11.so.0` at startup. A minimal machine
//! may not have that package. The two libraries are packed into the app and
//! this process re-execs itself once with them on `LD_LIBRARY_PATH`.

#[cfg(target_os = "linux")]
use crate::tools::{self, install_tool};

#[cfg(target_os = "linux")]
pub fn ensure_runtime() {
    if std::env::var_os("IMAGE_OPTIMIZER_READY").is_some() {
        return;
    }
    let Ok(bin_dir) = tools::cache_bin_dir() else {
        return;
    };
    let Some(cache) = bin_dir.parent() else {
        return;
    };
    let lib_dir = cache.join("lib");
    if install_tool(
        &lib_dir,
        "libxkbcommon-x11.so.0",
        include_bytes!("../lib/libxkbcommon-x11.so.0"),
    )
    .is_err()
    {
        return;
    }
    if install_tool(
        &lib_dir,
        "libxcb-xkb.so.1",
        include_bytes!("../lib/libxcb-xkb.so.1"),
    )
    .is_err()
    {
        return;
    }
    let mut ld = lib_dir.display().to_string();
    if let Ok(existing) = std::env::var("LD_LIBRARY_PATH") {
        if !existing.is_empty() {
            ld = format!("{ld}:{existing}");
        }
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.env("LD_LIBRARY_PATH", &ld)
        .env("IMAGE_OPTIMIZER_READY", "1")
        .args(std::env::args_os().skip(1));
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    eprintln!("could not restart with bundled libraries: {err}");
}

#[cfg(not(target_os = "linux"))]
pub fn ensure_runtime() {}
