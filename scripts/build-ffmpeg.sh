#!/usr/bin/env bash
# Builds the FFmpeg that Peraz bundles: `ffmpeg` and `ffprobe`, linked statically against the
# encoder libraries Peraz uses, with nothing from the build machine's own libraries.
#
#   scripts/build-ffmpeg.sh [out-dir]                       (default: target/ffmpeg)
#   PERAZ_FFMPEG_TARGET=windows scripts/build-ffmpeg.sh     (Windows x64, cross-built on Linux)
#
# Builds for the machine it runs on (macOS or Linux), or for Windows x64 with MinGW-w64.
# Needs a C/C++ compiler, make, cmake, meson, ninja, pkg-config, xxd, and nasm on x86_64.
#
# Every source is pinned by version and SHA-256 below. <out-dir>/sources gets the exact archives
# the binaries were built from (the corresponding source the GPL asks for), <out-dir>/licenses
# their license texts.
#
# This is a GPL build (x264, x265). No --enable-nonfree: the result can be redistributed.
set -euo pipefail

# name | version | url | sha256
SOURCES=(
  "ffmpeg|9.0.2|https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz|8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e"
  "x264|b35605ace3ddf7c1a5d67a2eb553f034aef41d55|https://code.videolan.org/videolan/x264/-/archive/b35605ace3ddf7c1a5d67a2eb553f034aef41d55/x264-b35605ace3ddf7c1a5d67a2eb553f034aef41d55.tar.bz2|6eeb82934e69fd51e043bd8c5b0d152839638d1ce7aa4eea65a3fedcf83ff224"
  "x265|4.3|https://github.com/Multicorewareinc/x265/releases/download/4.3/x265_4.3.tar.gz|83c53e4c8bbb8f1e33ed59e10a7d621d1d7801ca853910c3eb41f038b8ffb121"
  "svt-av1|4.3.0|https://gitlab.com/AOMediaCodec/SVT-AV1/-/archive/v4.3.0/SVT-AV1-v4.3.0.tar.bz2|9223ce06e6172ad41357ed23b3b00f036e03794b86b0e20ea9ccb3f648b8955d"
  "dav1d|1.5.4|https://code.videolan.org/videolan/dav1d/-/archive/1.5.4/dav1d-1.5.4.tar.bz2|2abfb0c89212e6e4733a54e0ae509ec00a5b845a6360946f918806e14aedb011"
  "libvmaf|3.2.1|https://github.com/Netflix/vmaf/archive/refs/tags/v3.2.1.tar.gz|5df7386911bc15fd1ca783132528748d219768ae4fc5f8e0b61184f041648092"
  "libwebp|1.6.0|https://storage.googleapis.com/downloads.webmproject.org/releases/webp/libwebp-1.6.0.tar.gz|e4ab7009bf0629fd11982d4c2aa83964cf244cffba7347ecd39019a9e38c4564"
  "zlib|1.4|https://github.com/madler/zlib/releases/download/v1.4/zlib-1.4.tar.xz|4f11c512053d7bab8d9e9b89bde3575add3e4c50a383b42ef6cb837035fc5a7c"
  "nv-codec-headers|13.1.15.0|https://github.com/FFmpeg/nv-codec-headers/releases/download/n13.1.15.0/nv-codec-headers-13.1.15.0.tar.gz|52532ceade3d5c1af62624986f13cf01b63c910576b08c0c278756c5e4b41ad0"
  "amf-headers|1.5.3|https://github.com/GPUOpen-LibrariesAndSDKs/AMF/releases/download/v1.5.3/AMF-headers-v1.5.3.tar.gz|0f521ad98041f8001147cded4a660127f18b1c27a801a5cd691bda1fa48381be"
)

out=$(mkdir -p "${1:-target/ffmpeg}" && cd "${1:-target/ffmpeg}" && pwd)
work="$out/../ffmpeg-work"
prefix="$work/prefix"
rm -rf "$work/build" "$prefix"
mkdir -p "$work/sources" "$work/build" "$prefix"

case "${PERAZ_FFMPEG_TARGET:-$(uname -s)}" in
  Darwin) system=macos ;;
  Linux) system=linux ;;
  windows) system=windows ;;
  *)
    echo "Unsupported target: ${PERAZ_FFMPEG_TARGET:-$(uname -s)}" >&2
    exit 1
    ;;
esac

