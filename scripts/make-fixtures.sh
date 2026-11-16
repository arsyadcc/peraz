#!/usr/bin/env bash
# Regenerates test samples into ./fixtures (not committed).
# Requires: ffmpeg, zip, and curl for the HDR photos. On macOS, sips creates the HEIC and PDF
# samples, and Swift the HEIC with a gain map; elsewhere they are skipped.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/fixtures"
mkdir -p "$out"
cd "$out"

ff() { ffmpeg -hide_banner -loglevel error -y "$@"; }

echo "video: 1080p30 H.264 + stereo AAC, 10 s"
ff -f lavfi -i "testsrc2=size=1920x1080:rate=30:duration=10" \
   -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=10" \
   -ac 2 -c:v libx264 -crf 18 -pix_fmt yuv420p -c:a aac -b:a 192k -shortest video-1080p-audio.mp4

echo "video: 720p without audio, 5 s"
ff -f lavfi -i "testsrc2=size=1280x720:rate=30:duration=5" \
   -c:v libx264 -crf 18 -pix_fmt yuv420p video-720p-noaudio.mp4

echo "video: 4K60 HEVC, 3 s"
ff -f lavfi -i "testsrc2=size=3840x2160:rate=60:duration=3" \
   -c:v libx265 -crf 20 -tag:v hvc1 -pix_fmt yuv420p video-4k60-hevc.mp4 2>/dev/null \
   || echo "  skipped: local ffmpeg has no libx265"

echo "video: 720p HDR10 (10-bit HEVC, BT.2020, PQ), 2 s"
ff -f lavfi -i "testsrc2=size=1280x720:rate=30:duration=2,format=yuv420p10le,setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc:range=tv" \
   -c:v libx265 -crf 20 -tag:v hvc1 \
   -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv \
   -x265-params "log-level=error:hdr10=1:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400" \
   video-hdr10-hevc.mp4 2>/dev/null \
   || echo "  skipped: local ffmpeg has no libx265"

echo "images: opaque PNG, transparent PNG, JPEG"
ff -f lavfi -i "testsrc2=size=1600x1200,format=rgb24" -frames:v 1 image-opaque.png
ff -f lavfi -i "testsrc2=size=1200x900,format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lt(hypot(X-W/2,Y-H/2),H/3),255,X*255/W)'" \
   -frames:v 1 image-alpha.png
ff -i image-opaque.png -q:v 2 image-photo.jpg

echo "HDR photos (downloaded)"
# From libavif's test data (https://github.com/AOMediaCodec/libavif/tree/main/tests/data),
# under libavif's license (BSD 2-Clause); its README names the source of each picture. The
# commit is pinned and every file is checked against its SHA-256.
#   hdr-ultrahdr.jpg       a JPEG with a gain map (Ultra HDR), saved from Photoshop
#   hdr-apple-gainmap.jpg  a JPEG with a gain map as iPhones store it
#   hdr-gainmap.avif       an AVIF with a gain map (ISO 21496-1)
#   hdr-pq.avif            a 10-bit AVIF that is HDR by its transfer function (PQ), no gain map
libavif="https://raw.githubusercontent.com/AOMediaCodec/libavif/37a5461042aa3b1eb105193724ee41bfd2478d0b/tests/data"
hdr_sample() { # name in ./fixtures, where it is downloaded from, SHA-256
  if curl -fsSL --max-time 60 -o "$1.part" "$2" \
     && echo "$3  $1.part" | shasum -a 256 -c - >/dev/null 2>&1; then
    mv "$1.part" "$1"
  else
    rm -f "$1.part"
    echo "  skipped: could not download $1, or its checksum is not the expected one"
  fi
}
hdr_sample hdr-ultrahdr.jpg "$libavif/seine_sdr_gainmap_srgb.jpg" 1aaf0a1afa91a485c13e4ad696a3da1fe1ddd2675417e3812a23a9d13f765262
hdr_sample hdr-apple-gainmap.jpg "$libavif/apple_gainmap_v3.jpg" 071203c2a0ba754852e480a1cb70a1d45ea7476a25a79fc38e4433ea1aabb871
hdr_sample hdr-gainmap.avif "$libavif/seine_sdr_gainmap_srgb.avif" e0ebdb2f1f44c7d901e6b5f817eb2520eace344624cab0d57aaa929d05d6d971
hdr_sample hdr-pq.avif "$libavif/seine_hdr_srgb.avif" b4f440392ad8cdfd398aea0c8817c93d98cfd8cde381bbe430aceae0207e93c8
# From the test data of apple-hdr-heic (https://github.com/johncf/apple-hdr-heic), under its
# license (MIT, Copyright (c) 2024 John Charankattu). The commit is pinned and the file is
# checked against its SHA-256.
#   hdr-iphone-gainmap.heic  a HEIC from an iPhone 13 Pro, whose gain map is an auxiliary
#                            picture: how iPhones stored it before ISO 21496-1
hdr_sample hdr-iphone-gainmap.heic \
  "https://media.githubusercontent.com/media/johncf/apple-hdr-heic/e64716c29abc91a3b40543d7c47fb0f526608982/tests/data/hdr-sample.heic" \
  c690fa4ecc6ae71ee9a926de869a95835a5d41e764ccbc200647e2fb9d0800cc
