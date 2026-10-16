#!/bin/bash
# Runs inside a Linux container that has the repo mounted at /src.
# Produces bin/*.exe and target/x86_64-pc-windows-gnu/release/image-optimizer.exe.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y gcc-mingw-w64-x86-64 cmake make curl unzip python3 xz-utils pkg-config

rustup target add x86_64-pc-windows-gnu

root=/src
third="$root/third_party"
prefix="$third/win-install"
mkdir -p "$third" "$prefix" "$root/bin"

fetch() {
  url=$1
  out=$2
  if [ ! -f "$out" ]; then
    curl -fL --retry 3 -o "$out" "$url"
  fi
}

CC=x86_64-w64-mingw32-gcc
cat > "$third/mingw-toolchain.cmake" <<'EOF'
set(CMAKE_SYSTEM_NAME Windows)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
set(CMAKE_C_COMPILER x86_64-w64-mingw32-gcc)
set(CMAKE_RC_COMPILER x86_64-w64-mingw32-windres)
set(CMAKE_FIND_ROOT_PATH /src/third_party/win-install)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
EOF

if [ ! -f "$prefix/lib/libz.a" ] && [ ! -f "$prefix/lib/libzlibstatic.a" ]; then
  fetch "https://github.com/madler/zlib/releases/download/v1.3.1/zlib-1.3.1.tar.gz" "$third/zlib-1.3.1.tar.gz"
  if [ ! -d "$third/zlib-1.3.1" ]; then
    tar -xzf "$third/zlib-1.3.1.tar.gz" -C "$third"
  fi
  cmake -S "$third/zlib-1.3.1" -B "$third/zlib-win" \
    -DCMAKE_TOOLCHAIN_FILE="$third/mingw-toolchain.cmake" \
    -DCMAKE_INSTALL_PREFIX="$prefix" \
    -DBUILD_SHARED_LIBS=OFF \
    -DCMAKE_BUILD_TYPE=Release
  cmake --build "$third/zlib-win" -j"$(nproc)"
  cmake --install "$third/zlib-win"
fi
if [ -f "$prefix/lib/libzlibstatic.a" ] && [ ! -f "$prefix/lib/libz.a" ]; then
  cp -a "$prefix/lib/libzlibstatic.a" "$prefix/lib/libz.a"
fi

if [ ! -f "$prefix/lib/libjpeg.a" ]; then
  rm -rf "$third/libjpeg-win"
  fetch "https://github.com/libjpeg-turbo/libjpeg-turbo/releases/download/3.0.4/libjpeg-turbo-3.0.4.tar.gz" \
    "$third/libjpeg-turbo-3.0.4.tar.gz"
  if [ ! -d "$third/libjpeg-turbo-3.0.4" ]; then
    tar -xzf "$third/libjpeg-turbo-3.0.4.tar.gz" -C "$third"
  fi
  cmake -S "$third/libjpeg-turbo-3.0.4" -B "$third/libjpeg-win" \
    -DCMAKE_TOOLCHAIN_FILE="$third/mingw-toolchain.cmake" \
    -DCMAKE_INSTALL_PREFIX="$prefix" \
    -DENABLE_SHARED=OFF -DENABLE_STATIC=ON -DWITH_SIMD=0 \
    -DCMAKE_BUILD_TYPE=Release
  cmake --build "$third/libjpeg-win" -j"$(nproc)"
  cmake --install "$third/libjpeg-win"
fi

if [ ! -x "$third/jpegoptim-1.5.6/jpegoptim.exe" ]; then
  fetch "https://github.com/tjko/jpegoptim/archive/refs/tags/v1.5.6.tar.gz" \
    "$third/jpegoptim-1.5.6.tar.gz"
  if [ ! -d "$third/jpegoptim-1.5.6" ]; then
    tar -xzf "$third/jpegoptim-1.5.6.tar.gz" -C "$third"
  fi
  (
    cd "$third/jpegoptim-1.5.6"
    make distclean >/dev/null 2>&1 || true
    rm -f jpegoptim jpegoptim.exe *.o
    ./configure --host=x86_64-w64-mingw32 \
      --with-libjpeg="$prefix" \
      CC="$CC" \
      CPPFLAGS="-I$prefix/include" \
      LDFLAGS="-L$prefix/lib -static" \
      LIBS="-ljpeg"
    rm -f jpegoptim jpegoptim.exe *.o
    make -j"$(nproc)"
  )
fi
if [ -f "$third/jpegoptim-1.5.6/jpegoptim.exe" ]; then
  cp -a "$third/jpegoptim-1.5.6/jpegoptim.exe" "$root/bin/jpegoptim.exe"
else
  cp -a "$third/jpegoptim-1.5.6/jpegoptim" "$root/bin/jpegoptim.exe"
fi

if [ ! -f "$prefix/lib/libpng.a" ]; then
  fetch "https://github.com/pnggroup/libpng/archive/refs/tags/v1.6.44.tar.gz" \
    "$third/libpng-1.6.44.tar.gz"
  if [ ! -d "$third/libpng-1.6.44" ]; then
    tar -xzf "$third/libpng-1.6.44.tar.gz" -C "$third"
  fi
  cmake -S "$third/libpng-1.6.44" -B "$third/libpng-win" \
    -DCMAKE_TOOLCHAIN_FILE="$third/mingw-toolchain.cmake" \
    -DCMAKE_INSTALL_PREFIX="$prefix" \
    -DPNG_SHARED=OFF -DPNG_STATIC=ON -DPNG_TESTS=OFF \
    -DZLIB_LIBRARY="$prefix/lib/libz.a" \
    -DZLIB_INCLUDE_DIR="$prefix/include" \
    -DCMAKE_BUILD_TYPE=Release
  cmake --build "$third/libpng-win" -j"$(nproc)"
  cmake --install "$third/libpng-win"
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
  make clean >/dev/null 2>&1 || true
  make \
    CC="$CC" LD="$CC" \
    PNGINC="$prefix/include" \
    PNGLIB="$prefix/lib" \
    ZINC="$prefix/include" \
    ZLIB="$prefix/lib" \
    CFLAGS="-O3 -Wall" \
    LIBS="$prefix/lib/libpng.a $prefix/lib/libz.a -lm -static"
)
if [ -f "$third/pngcrush-src/pngcrush-1.8.13-nolib/pngcrush.exe" ]; then
  cp -a "$third/pngcrush-src/pngcrush-1.8.13-nolib/pngcrush.exe" "$root/bin/pngcrush.exe"
else
  cp -a "$third/pngcrush-src/pngcrush-1.8.13-nolib/pngcrush" "$root/bin/pngcrush.exe"
fi

fetch "https://storage.googleapis.com/downloads.webmproject.org/releases/webp/libwebp-1.6.0-windows-x64.zip" \
  "$third/libwebp-windows.zip"
if [ ! -f "$root/bin/cwebp.exe" ]; then
  rm -rf "$third/libwebp-win"
  unzip -q "$third/libwebp-windows.zip" -d "$third/libwebp-win"
  found=$(find "$third/libwebp-win" -name cwebp.exe -type f | head -1)
  cp -a "$found" "$root/bin/cwebp.exe"
fi

echo "=== tool DLL dependencies ==="
for exe in jpegoptim.exe pngcrush.exe cwebp.exe; do
  echo "-- $exe"
  x86_64-w64-mingw32-objdump -p "$root/bin/$exe" | awk '/DLL Name/{print}'
done

cargo build --release --target x86_64-pc-windows-gnu
ls -lh "$root/target/x86_64-pc-windows-gnu/release/image-optimizer.exe"
