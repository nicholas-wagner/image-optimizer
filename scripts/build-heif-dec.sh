# Sourced by scripts/build-tools.sh and scripts/build-windows-inner.sh.
# Builds a static heif-dec (libheif + libde265) into $bin.
#
# Required: root, third, bin, zlib_a, zlib_inc, png_a, png_inc, and fetch().
# Optional: heif_toolchain (CMake toolchain file), heif_exe (".exe" or empty).

build_heif_dec() {
  heif_ver=1.23.7
  de265_ver=1.1.3
  heif_exe=${heif_exe:-}
  suffix=
  if [ -n "$heif_exe" ]; then
    suffix=-win
  fi
  de265_prefix="$third/de265-install$suffix"
  heif_prefix="$third/heif-install$suffix"
  dest="$bin/heif-dec$heif_exe"
  if [ -f "$dest" ]; then
    return 0
  fi

  toolchain_arg=
  compiler_args=
  if [ -n "${heif_toolchain:-}" ]; then
    toolchain_arg="-DCMAKE_TOOLCHAIN_FILE=$heif_toolchain"
  else
    compiler_args="-DCMAKE_C_COMPILER=${CC:-gcc} -DCMAKE_CXX_COMPILER=${CXX:-g++}"
  fi
  if [ -n "$heif_exe" ]; then
    link_flags="-static -static-libstdc++ -static-libgcc"
  else
    link_flags="-static-libstdc++ -static-libgcc"
  fi

  fetch "https://github.com/strukturag/libde265/releases/download/v${de265_ver}/libde265-${de265_ver}.tar.gz" \
    "$third/libde265-${de265_ver}.tar.gz"
  if [ ! -d "$third/libde265-${de265_ver}" ]; then
    tar -xzf "$third/libde265-${de265_ver}.tar.gz" -C "$third"
  fi
  if [ ! -f "$de265_prefix/lib/libde265.a" ]; then
    rm -rf "$third/libde265-build$suffix"
    # shellcheck disable=SC2086
    cmake -S "$third/libde265-${de265_ver}" -B "$third/libde265-build$suffix" \
      $toolchain_arg \
      $compiler_args \
      -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_INSTALL_PREFIX="$de265_prefix" \
      -DCMAKE_INSTALL_LIBDIR=lib \
      -DBUILD_SHARED_LIBS=OFF \
      -DENABLE_SDL=OFF \
      -DENABLE_DECODER=ON \
      -DENABLE_ENCODER=OFF \
      -DENABLE_SHERLOCK265=OFF \
      -DENABLE_AVX512=OFF \
      -DCMAKE_POSITION_INDEPENDENT_CODE=ON
    cmake --build "$third/libde265-build$suffix" -j"$(nproc)"
    cmake --install "$third/libde265-build$suffix"
  fi
  if [ ! -f "$de265_prefix/lib/libde265.a" ]; then
    echo "libde265.a was not installed" >&2
    exit 1
  fi

  fetch "https://github.com/strukturag/libheif/releases/download/v${heif_ver}/libheif-${heif_ver}.tar.gz" \
    "$third/libheif-${heif_ver}.tar.gz"
  if [ ! -d "$third/libheif-${heif_ver}" ]; then
    tar -xzf "$third/libheif-${heif_ver}.tar.gz" -C "$third"
  fi
  if [ ! -f "$heif_prefix/bin/heif-dec$heif_exe" ]; then
    rm -rf "$third/libheif-build$suffix"
    # shellcheck disable=SC2086
    cmake -S "$third/libheif-${heif_ver}" -B "$third/libheif-build$suffix" \
      $toolchain_arg \
      $compiler_args \
      -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_INSTALL_PREFIX="$heif_prefix" \
      -DCMAKE_INSTALL_LIBDIR=lib \
      -DCMAKE_FIND_LIBRARY_SUFFIXES=".a" \
      "-DCMAKE_EXE_LINKER_FLAGS=$link_flags" \
      -DBUILD_SHARED_LIBS=OFF \
      -DENABLE_PLUGIN_LOADING=OFF \
      -DWITH_LIBDE265=ON \
      -DWITH_LIBDE265_PLUGIN=OFF \
      -DLIBDE265_INCLUDE_DIR="$de265_prefix/include" \
      -DLIBDE265_LIBRARY="$de265_prefix/lib/libde265.a" \
      -DWITH_X265=OFF \
      -DWITH_X264=OFF \
      -DWITH_KVAZAAR=OFF \
      -DWITH_UVG266=OFF \
      -DWITH_VVDEC=OFF \
      -DWITH_VVENC=OFF \
      -DWITH_OpenH264_DECODER=OFF \
      -DWITH_DAV1D=OFF \
      -DWITH_AOM_DECODER=OFF \
      -DWITH_AOM_ENCODER=OFF \
      -DWITH_SvtEnc=OFF \
      -DWITH_RAV1E=OFF \
      -DWITH_JPEG_DECODER=OFF \
      -DWITH_JPEG_ENCODER=OFF \
      -DWITH_OpenJPEG_ENCODER=OFF \
      -DWITH_OpenJPEG_DECODER=OFF \
      -DWITH_FFMPEG_DECODER=OFF \
      -DWITH_OPENJPH_ENCODER=OFF \
      -DWITH_UNCOMPRESSED_CODEC=OFF \
      -DWITH_WEBCODECS=OFF \
      -DWITH_LIBSHARPYUV=OFF \
      -DWITH_EXAMPLES=ON \
      -DWITH_EXAMPLE_HEIF_THUMB=OFF \
      -DWITH_EXAMPLE_HEIF_VIEW=OFF \
      -DWITH_GDK_PIXBUF=OFF \
      -DBUILD_TESTING=OFF \
      -DBUILD_DOCUMENTATION=OFF \
      -DWITH_HEADER_COMPRESSION=OFF \
      -DPNG_LIBRARY="$png_a" \
      -DPNG_PNG_INCLUDE_DIR="$png_inc" \
      -DZLIB_LIBRARY="$zlib_a" \
      -DZLIB_INCLUDE_DIR="$zlib_inc" \
      -DCMAKE_DISABLE_FIND_PACKAGE_TIFF=ON \
      -DCMAKE_DISABLE_FIND_PACKAGE_WEBP=ON \
      -DCMAKE_DISABLE_FIND_PACKAGE_JPEG=ON
    cmake --build "$third/libheif-build$suffix" -j"$(nproc)"
    cmake --install "$third/libheif-build$suffix"
  fi
  if [ ! -f "$heif_prefix/bin/heif-dec$heif_exe" ]; then
    echo "heif-dec was not installed" >&2
    exit 1
  fi
  cp -a "$heif_prefix/bin/heif-dec$heif_exe" "$dest"
  chmod +x "$dest"

  if [ -z "$heif_exe" ]; then
    if ldd "$dest" | grep -E 'libheif|libde265|libpng|libz\.so|libstdc\+\+|libgcc_s'; then
      echo "heif-dec is linked to a shared library it should contain" >&2
      exit 1
    fi
  fi
}
