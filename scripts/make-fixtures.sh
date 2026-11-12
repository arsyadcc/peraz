#!/usr/bin/env bash
# Regenerates test samples into ./fixtures (not committed).
# Requires: ffmpeg, zip. On macOS, sips creates the HEIC and PDF samples; elsewhere they are skipped.
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

echo "images: opaque PNG, transparent PNG, JPEG"
ff -f lavfi -i "testsrc2=size=1600x1200,format=rgb24" -frames:v 1 image-opaque.png
ff -f lavfi -i "testsrc2=size=1200x900,format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lt(hypot(X-W/2,Y-H/2),H/3),255,X*255/W)'" \
   -frames:v 1 image-alpha.png
ff -i image-opaque.png -q:v 2 image-photo.jpg

echo "animated GIF: 480 px, 3 s"
ff -f lavfi -i "testsrc2=size=480x270:rate=15:duration=3" \
   -vf "split[a][b];[a]palettegen[p];[b][p]paletteuse" animation.gif

if command -v sips >/dev/null; then
  echo "HEIC and PDF (sips)"
  sips -s format heic image-photo.jpg --out image-photo.heic >/dev/null
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
