#!/usr/bin/env bash
# Builds the FFmpeg that Peraz bundles: `ffmpeg` and `ffprobe`, linked statically against the
# encoder libraries Peraz uses, with nothing from the build machine's own libraries.
#
#   scripts/build-ffmpeg.sh [out-dir]          (default: target/ffmpeg)
#
# Needs a C/C++ compiler, make, cmake, meson, ninja, and pkg-config (pkgconf). Every source is
# pinned by version and SHA-256 below; the downloads are kept in <out-dir>/../ffmpeg-work/sources
# and copied to <out-dir>/sources, which is the corresponding source for the binaries (GPL).
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
  "libwebp|1.6.0|https://storage.googleapis.com/downloads.webmproject.org/releases/webp/libwebp-1.6.0.tar.gz|e4ab7009bf0629fd11982d4c2aa83964cf244cffba7347ecd39019a9e38c4564"
)

out=$(mkdir -p "${1:-target/ffmpeg}" && cd "${1:-target/ffmpeg}" && pwd)
work="$out/../ffmpeg-work"
prefix="$work/prefix"
mkdir -p "$work/sources" "$work/build" "$prefix"

os=$(uname -s)
case "$os" in
  Darwin)
    jobs=$(sysctl -n hw.ncpu)
    export MACOSX_DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-11.0}
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
    ;;
  *)
    echo "Not supported yet: $os" >&2
    exit 1
    ;;
esac

# Only libraries built here are visible to pkg-config, never the build machine's own.
export PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig"
unset PKG_CONFIG_PATH
export CFLAGS="-O2" CXXFLAGS="-O2"

log() { printf '\n==> %s\n' "$*"; }

# Downloads, verifies, and unpacks a source; prints the unpacked directory.
fetch() {
  local name=$1 entry n version url sum file dir
  for entry in "${SOURCES[@]}"; do
    IFS='|' read -r n version url sum <<<"$entry"
    [[ $n == "$name" ]] && break
  done
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
  cmake -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$prefix" \
    -DCMAKE_INSTALL_LIBDIR=lib -DBUILD_SHARED_LIBS=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -DCMAKE_POLICY_VERSION_MINIMUM=3.5 "$@"
}

build_x264() {
  log x264
  local dir
  dir=$(fetch x264)
  (cd "$dir" && ./configure --prefix="$prefix" --enable-static --enable-pic --disable-cli \
    && make -j"$jobs" && make install)
}

# x265 as one library with 8-bit and 10-bit encoders (10-bit keeps HDR video).
build_x265() {
  log x265
  local dir
  dir=$(fetch x265)
  local b10="$dir/build-10" b8="$dir/build-8"
  mkdir -p "$b10" "$b8"
  (cd "$b10" && cmake_static -DHIGH_BIT_DEPTH=ON -DEXPORT_C_API=OFF -DENABLE_SHARED=OFF \
    -DENABLE_CLI=OFF ../source && ninja)
  cp "$b10/libx265.a" "$b8/libx265_main10.a"
  (cd "$b8" && cmake_static -DEXTRA_LIB=x265_main10.a -DEXTRA_LINK_FLAGS=-L. -DLINKED_10BIT=ON \
    -DENABLE_SHARED=OFF -DENABLE_CLI=OFF ../source && ninja)
  (cd "$b8" && mv libx265.a libx265_main.a \
    && libtool -static -o libx265.a libx265_main.a libx265_main10.a && ninja install)
}

build_svt_av1() {
  log SVT-AV1
  local dir
  dir=$(fetch svt-av1)
  mkdir -p "$dir/build"
  (cd "$dir/build" && cmake_static -DBUILD_APPS=OFF -DBUILD_TESTING=OFF .. && ninja && ninja install)
}

build_dav1d() {
  log dav1d
  local dir
  dir=$(fetch dav1d)
  (cd "$dir" && meson setup build --prefix="$prefix" --libdir=lib --buildtype=release \
    --default-library=static -Denable_tools=false -Denable_tests=false \
    && ninja -C build && ninja -C build install)
}

build_libwebp() {
  log libwebp
  local dir
  dir=$(fetch libwebp)
  mkdir -p "$dir/build"
  (cd "$dir/build" && cmake_static -DWEBP_BUILD_ANIM_UTILS=OFF -DWEBP_BUILD_CWEBP=OFF \
    -DWEBP_BUILD_DWEBP=OFF -DWEBP_BUILD_GIF2WEBP=OFF -DWEBP_BUILD_IMG2WEBP=OFF \
    -DWEBP_BUILD_VWEBP=OFF -DWEBP_BUILD_WEBPINFO=OFF -DWEBP_BUILD_WEBPMUX=OFF \
    -DWEBP_BUILD_EXTRAS=OFF .. && ninja && ninja install)
}

build_ffmpeg() {
  log FFmpeg
  local dir
  dir=$(fetch ffmpeg)
  local platform=()
  case "$os" in
    Darwin) platform=(--enable-videotoolbox --enable-zlib --enable-bzlib --enable-iconv --extra-libs=-liconv) ;;
  esac
  (cd "$dir" && ./configure --prefix="$prefix" --pkg-config-flags=--static \
    --extra-cflags="-I$prefix/include" --extra-ldflags="-L$prefix/lib" \
    --disable-shared --enable-static --enable-gpl --disable-autodetect \
    --disable-doc --disable-ffplay --disable-network --disable-devices \
    --enable-libx264 --enable-libx265 --enable-libsvtav1 --enable-libdav1d --enable-libwebp \
    "${platform[@]}" \
    && make -j"$jobs" && make install)
}

collect() {
  log "Collecting into $out"
  rm -rf "$out/sources" "$out/licenses"
  mkdir -p "$out/sources" "$out/licenses"
  local tool
  for tool in ffmpeg ffprobe; do
    cp "$prefix/bin/$tool" "$out/$tool"
    strip "$out/$tool"
  done
  cp "$work"/sources/* "$out/sources/"
  cp "$work/build/ffmpeg/COPYING.GPLv3" "$out/licenses/ffmpeg-COPYING.GPLv3"
  cp "$work/build/ffmpeg/LICENSE.md" "$out/licenses/ffmpeg-LICENSE.md"
  cp "$work/build/x264/COPYING" "$out/licenses/x264-COPYING"
  cp "$work/build/x265/COPYING" "$out/licenses/x265-COPYING"
  cp "$work/build/svt-av1/LICENSE.md" "$out/licenses/svt-av1-LICENSE.md"
  cp "$work/build/svt-av1/PATENTS.md" "$out/licenses/svt-av1-PATENTS.md"
  cp "$work/build/dav1d/COPYING" "$out/licenses/dav1d-COPYING"
  cp "$work/build/libwebp/COPYING" "$out/licenses/libwebp-COPYING"
  cp "$work/build/libwebp/PATENTS" "$out/licenses/libwebp-PATENTS"
  printf '%s\n' "${SOURCES[@]}" | awk -F'|' '{print $1, $2, $4}' > "$out/sources/SOURCES.txt"
}

build_x264
build_x265
build_svt_av1
build_dav1d
build_libwebp
build_ffmpeg
collect
"$out/ffmpeg" -hide_banner -version | head -1
