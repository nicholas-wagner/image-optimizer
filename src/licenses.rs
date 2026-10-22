//! Notices shown in the app. Release builds are one file, so these texts
//! travel inside that file.

pub const TEXT: &str = concat!(
    include_str!("../LICENSES/notices.txt"),
    "\n----- GNU General Public License, version 3 -----\n\n",
    include_str!("../LICENSES/GPL-3.0.txt"),
    "\n----- pngcrush 1.8.13 notice -----\n\n",
    include_str!("../LICENSES/pngcrush.txt"),
    "\n----- libwebp -----\n\n",
    include_str!("../LICENSES/libwebp-COPYING"),
    "\n",
    include_str!("../LICENSES/libwebp-PATENTS"),
    "\n----- libxkbcommon -----\n\n",
    include_str!("../LICENSES/libxkbcommon-copyright"),
    "\n----- libxcb -----\n\n",
    include_str!("../LICENSES/libxcb-COPYING"),
);

#[cfg(test)]
mod tests {
    use super::TEXT;

    #[test]
    fn bundled_tools_keep_their_notices() {
        for phrase in [
            "This software is based in part on the work of the Independent JPEG Group.",
            "version 3 or any later version",
            "Copyright (C) 1996-2022",
            "Image Optimizer change:",
            "GNU GENERAL PUBLIC LICENSE",
            "Copyright (c) 2010, Google Inc.",
            "libxkbcommon-x11.so.0",
            "libxcb-xkb.so.1",
            "Bart Massey, Jamey Sharp, and Josh Triplett.",
        ] {
            assert!(TEXT.contains(phrase), "missing {phrase}");
        }
    }
}
