# Image Optimizer

One file. Nothing to install. It optimizes JPEG, PNG, and WebP with the real `jpegoptim`, `pngcrush`, and `cwebp` tools, which are packed inside the app.

## Use it

Linux / WSL: run `./image-optimizer`. The window opens in **Simple**.

1. Pick a style. **Smaller file** keeps the full size. **For the web** shrinks the long side to 1600 px. **Perfect quality** does not reduce quality.
2. Click **Add images** at the bottom, or drop files onto the window.
3. Click **Optimize**.

Originals stay untouched. Results land in an `optimized` folder next to the first image. Click a file to compare before and after. The same button stays at the bottom while you scroll.

**Advanced**, in the header, opens the rest: quality sliders, resize, output folder, metadata, PNG maximum compression, per-file format (Best, JPEG, PNG, WebP, or Skip), add by path, and the command that ran. Check files to change their format together.

**Your settings** appears when a control no longer matches a style. Picking a style again resets those controls and keeps the output folder. Changing a style or a slider brings the button back to **Optimize**.

Windows: run `image-optimizer.exe`. It uses the normal Windows file dialog.

The first launch unpacks the three tools into `~/.cache/image-optimizer/bin/`. If `libxkbcommon-x11` or `libxcb-xkb` is missing, it also unpacks that library into `~/.cache/image-optimizer/lib/`. You do not place those files yourself.

## Defaults

- **Smaller file** (Compress) is the starting preset: full size, JPEG max quality 90, WebP quality 80, Best format. **For the web** (Web) is the same with resize on at 1600 px. **Perfect quality** (Lossless) turns on lossless JPEG and lossless WebP. Any other slider change is **Your settings** (Custom).
- JPEG max quality is 90 (`jpegoptim --max=90`). Move the slider to change it. Lossless JPEG turns the cap off.
- Files already saved at 90 or below are only optimized losslessly.
- WebP quality is 80. Lossless WebP and images with transparency use `cwebp -lossless -z 6 -exact`.
- PNG uses `pngcrush -reduce`. Maximum compression adds `-brute` and is slow.
- Strip metadata is off.
- Resize is off in **Compress**. **Web** turns it on at 1600 px. Size chips are 800, 1600, and 1920. That is what cuts multi‑megabyte photos down for the web.
- Recommend compares candidates and picks the smaller format. WebP has to be at least 8% smaller than JPEG (photos) or PNG (graphics), and the winner has to save at least 3%, or the original is kept.

## Releases

Pushing a tag such as `v0.1.0` builds both binaries and attaches them to a GitHub release.

- `image-optimizer-linux-x86_64`
- `image-optimizer-windows-x86_64.exe`

`chmod +x` the Linux file, then run it. On Windows, download the exe and double-click it.

## Build

Linux, about 2 minutes if the tools are already built:

1. `./scripts/build-tools.sh`
2. `cargo build --release`
3. Ship `target/release/image-optimizer`.

Windows, about 15 minutes, needs Docker:

1. `./scripts/build-windows.sh`
2. Ship `target/x86_64-pc-windows-gnu/release/image-optimizer.exe`.

## Licenses

The GUI is MIT. jpegoptim is GPL-3 and is run as a separate program. pngcrush, libpng, zlib, libjpeg-turbo, and libwebp have their own notices. Open Licenses in the app.
