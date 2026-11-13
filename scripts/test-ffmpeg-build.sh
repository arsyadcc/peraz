#!/usr/bin/env bash
# Checks a bundled FFmpeg build from scripts/build-ffmpeg.sh: the encoders Peraz uses are there
# and work, and the binaries load only libraries every machine has.
#
#   scripts/test-ffmpeg-build.sh [dir]     (default: target/ffmpeg)
#
# Encoding needs a machine that can run the binaries; a Windows build cross-built on Linux gets
# only the library check (pass --libraries-only).
set -euo pipefail

only_libraries=""
if [[ ${1:-} == --libraries-only ]]; then
  only_libraries=1
  shift
fi
dir=$(cd "${1:-target/ffmpeg}" && pwd)
exe=""
[[ -f $dir/ffmpeg.exe ]] && exe=.exe

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# Libraries the binaries may load: the system's own, never one from the build machine.
check_libraries() {
  local tool=$1 deps
  if [[ -n $exe ]]; then
    command -v x86_64-w64-mingw32-objdump >/dev/null || { echo "skip library check for $tool"; return; }
    deps=$(x86_64-w64-mingw32-objdump -p "$tool" | sed -n 's/^\s*DLL Name: //p')
    # Windows system DLLs only (MinGW's own runtime DLLs must be linked in statically).
    echo "$deps" | grep -viE '^(kernel32|user32|advapi32|bcrypt|ole32|oleaut32|shell32|shlwapi|gdi32|psapi|secur32|ws2_32|msvcrt|ucrtbase|api-ms-win-[a-z0-9-]+)\.dll$' \
      && fail "$tool loads a library that is not part of Windows"
  elif [[ $(uname -s) == Darwin ]]; then
    deps=$(otool -L "$tool" | tail -n +2 | awk '{print $1}')
    echo "$deps" | grep -vE '^/(usr/lib|System/Library)/' \
      && fail "$tool loads a library that is not part of macOS"
  else
    deps=$(readelf -d "$tool" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')
    echo "$deps" | grep -vE '^(libc|libm|libmvec|libdl|libpthread|librt|libstdc\+\+|libgcc_s|ld-linux-[a-z0-9_-]+)\.so(\.[0-9]+)*$' \
      && fail "$tool loads a library that is not part of the base system"
  fi
  echo "$(basename "$tool"): $(echo "$deps" | tr '\n' ' ')"
}

for tool in ffmpeg ffprobe; do
  [[ -f $dir/$tool$exe ]] || fail "$dir/$tool$exe is missing"
  check_libraries "$dir/$tool$exe"
done
[[ -n $only_libraries ]] && exit 0

ff="$dir/ffmpeg$exe"
probe="$dir/ffprobe$exe"
# Outputs are read whole first: `grep -q` stops reading early, and with pipefail the closed pipe
# would count as a failure of ffmpeg.
version=$("$ff" -hide_banner -version)
echo "${version%%$'\n'*}"
grep -q -- --enable-gpl <<<"$version" || fail "not a GPL build"
grep -q -- --enable-nonfree <<<"$version" && fail "a nonfree build cannot be shared"

encoders=$("$ff" -hide_banner -encoders)
for encoder in libx264 libx265 libsvtav1 libwebp libwebp_anim aac gif png mjpeg; do
  grep -qE "^ [A-Z.]{6} $encoder " <<<"$encoders" || fail "encoder $encoder is missing"
done
decoders=$("$ff" -hide_banner -decoders)
grep -qE '^ [A-Z.]{6} libdav1d ' <<<"$decoders" || fail "decoder libdav1d is missing"
filters=$("$ff" -hide_banner -filters)
grep -qE '^ [A-Z.]+ libvmaf ' <<<"$filters" || fail "filter libvmaf is missing"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
src=(-v error -f lavfi -i testsrc2=size=320x240:rate=30 -t 1)
"$ff" "${src[@]}" -c:v libx264 "$tmp/h264.mp4"
"$ff" "${src[@]}" -c:v libx265 -pix_fmt yuv420p10le "$tmp/hevc10.mp4"
"$ff" "${src[@]}" -c:v libsvtav1 "$tmp/av1.mp4"
"$ff" "${src[@]}" -frames:v 1 -c:v libwebp "$tmp/still.webp"
"$ff" -v error -f lavfi -i sine=duration=1 -c:a aac "$tmp/audio.m4a"
"$ff" -v error -c:v libdav1d -i "$tmp/av1.mp4" -f null -
# VMAF with the built-in model `--target-vmaf` names: a file against itself scores near 100.
(cd "$tmp" && "$ff" -v error -i h264.mp4 -i h264.mp4 \
  -lavfi "libvmaf=model=version=vmaf_v0.6.1:log_fmt=json:log_path=vmaf.json" -f null -)
grep -q '"vmaf"' "$tmp/vmaf.json" || fail "libvmaf wrote no score"

pix_fmt=$("$probe" -v error -select_streams v:0 -show_entries stream=pix_fmt -of csv=p=0 "$tmp/hevc10.mp4")
[[ ${pix_fmt%$'\r'} == yuv420p10le ]] || fail "10-bit HEVC came out as $pix_fmt"
echo "OK: $dir"
