#!/usr/bin/env bash
# Writes the winget manifests for a Peraz release, for a pull request to microsoft/winget-pkgs.
#
#   packaging/winget/manifests.sh v0.1.0 [out-dir]
#
# The release must be published: the installer's checksum and the release date come from GitHub.
# The files land in <out-dir>/manifests/a/Arsyadcc/Peraz/<version>/, the layout winget-pkgs uses.
set -euo pipefail

tag=${1:?usage: manifests.sh <tag> [out-dir]}
out=${2:-winget}
version=${tag#v}
repo=https://github.com/arsyadcc/peraz
id=Arsyadcc.Peraz
asset=peraz-x86_64-pc-windows-msvc.zip
url="$repo/releases/download/$tag/$asset"

sha=$(curl -fsSL "$url.sha256" | cut -d' ' -f1 | tr '[:lower:]' '[:upper:]')
[[ $sha =~ ^[0-9A-F]{64}$ ]] || { echo "No checksum for $asset in $tag" >&2; exit 1; }
date=$(curl -fsSL "https://api.github.com/repos/arsyadcc/peraz/releases/tags/$tag" |
  sed -n 's/^  "published_at": "\([0-9-]*\)T.*/\1/p')
[[ $date =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || { echo "No release date for $tag" >&2; exit 1; }

dir="$out/manifests/a/Arsyadcc/Peraz/$version"
mkdir -p "$dir"

cat > "$dir/$id.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.1.12.0.schema.json

PackageIdentifier: $id
PackageVersion: $version
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.12.0
EOF

# Peraz runs FFmpeg as a separate program for videos, GIFs, HEIC, and AVIF, so FFmpeg is installed
# with it, as Homebrew does. The release is built with a static C runtime: no Visual C++
# redistributable is needed.
cat > "$dir/$id.installer.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.1.12.0.schema.json

PackageIdentifier: $id
PackageVersion: $version
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
- RelativeFilePath: peraz.exe
  PortableCommandAlias: peraz
Dependencies:
  PackageDependencies:
  - PackageIdentifier: Gyan.FFmpeg
ReleaseDate: $date
Installers:
- Architecture: x64
  InstallerUrl: $url
  InstallerSha256: $sha
ManifestType: installer
ManifestVersion: 1.12.0
EOF

cat > "$dir/$id.locale.en-US.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.1.12.0.schema.json

PackageIdentifier: $id
PackageVersion: $version
PackageLocale: en-US
Publisher: Arsyad
PublisherUrl: https://github.com/arsyadcc
PublisherSupportUrl: $repo/issues
Author: Arsyad
PackageName: Peraz
PackageUrl: $repo
License: GPL-3.0-or-later
LicenseUrl: $repo/blob/$tag/LICENSE
ShortDescription: Compress videos, images, GIFs, PDFs, and EPUBs with results you can trust
Description: |-
  Peraz is a command-line tool that makes videos, images, GIFs, PDFs, and EPUBs smaller while
  keeping them looking the same. A result that is not smaller is not kept, the original is never
  changed, and --json output and exit codes make it easy to script.
Moniker: peraz
Tags:
- cli
- compression
- ffmpeg
- gif
- image
- jpeg
- pdf
- png
- video
- webp
ReleaseNotesUrl: $repo/releases/tag/$tag
Documentations:
- DocumentLabel: Command-line reference
  DocumentUrl: $repo/blob/$tag/docs/cli.md
ManifestType: defaultLocale
ManifestVersion: 1.12.0
EOF

echo "$dir"