cross="" exe=""
case "$system" in
  macos)
    jobs=$(sysctl -n hw.ncpu)
    export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}
    export CC=clang CXX=clang++ AR=ar STRIP=strip
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
    ;;
  linux | windows)
    jobs=$(nproc)
    export CC=gcc CXX=g++ AR=ar STRIP=strip
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
    ;;
esac
if [[ $system == windows ]]; then
  cross=x86_64-w64-mingw32 exe=.exe
  export CC=$cross-gcc CXX=$cross-g++ AR=$cross-ar STRIP=$cross-strip
fi

# Only libraries built here are visible to pkg-config, never the build machine's own.
export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
unset PKG_CONFIG_PATH
export CFLAGS="-O2" CXXFLAGS="-O2"
[[ $system == linux ]] && CFLAGS+=" -fPIC" CXXFLAGS+=" -fPIC"

used=()
log() { printf '\n==> %s\n' "$*" >&2; }

# Downloads, verifies, and unpacks a source; prints the unpacked directory.
fetch() {
  local name=$1 entry n version url sum file dir found=""
  for entry in "${SOURCES[@]}"; do
    IFS='|' read -r n version url sum <<<"$entry"
    if [[ $n == "$name" ]]; then
      found=$entry
      break
    fi
  done
  [[ -n $found ]] || { echo "Unknown source: $name" >&2; exit 1; }
  file="$work/sources/$(basename "$url")"
  [[ -f $file ]] || curl -fsSL --retry 3 -o "$file" "$url"
  if [[ $(sha256 "$file") != "$sum" ]]; then
    echo "Checksum mismatch for $file" >&2
    exit 1
  fi
  dir="$work/build/$name"
  rm -rf "$dir" && mkdir -p "$dir"
  tar -xf "$file" -C "$dir" --strip-components 1
  echo "$dir"
}

cmake_static() {
  local args=(-G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$prefix"
    -DCMAKE_INSTALL_LIBDIR=lib -DBUILD_SHARED_LIBS=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON
    -DCMAKE_POLICY_VERSION_MINIMUM=3.5)
  if [[ -n $cross ]]; then
    args+=(-DCMAKE_SYSTEM_NAME=Windows -DCMAKE_SYSTEM_PROCESSOR=x86_64
      -DCMAKE_C_COMPILER="$CC" -DCMAKE_CXX_COMPILER="$CXX" -DCMAKE_RC_COMPILER="$cross-windres"
      -DCMAKE_FIND_ROOT_PATH="$prefix" -DCMAKE_FIND_ROOT_PATH_MODE_PROGRAM=NEVER
      -DCMAKE_FIND_ROOT_PATH_MODE_LIBRARY=ONLY -DCMAKE_FIND_ROOT_PATH_MODE_INCLUDE=ONLY)
  fi
  cmake "${args[@]}" "$@"
}

# Merges static libraries into one.
merge_archives() {
  local output=$1
  shift
  if [[ $system == macos ]]; then
    libtool -static -o "$output" "$@"
  else
    {
      echo "CREATE $output"
      local lib
      for lib in "$@"; do echo "ADDLIB $lib"; done
      echo "SAVE"
      echo "END"
    } | "$AR" -M
  fi
}

