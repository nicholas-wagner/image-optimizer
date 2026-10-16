# Image Optimizer

One file. Nothing to install. It optimizes JPEG, PNG, and WebP with the real `jpegoptim`, `pngcrush`, and `cwebp` tools, which are packed inside the app.

## Use it

Linux / WSL: run `./image-optimizer`. Click **Add images**, pick a folder, then click **Optimize**. Results land in an `optimized` folder next to the first image. Originals stay untouched. Quality and format choices stay under **Options**.

You can also type a path next to **Or path**, or drop files onto the window.

Windows: run `image-optimizer.exe`. It uses the normal Windows file dialog.

The first launch unpacks the three tools into `~/.cache/image-optimizer/bin/`, and two small X11 libraries into `~/.cache/image-optimizer/lib/`. You do not place those files yourself.

## Defaults

- JPEG max quality is 90 (`jpegoptim --max=90`). Move the slider to change it. Lossless JPEG turns the cap off.
- Files already saved at 90 or below are only optimized losslessly.
- WebP quality is 80. Lossless WebP and images with transparency use `cwebp -lossless -z 6 -exact`.
- PNG uses `pngcrush -reduce`. Maximum compression adds `-brute` and is slow.
- Strip metadata is off.
- Resize is off. Turn it on to shrink the long edge (1600 px) before encoding. That is the step that cuts multi‑megabyte photos down for the web.
- Recommend compares candidates and picks the smaller format. WebP has to be at least 8% smaller than JPEG (photos) or PNG (graphics), and the winner has to save at least 3%, or the original is kept.

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
