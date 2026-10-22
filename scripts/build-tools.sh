#!/bin/sh
# Builds jpegoptim and pngcrush, and downloads the official static cwebp, into bin/.
# Needs: curl, tar, gcc, make, cmake. A system libz.a is used when one is
# installed; otherwise a static zlib is built here.
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

# CMake installs to lib or lib64 depending on the distro.
archive_in() {
  prefix=$1
  name=$2
  for dir in lib lib64; do
    if [ -f "$prefix/$dir/$name" ]; then
      printf '%s\n' "$prefix/$dir/$name"
      return 0
    fi
  done
  return 1
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
if ! jpeg_a=$(archive_in "$third/jpeg-install" libjpeg.a); then
  cmake -S "$third/libjpeg-turbo-3.0.4" -B "$third/libjpeg-turbo-build" \
    -DENABLE_SHARED=OFF -DENABLE_STATIC=ON -DWITH_SIMD=0 \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$third/jpeg-install" \
    -DCMAKE_INSTALL_LIBDIR=lib
  cmake --build "$third/libjpeg-turbo-build" -j"$(nproc)"
  cmake --install "$third/libjpeg-turbo-build"
  jpeg_a=$(archive_in "$third/jpeg-install" libjpeg.a)
fi

fetch "https://github.com/tjko/jpegoptim/archive/refs/tags/v1.5.6.tar.gz" \
  "$third/jpegoptim-1.5.6.tar.gz"
if [ ! -d "$third/jpegoptim-1.5.6" ]; then
  tar -xzf "$third/jpegoptim-1.5.6.tar.gz" -C "$third"
fi
# A previous build looked in lib/ and linked the system libjpeg.so instead.
if [ -x "$third/jpegoptim-1.5.6/jpegoptim" ] && ldd "$third/jpegoptim-1.5.6/jpegoptim" | grep -q 'libjpeg\.so'; then
  rm -f "$third/jpegoptim-1.5.6/jpegoptim"
fi
if [ ! -x "$third/jpegoptim-1.5.6/jpegoptim" ]; then
  (
    cd "$third/jpegoptim-1.5.6"
    ./configure --with-libjpeg="$third/jpeg-install" --with-arith \
      CPPFLAGS="-I$third/jpeg-install/include"
    make -j"$(nproc)" LIBS="$jpeg_a -lm" LDFLAGS=
  )
fi
cp -a "$third/jpegoptim-1.5.6/jpegoptim" "$bin/jpegoptim"

# pngcrush is linked statically. Debian keeps libz.a under the multiarch
# directory; Arch and others use /usr/lib, and some installs have no static zlib.
zlib_a=""
zlib_inc=/usr/include
for candidate in \
  /usr/lib/x86_64-linux-gnu/libz.a \
  /usr/lib64/libz.a \
  /usr/lib/libz.a \
  /lib/x86_64-linux-gnu/libz.a \
  /lib64/libz.a
do
  if [ -f "$candidate" ]; then
    zlib_a=$candidate
    break
  fi
done
if [ -z "$zlib_a" ]; then
  fetch "https://github.com/madler/zlib/releases/download/v1.3.1/zlib-1.3.1.tar.gz" \
    "$third/zlib-1.3.1.tar.gz"
  if [ ! -d "$third/zlib-1.3.1" ]; then
    tar -xzf "$third/zlib-1.3.1.tar.gz" -C "$third"
  fi
  if [ ! -f "$third/zlib-install/lib/libz.a" ]; then
    (
      cd "$third/zlib-1.3.1"
      ./configure --static --prefix="$third/zlib-install" --libdir="$third/zlib-install/lib"
      make -j"$(nproc)"
      make install
    )
  fi
  zlib_a="$third/zlib-install/lib/libz.a"
  zlib_inc="$third/zlib-install/include"
fi

fetch "https://github.com/pnggroup/libpng/archive/refs/tags/v1.6.44.tar.gz" \
  "$third/libpng-1.6.44.tar.gz"
if [ ! -d "$third/libpng-1.6.44" ]; then
  tar -xzf "$third/libpng-1.6.44.tar.gz" -C "$third"
fi
if ! png_a=$(archive_in "$third/png-install" libpng.a); then
  cmake -S "$third/libpng-1.6.44" -B "$third/libpng-build" \
    -DPNG_SHARED=OFF -DPNG_STATIC=ON -DPNG_TESTS=OFF \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$third/png-install" \
    -DCMAKE_INSTALL_LIBDIR=lib \
    -DZLIB_LIBRARY="$zlib_a" \
    -DZLIB_INCLUDE_DIR="$zlib_inc"
  cmake --build "$third/libpng-build" -j"$(nproc)"
  cmake --install "$third/libpng-build"
  png_a=$(archive_in "$third/png-install" libpng.a)
fi

fetch "http://deb.debian.org/debian/pool/main/p/pngcrush/pngcrush_1.8.13.orig.tar.xz" \
  "$third/pngcrush-1.8.13.orig.tar.xz"
if [ ! -d "$third/pngcrush-src/pngcrush-1.8.13-nolib" ]; then
  mkdir -p "$third/pngcrush-src"
  tar -xJf "$third/pngcrush-1.8.13.orig.tar.xz" -C "$third/pngcrush-src"
fi
src="$third/pngcrush-src/pngcrush-1.8.13-nolib/pngcrush.c"
python3 "$root/scripts/patch-pngcrush.py" "$src"
(
  cd "$third/pngcrush-src/pngcrush-1.8.13-nolib"
  # Drop the old object so a new zlib include path is actually compiled in.
  rm -f pngcrush.o pngcrush
  make \
    PNGINC="$third/png-install/include" \
    CFLAGS="-O3 -Wall -I$zlib_inc" \
    LIBS="$png_a $zlib_a -lm"
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
