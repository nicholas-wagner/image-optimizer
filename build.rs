use std::path::Path;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(embed_tools)");
    println!("cargo::rustc-check-cfg=cfg(windows_tools)");
    let target = std::env::var("TARGET").unwrap_or_default();
    let windows = target.contains("windows");
    if windows {
        println!("cargo:rustc-cfg=windows_tools");
    }
    for name in tool_names(windows) {
        println!("cargo:rerun-if-changed=bin/{name}");
    }

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let bin = Path::new(&manifest).join("bin");
    let ready = tool_names(windows).iter().all(|name| bin.join(name).is_file());
    let profile = std::env::var("PROFILE").unwrap_or_default();

    if profile == "release" {
        if !ready {
            let names = tool_names(windows).join(", ");
            panic!("release build needs these files in bin/: {names}. Run scripts/build-tools.sh or scripts/build-windows.sh");
        }
        println!("cargo:rustc-cfg=embed_tools");
    }
}

fn tool_names(windows: bool) -> Vec<String> {
    ["jpegoptim", "pngcrush", "cwebp"]
        .into_iter()
        .map(|name| {
            if windows {
                format!("{name}.exe")
            } else {
                name.to_string()
            }
        })
        .collect()
}
