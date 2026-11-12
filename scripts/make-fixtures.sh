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
