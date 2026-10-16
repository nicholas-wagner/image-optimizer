#!/bin/sh
# Builds jpegoptim and pngcrush, and downloads the official static cwebp, into bin/.
# Needs: curl, tar, gcc, make, cmake, zlib headers (zlib.h and libz.a).
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
third="$root/third_party"
bin="$root/bin"
mkdir -p "$third" "$bin"

fetch() {
  url=$1
  out=$2
  if [ ! -f "$out" ]; then
    curl -fL --retry 3 -o "$out" "$url"
  fi
}

fetch "https://storage.googleapis.com/downloads.webmproject.org/releases/webp/libwebp-1.6.0-linux-x86-64.tar.gz" \
  "$third/libwebp-1.6.0-linux-x86-64.tar.gz"
if [ ! -x "$third/libwebp-1.6.0-linux-x86-64/bin/cwebp" ]; then
  tar -xzf "$third/libwebp-1.6.0-linux-x86-64.tar.gz" -C "$third"
fi
cp -a "$third/libwebp-1.6.0-linux-x86-64/bin/cwebp" "$bin/cwebp"

fetch "https://github.com/libjpeg-turbo/libjpeg-turbo/releases/download/3.0.4/libjpeg-turbo-3.0.4.tar.gz" \
  "$third/libjpeg-turbo-3.0.4.tar.gz"
if [ ! -d "$third/libjpeg-turbo-3.0.4" ]; then
  tar -xzf "$third/libjpeg-turbo-3.0.4.tar.gz" -C "$third"
fi
if [ ! -f "$third/jpeg-install/lib/libjpeg.a" ]; then
  cmake -S "$third/libjpeg-turbo-3.0.4" -B "$third/libjpeg-turbo-build" \
    -DENABLE_SHARED=OFF -DENABLE_STATIC=ON -DWITH_SIMD=0 \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$third/jpeg-install"
  cmake --build "$third/libjpeg-turbo-build" -j"$(nproc)"
  cmake --install "$third/libjpeg-turbo-build"
fi

fetch "https://github.com/tjko/jpegoptim/archive/refs/tags/v1.5.6.tar.gz" \
  "$third/jpegoptim-1.5.6.tar.gz"
if [ ! -d "$third/jpegoptim-1.5.6" ]; then
  tar -xzf "$third/jpegoptim-1.5.6.tar.gz" -C "$third"
fi
if [ ! -x "$third/jpegoptim-1.5.6/jpegoptim" ]; then
  (
    cd "$third/jpegoptim-1.5.6"
    ./configure --with-libjpeg="$third/jpeg-install" --with-arith \
      CPPFLAGS="-I$third/jpeg-install/include" \
      LDFLAGS="-L$third/jpeg-install/lib"
    make -j"$(nproc)"
  )
fi
cp -a "$third/jpegoptim-1.5.6/jpegoptim" "$bin/jpegoptim"

fetch "https://github.com/pnggroup/libpng/archive/refs/tags/v1.6.44.tar.gz" \
  "$third/libpng-1.6.44.tar.gz"
if [ ! -d "$third/libpng-1.6.44" ]; then
  tar -xzf "$third/libpng-1.6.44.tar.gz" -C "$third"
fi
if [ ! -f "$third/png-install/lib/libpng.a" ]; then
  cmake -S "$third/libpng-1.6.44" -B "$third/libpng-build" \
    -DPNG_SHARED=OFF -DPNG_STATIC=ON -DPNG_TESTS=OFF \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$third/png-install" \
    -DZLIB_LIBRARY=/usr/lib/x86_64-linux-gnu/libz.a \
    -DZLIB_INCLUDE_DIR=/usr/include
  cmake --build "$third/libpng-build" -j"$(nproc)"
  cmake --install "$third/libpng-build"
fi

fetch "http://deb.debian.org/debian/pool/main/p/pngcrush/pngcrush_1.8.13.orig.tar.xz" \
  "$third/pngcrush-1.8.13.orig.tar.xz"
if [ ! -d "$third/pngcrush-src/pngcrush-1.8.13-nolib" ]; then
  mkdir -p "$third/pngcrush-src"
  tar -xJf "$third/pngcrush-1.8.13.orig.tar.xz" -C "$third/pngcrush-src"
fi
src="$third/pngcrush-src/pngcrush-1.8.13-nolib/pngcrush.c"
if ! grep -q "Removed from libpng 1.6.32" "$src"; then
  python3 - "$src" <<'PY'
import pathlib, sys
path = pathlib.Path(sys.argv[1])
text = path.read_text()
old = """                    png_set_option(read_ptr, PNG_IGNORE_ADLER32,
                        PNG_OPTION_ON);
"""
new = """#ifdef PNG_IGNORE_ADLER32
                    /* Removed from libpng 1.6.32+. Skip when the option is absent. */
                    png_set_option(read_ptr, PNG_IGNORE_ADLER32,
                        PNG_OPTION_ON);
#endif
"""
if old not in text:
    raise SystemExit("pngcrush.c patch context not found")
path.write_text(text.replace(old, new, 1))
PY
fi
(
  cd "$third/pngcrush-src/pngcrush-1.8.13-nolib"
  make \
    PNGINC="$third/png-install/include" \
    PNGLIB="$third/png-install/lib" \
    ZINC=/usr/include \
    ZLIB=/usr/lib/x86_64-linux-gnu \
    CFLAGS="-O3 -Wall" \
    LIBS="$third/png-install/lib/libpng.a /usr/lib/x86_64-linux-gnu/libz.a -lm"
)
cp -a "$third/pngcrush-src/pngcrush-1.8.13-nolib/pngcrush" "$bin/pngcrush"
mkdir -p "$root/lib"
fetch_deb() {
  pkg=$1
  so=$2
  dest=$3
  if [ -f "$dest" ]; then
    return
  fi
  tmp=$(mktemp -d)
  (
    cd "$tmp"
    apt-get download "$pkg"
    dpkg-deb -x ./*.deb "$tmp/root"
  )
  found=$(find "$tmp/root" -name "$so" -type f | head -1)
  cp -a "$found" "$dest"
  rm -rf "$tmp"
}
fetch_deb libxkbcommon-x11-0 "libxkbcommon-x11.so.0.0.0" "$root/lib/libxkbcommon-x11.so.0"
fetch_deb libxcb-xkb1 "libxcb-xkb.so.1.0.0" "$root/lib/libxcb-xkb.so.1"

chmod +x "$bin/jpegoptim" "$bin/pngcrush" "$bin/cwebp"
echo "tools in $bin"
ls -lh "$bin/jpegoptim" "$bin/pngcrush" "$bin/cwebp"
