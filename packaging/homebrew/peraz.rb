# Homebrew formula that builds Peraz from source, for the tap's stable releases and for
# homebrew-core. For stable releases, the Homebrew tap workflow fills in `url` and `sha256`.
class Peraz < Formula
  desc "Compress videos, images, GIFs, PDFs, and EPUBs with results you can trust"
  homepage "https://github.com/arsyadcc/peraz"
  url "https://github.com/arsyadcc/peraz/archive/refs/tags/v0.0.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "GPL-3.0-or-later"
  head "https://github.com/arsyadcc/peraz.git", branch: "main"

  depends_on "pkgconf" => :build
  depends_on "rust" => :build
  depends_on "ffmpeg"
  depends_on "libdeflate"

  on_intel do
    # The SIMD JPEG and AV1 code is assembled with nasm on x86_64 only.
    depends_on "nasm" => :build
  end

  def install
    system "cargo", "install", "--features", "simd,system-libs", *std_cargo_args(path: "crates/peraz")
    generate_completions_from_executable(bin/"peraz", "completions")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/peraz --version")
    assert_match "ffmpeg", shell_output("#{bin}/peraz doctor")

    system formula_opt_bin("ffmpeg")/"ffmpeg", "-loglevel", "error", "-f", "lavfi",
           "-i", "testsrc=size=320x240", "-frames:v", "1", testpath/"in.png"
    system bin/"peraz", "compress", "--format", "jpeg", "--output", testpath/"out.jpg", testpath/"in.png"
    assert_equal "\xFF\xD8".b, (testpath/"out.jpg").binread(2)
  end
end