# A HEIC with a gain map, as current phones store HDR photos: made from the JPEG above with
# macOS's own frameworks.
if command -v swift >/dev/null && [ "$(uname)" = Darwin ] && [ -f hdr-ultrahdr.jpg ]; then
  swift "$root/scripts/gainmap-heic.swift" hdr-ultrahdr.jpg hdr-gainmap.heic \
    || echo "  skipped: could not make the HEIC with a gain map"
else
  echo "  skipped: the HEIC with a gain map needs Swift (macOS) and the JPEG above"
fi

echo "JPEGs with EXIF orientations 1-8 that all display the same upright picture"
# Upright, 800x600: red top-left, green top-right, blue bottom-left, yellow bottom-right corners.
corners="drawbox=x=0:y=0:w=100:h=100:c=red:t=fill,drawbox=x=iw-100:y=0:w=100:h=100:c=lime:t=fill"
corners="$corners,drawbox=x=0:y=ih-100:w=100:h=100:c=blue:t=fill,drawbox=x=iw-100:y=ih-100:w=100:h=100:c=yellow:t=fill"
# Stored pixels are the inverse of the transform each orientation asks a viewer to apply.
stored=(null hflip hflip,vflip vflip transpose=0 transpose=2 transpose=3 transpose=1)
for n in 1 2 3 4 5 6 7 8; do
  ff -f lavfi -i "testsrc2=size=800x600,$corners,${stored[n-1]}" -frames:v 1 -q:v 2 stored.jpg
  {
    head -c 2 stored.jpg # SOI
    # APP1 Exif segment, big-endian TIFF, one IFD entry: Orientation (0x0112), SHORT, value n.
    printf '\377\341\000\042Exif\000\000MM\000\052\000\000\000\010\000\001\001\022\000\003\000\000\000\001'
    printf "\\000\\$(printf '%03o' "$n")\\000\\000\\000\\000\\000\\000"
    tail -c +3 stored.jpg
  } >"image-orientation$n.jpg"
done
rm stored.jpg

echo "animated GIF: 480 px, 3 s"
ff -f lavfi -i "testsrc2=size=480x270:rate=15:duration=3" \
   -vf "split[a][b];[a]palettegen[p];[b][p]paletteuse" animation.gif

if command -v sips >/dev/null; then
  echo "HEIC and PDF (sips)"
  sips -s format heic image-photo.jpg --out image-photo.heic >/dev/null
  # Transparent: the alpha plane is a separate auxiliary image (its own tile grid).
  sips -s format heic image-alpha.png --out image-alpha.heic >/dev/null
  sips -s format pdf image-photo.jpg --out document.pdf >/dev/null
else
  echo "  skipped: HEIC and PDF need sips (macOS)"
fi

echo "minimal EPUB with two images"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/META-INF" "$tmp/OEBPS/images"
printf 'application/epub+zip' >"$tmp/mimetype"
cat >"$tmp/META-INF/container.xml" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>
XML
cat >"$tmp/OEBPS/content.opf" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">peraz-fixture</dc:identifier><dc:title>Sample</dc:title><dc:language>en</dc:language>
    <meta property="dcterms:modified">2026-01-01T00:00:00Z</meta>
  </metadata>
  <manifest>
    <item id="c1" href="chapter.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="i1" href="images/photo.jpg" media-type="image/jpeg"/>
    <item id="i2" href="images/alpha.png" media-type="image/png"/>
  </manifest>
  <spine><itemref idref="c1"/></spine>
</package>
XML
cat >"$tmp/OEBPS/chapter.xhtml" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Sample</title></head>
<body><nav epub:type="toc"><ol><li><a href="chapter.xhtml">Sample</a></li></ol></nav>
<img src="images/photo.jpg" alt=""/><img src="images/alpha.png" alt=""/></body></html>
XML
cp image-photo.jpg "$tmp/OEBPS/images/photo.jpg"
cp image-alpha.png "$tmp/OEBPS/images/alpha.png"
rm -f book.epub
(cd "$tmp" && zip -q -X -0 "$out/book.epub" mimetype && zip -q -X -r "$out/book.epub" META-INF OEBPS)

echo
ls -lh "$out"
