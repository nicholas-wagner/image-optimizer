//! On Linux, winit loads `libxkbcommon-x11.so.0` at startup. A minimal machine
//! may not have that package. The two libraries are packed into the app and
//! this process re-execs itself once with them on `LD_LIBRARY_PATH`.
//!
//! Machines that already have the libraries keep using them. The packed copies
//! were built against a different libxkbcommon, and putting them first on the
//! library path crashes inside `xkb_state_update_mask`.

#[cfg(target_os = "linux")]
use crate::tools::{self, install_tool};

#[cfg(target_os = "linux")]
const X11_XKB: &str = "libxkbcommon-x11.so.0";
#[cfg(target_os = "linux")]
const XCB_XKB: &str = "libxcb-xkb.so.1";

#[cfg(target_os = "linux")]
pub fn ensure_runtime() {
    if std::env::var_os("IMAGE_OPTIMIZER_READY").is_some() {
        return;
    }
    let need_x11 = !library_loads(X11_XKB);
    let need_xcb = !library_loads(XCB_XKB);
    if !need_x11 && !need_xcb {
        return;
    }
    let Ok(bin_dir) = tools::cache_bin_dir() else {
        return;
    };
    let Some(cache) = bin_dir.parent() else {
        return;
    };
    let lib_dir = cache.join("lib");
    if need_x11
        && install_tool(
            &lib_dir,
            X11_XKB,
            include_bytes!("../lib/libxkbcommon-x11.so.0"),
        )
        .is_err()
    {
        return;
    }
    if need_xcb
        && install_tool(&lib_dir, XCB_XKB, include_bytes!("../lib/libxcb-xkb.so.1")).is_err()
    {
        return;
    }
    // An older run may have unpacked both. Leave the system copy in charge
    // when this machine already provides that library.
    if !need_x11 {
        let _ = std::fs::remove_file(lib_dir.join(X11_XKB));
    }
    if !need_xcb {
        let _ = std::fs::remove_file(lib_dir.join(XCB_XKB));
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

#[cfg(target_os = "linux")]
fn library_loads(name: &str) -> bool {
    let Ok(name) = std::ffi::CString::new(name) else {
        return false;
    };
    unsafe {
        let handle = dlopen(name.as_ptr(), RTLD_NOW);
        if handle.is_null() {
            false
        } else {
            dlclose(handle);
            true
        }
    }
}

#[cfg(target_os = "linux")]
const RTLD_NOW: i32 = 2;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn dlopen(filename: *const std::ffi::c_char, flags: i32) -> *mut std::ffi::c_void;
    fn dlclose(handle: *mut std::ffi::c_void) -> i32;
}

#[cfg(not(target_os = "linux"))]
pub fn ensure_runtime() {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::library_loads;

    #[test]
    fn libc_is_already_loaded() {
        assert!(library_loads("libc.so.6"));
    }

    #[test]
    fn a_made_up_library_is_left_for_the_packed_copy() {
        assert!(!library_loads("libimage-optimizer-missing.so.0"));
    }
}