build_zlib() {
  log zlib
  used+=(zlib)
  local dir
  dir=$(fetch zlib)
  (
    cd "$dir"
    for src in adler32 compress crc32 deflate gzclose gzlib gzread gzwrite infback inffast \
      inflate inftrees trees uncompr zutil; do
      $CC $CFLAGS -c "$src.c" -o "$src.o"
    done
    "$AR" rcs libz.a ./*.o
    mkdir -p "$prefix/include" "$prefix/lib/pkgconfig"
    cp zlib.h zconf.h "$prefix/include/"
    cp libz.a "$prefix/lib/"
    printf 'prefix=%s\nName: zlib\nDescription: zlib\nVersion: 1.4\nLibs: -L%s/lib -lz\nCflags: -I%s/include\n' \
      "$prefix" "$prefix" "$prefix" > "$prefix/lib/pkgconfig/zlib.pc"
  )
}

build_x264() {
  log x264
  used+=(x264)
  local dir args=(--prefix="$prefix" --enable-static --enable-pic --disable-cli)
  dir=$(fetch x264)
  [[ -n $cross ]] && args+=(--host="$cross" --cross-prefix="$cross-")
  (cd "$dir" && ./configure "${args[@]}" && make -j"$jobs" && make install)
}

# x265 as one library with 8-bit and 10-bit encoders (10-bit keeps HDR video).
build_x265() {
  log x265
  used+=(x265)
  local dir options=(-DENABLE_SHARED=OFF -DENABLE_CLI=OFF)
  dir=$(fetch x265)
  # x265's SVE2 code needs a compiler that knows Armv9 (GCC 12 or newer); without one, it uses
  # its Neon and SVE code. It picks the code for the CPU at run time either way.
  if [[ $(uname -m) == aarch64 ]] && ! echo 'int x;' | $CC -march=armv9-a -x c -c -o /dev/null - 2>/dev/null; then
    options+=(-DENABLE_SVE2=OFF -DENABLE_SVE2_BITPERM=OFF)
  fi
  local b10="$dir/build-10" b8="$dir/build-8"
  mkdir -p "$b10" "$b8"
  (cd "$b10" && cmake_static "${options[@]}" -DHIGH_BIT_DEPTH=ON -DEXPORT_C_API=OFF ../source \
    && ninja)
  cp "$b10/libx265.a" "$b8/libx265_main10.a"
  (cd "$b8" && cmake_static "${options[@]}" -DEXTRA_LIB=x265_main10.a -DEXTRA_LINK_FLAGS=-L. \
    -DLINKED_10BIT=ON ../source && ninja)
  (cd "$b8" && mv libx265.a libx265_main.a \
    && merge_archives libx265.a libx265_main.a libx265_main10.a && ninja install)
  # On Windows, -lgcc_s would pull in libgcc_s_seh-1.dll; the static libgcc is linked anyway.
  sed -i.bak 's/ -lgcc_s//g' "$prefix/lib/pkgconfig/x265.pc"
  rm -f "$prefix/lib/pkgconfig/x265.pc.bak"
}

build_svt_av1() {
  log SVT-AV1
  used+=(svt-av1)
  local dir
  dir=$(fetch svt-av1)
  mkdir -p "$dir/build"
  (cd "$dir/build" && cmake_static -DBUILD_APPS=OFF -DBUILD_TESTING=OFF .. && ninja && ninja install)
}

# A static meson build into the prefix, cross-compiled for Windows when needed. Runs in the
# current directory; the arguments go to `meson setup`.
meson_static() {
  local args=(--prefix="$prefix" --libdir=lib --buildtype=release --default-library=static)
  if [[ -n $cross ]]; then
    cat > cross.ini <<EOF
[binaries]
c = '$CC'
cpp = '$CXX'
ar = '$AR'
strip = '$STRIP'
windres = '$cross-windres'

[host_machine]
system = 'windows'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'
EOF
    args+=(--cross-file=cross.ini)
  fi
  meson setup build "${args[@]}" "$@" && ninja -C build && ninja -C build install
}

build_dav1d() {
  log dav1d
  used+=(dav1d)
  local dir
  dir=$(fetch dav1d)
  (cd "$dir" && meson_static -Denable_tools=false -Denable_tests=false)
}

# VMAF scoring for `--target-vmaf`, with its models built in (which needs xxd).
build_libvmaf() {
  log libvmaf
  used+=(libvmaf)
  command -v xxd >/dev/null || { echo "xxd is needed to build the VMAF models in" >&2; exit 1; }
  local dir
  dir=$(fetch libvmaf)
  (cd "$dir/libvmaf" && meson_static -Denable_tests=false -Denable_docs=false \
    -Denable_tools=false -Dbuilt_in_models=true -Denable_float=false)
  # Part of libvmaf is C++, but its pkg-config file does not name the C++ runtime, so FFmpeg's
  # static link check fails without this.
  local cxx_runtime=-lstdc++
  [[ $system == macos ]] && cxx_runtime=-lc++
  echo "Libs.private: $cxx_runtime" >> "$prefix/lib/pkgconfig/libvmaf.pc"
}

build_libwebp() {
  log libwebp
  used+=(libwebp)
  local dir
  dir=$(fetch libwebp)
  mkdir -p "$dir/build"
  (cd "$dir/build" && cmake_static -DWEBP_BUILD_ANIM_UTILS=OFF -DWEBP_BUILD_CWEBP=OFF \
    -DWEBP_BUILD_DWEBP=OFF -DWEBP_BUILD_GIF2WEBP=OFF -DWEBP_BUILD_IMG2WEBP=OFF \
    -DWEBP_BUILD_VWEBP=OFF -DWEBP_BUILD_WEBPINFO=OFF -DWEBP_BUILD_WEBPMUX=OFF \
    -DWEBP_BUILD_EXTRAS=OFF .. && ninja && ninja install)
}

# NVIDIA and AMD encoders: headers only, the drivers' libraries are loaded at run time.
install_gpu_headers() {
  log "NVENC and AMF headers"
  used+=(nv-codec-headers amf-headers)
  local dir
  dir=$(fetch nv-codec-headers)
  make -C "$dir" PREFIX="$prefix" install
  dir=$(fetch amf-headers)
  mkdir -p "$prefix/include"
  cp -R "$dir/AMF" "$prefix/include/"
}

build_ffmpeg() {
  log FFmpeg
  used+=(ffmpeg)
  local dir
  dir=$(fetch ffmpeg)
  local args=(--prefix="$prefix" --pkg-config=pkg-config --pkg-config-flags=--static
    --extra-cflags="-I$prefix/include" --disable-shared --enable-static --enable-gpl
    --disable-autodetect --disable-doc --disable-ffplay --disable-network --disable-devices
    --enable-indev=lavfi
    --enable-zlib --enable-libx264 --enable-libx265 --enable-libsvtav1 --enable-libdav1d
    --enable-libwebp --enable-libvmaf)
  case "$system" in
    macos)
      args+=(--extra-ldflags="-L$prefix/lib" --enable-pthreads --enable-videotoolbox
        --enable-bzlib --enable-iconv --extra-libs=-liconv)
      ;;
    linux)
      args+=(--extra-ldflags="-L$prefix/lib" --enable-pthreads --enable-ffnvcodec --enable-nvenc
        --enable-amf --extra-libs="-lpthread -lm -ldl")
      ;;
    windows)
      args+=(--extra-ldflags="-L$prefix/lib -static" --target-os=mingw32 --arch=x86_64
        --cross-prefix="$cross-" --enable-cross-compile --enable-w32threads --enable-ffnvcodec
        --enable-nvenc --enable-amf)
      ;;
  esac
  (cd "$dir" && ./configure "${args[@]}" && make -j"$jobs" && make install)
}

# The license comment at the top of a header file.
header_license() {
  sed -n '1,/\*\//p' "$1"
}

collect() {
  log "Collecting into $out"
  rm -rf "$out/sources" "$out/licenses" "$out"/ffmpeg* "$out"/ffprobe*
  mkdir -p "$out/sources" "$out/licenses"
  local tool name entry n version url sum
  for tool in ffmpeg ffprobe; do
    cp "$prefix/bin/$tool$exe" "$out/$tool$exe"
    "$STRIP" "$out/$tool$exe"
  done
  local b="$work/build"
  cp "$b/ffmpeg/COPYING.GPLv3" "$out/licenses/ffmpeg-COPYING.GPLv3"
  cp "$b/ffmpeg/LICENSE.md" "$out/licenses/ffmpeg-LICENSE.md"
  cp "$b/x264/COPYING" "$out/licenses/x264-COPYING"
  cp "$b/x265/COPYING" "$out/licenses/x265-COPYING"
  cp "$b/svt-av1/LICENSE.md" "$out/licenses/svt-av1-LICENSE.md"
  cp "$b/svt-av1/PATENTS.md" "$out/licenses/svt-av1-PATENTS.md"
  cp "$b/dav1d/COPYING" "$out/licenses/dav1d-COPYING"
  cp "$b/libvmaf/LICENSE" "$out/licenses/libvmaf-LICENSE"
  cp "$b/libwebp/COPYING" "$out/licenses/libwebp-COPYING"
  cp "$b/libwebp/PATENTS" "$out/licenses/libwebp-PATENTS"
  if [[ $system != macos ]]; then
    cp "$b/zlib/LICENSE" "$out/licenses/zlib-LICENSE"
    header_license "$b/nv-codec-headers/include/ffnvcodec/nvEncodeAPI.h" \
      > "$out/licenses/nv-codec-headers-LICENSE"
    header_license "$b/amf-headers/AMF/core/Platform.h" > "$out/licenses/amf-headers-LICENSE"
  fi
  for name in "${used[@]}"; do
    for entry in "${SOURCES[@]}"; do
      IFS='|' read -r n version url sum <<<"$entry"
      if [[ $n == "$name" ]]; then
        cp "$work/sources/$(basename "$url")" "$out/sources/"
        echo "$n $version $(basename "$url") $sum" >> "$out/sources/SOURCES.txt"
      fi
    done
  done
}

[[ $system != macos ]] && build_zlib
build_x264
build_x265
build_svt_av1
build_dav1d
build_libwebp
build_libvmaf
[[ $system != macos ]] && install_gpu_headers
build_ffmpeg
collect
log "Done: $out/ffmpeg$exe"
