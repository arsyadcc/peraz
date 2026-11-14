//! JPEG XL output through libjxl's `cjxl`, and `djxl` to read results back. Like FFmpeg, both run
//! as separate processes.
//!
//! Lookup order for `cjxl`: `PERAZ_CJXL`, then the bundled folder next to the `peraz` executable
//! (the same `peraz-ffmpeg` folder FFmpeg is in), then `PATH`. `djxl` is looked up next to `cjxl`,
//! then on `PATH`.
//!
//! A JPEG can be recompressed losslessly: `cjxl` keeps the JPEG's coefficients, so the file can be
//! turned back into the exact same JPEG, and is typically about 20% smaller. Before that, the
//! JPEG's metadata is removed as for every other output (EXIF, XMP, comments); only the color
//! profile and the orientation are kept, the latter in an EXIF block holding nothing else.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use image::DynamicImage;
use serde::Serialize;

use crate::ffmpeg::{BUNDLED_DIR, bundled_dir, command, exe_name, find_on_path};
use crate::image::Decoded;

/// Environment variable that points Peraz at a specific `cjxl` binary.
pub const CJXL_ENV: &str = "PERAZ_CJXL";

/// What to say when `cjxl` is missing.
pub const NEEDS_CJXL: &str = "JPEG XL output needs cjxl from libjxl, which was not found. Install \
                              libjxl's tools, or point Peraz to cjxl with PERAZ_CJXL";

/// The libjxl tools Peraz runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JxlTools {
    pub cjxl: PathBuf,
    /// Reads JPEG XL back, for `--target-ssimulacra2`.
    pub djxl: Option<PathBuf>,
}

impl JxlTools {
    /// Finds the tools using the process environment. The result is cached for the process.
    pub fn locate() -> Option<&'static Self> {
        static TOOLS: OnceLock<Option<JxlTools>> = OnceLock::new();
        TOOLS
            .get_or_init(|| {
                Self::locate_with(
                    std::env::var_os(CJXL_ENV),
                    bundled_dir().as_deref(),
                    std::env::var_os("PATH").as_deref(),
                )
            })
            .as_ref()
    }

    /// Same as [`JxlTools::locate`], with the environment and the bundled folder passed in.
    pub fn locate_with(
        env_cjxl: Option<OsString>,
        bundled: Option<&Path>,
        path_var: Option<&OsStr>,
    ) -> Option<Self> {
        let cjxl = [
            env_cjxl.filter(|v| !v.is_empty()).map(PathBuf::from),
            bundled.map(|dir| dir.join(exe_name("cjxl"))),
        ]
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
        .or_else(|| find_on_path("cjxl", path_var))?;
        let sibling = cjxl.with_file_name(exe_name("djxl"));
        let djxl = if sibling.is_file() {
            Some(sibling)
        } else {
            find_on_path("djxl", path_var)
        };
        Some(Self { cjxl, djxl })
    }

    /// The libjxl version, from `cjxl --version` (e.g. `0.11.1`).
    pub fn version(&self) -> Option<String> {
        let output = command(&self.cjxl).arg("--version").output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let word = text.split_whitespace().nth(1)?;
        Some(word.trim_start_matches('v').to_owned())
    }

    /// The tools come from the bundled folder.
    pub fn is_bundled(&self) -> bool {
        self.cjxl
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == BUNDLED_DIR)
    }
}

fn tools() -> Result<&'static JxlTools, String> {
    JxlTools::locate().ok_or_else(|| NEEDS_CJXL.to_owned())
}

/// Encodes a picture at a `cjxl` quality setting (0–100, where 100 is lossless). The color
/// profile is kept.
pub fn encode(prepared: &Decoded, quality: u8) -> Result<Vec<u8>, String> {
    let tools = tools()?;
    let workdir = crate::compress::work_folder()?;
    let input = workdir.join("in.png");
    let output = workdir.join("out.jxl");
    let result = write_png(&prepared.image, prepared.icc.as_deref(), &input)
        .and_then(|()| run_cjxl(&tools.cjxl, &input, &output, &["-q", &quality.to_string()]));
    let _ = std::fs::remove_dir_all(&workdir);
    result
}

/// Recompresses a JPEG losslessly. `orientation` is its EXIF orientation (1–8), which is kept.
pub fn recompress_jpeg(jpeg: &[u8], orientation: u8) -> Result<Vec<u8>, String> {
    let tools = tools()?;
    let stripped = strip_jpeg_metadata(jpeg, orientation)
        .ok_or("the JPEG's structure could not be read".to_owned())?;
    let workdir = crate::compress::work_folder()?;
    let input = workdir.join("in.jpg");
    let output = workdir.join("out.jxl");
    let result = std::fs::write(&input, stripped)
        .map_err(|e| format!("cannot write a work file: {e}"))
        .and_then(|()| run_cjxl(&tools.cjxl, &input, &output, &["--lossless_jpeg=1"]));
    let _ = std::fs::remove_dir_all(&workdir);
    result
}

/// Decodes a JPEG XL file Peraz just made, with `djxl`.
pub fn decode(bytes: &[u8]) -> Result<DynamicImage, String> {
    let djxl = tools()?.djxl.as_ref().ok_or(
        "--target-ssimulacra2 with JPEG XL output needs djxl from libjxl, to read each try back",
    )?;
    let workdir = crate::compress::work_folder()?;
    let input = workdir.join("in.jxl");
    let output = workdir.join("out.png");
    let result = std::fs::write(&input, bytes)
        .map_err(|e| format!("cannot write a work file: {e}"))
        .and_then(|()| {
            let status = command(djxl)
                .arg(&input)
                .arg(&output)
                .arg("--quiet")
                .output()
                .map_err(|e| format!("cannot run {}: {e}", djxl.display()))?;
            if !status.status.success() {
                return Err(format!(
                    "djxl failed: {}",
                    String::from_utf8_lossy(&status.stderr).trim()
                ));
            }
            image::open(&output).map_err(|e| format!("cannot read back: {e}"))
        });
    let _ = std::fs::remove_dir_all(&workdir);
    result
}

fn run_cjxl(cjxl: &Path, input: &Path, output: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let result = command(cjxl)
        .arg(input)
        .arg(output)
        .args(args)
        .arg("--quiet")
        .output()
        .map_err(|e| format!("cannot run {}: {e}", cjxl.display()))?;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let line = stderr.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!("cjxl failed ({}): {}", result.status, line.trim()));
    }
    std::fs::read(output).map_err(|e| format!("cannot read cjxl's output: {e}"))
}

/// Writes the picture as a PNG for `cjxl`, quickly (it is read once), with its color profile.
fn write_png(image: &DynamicImage, icc: Option<&[u8]>, path: &Path) -> Result<(), String> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::{ColorType, ImageEncoder};
    let color = image.color();
    let deep = color.bytes_per_pixel() / color.channel_count() > 1;
    let pixels = match (deep, color.has_alpha()) {
        (false, false) => DynamicImage::ImageRgb8(image.to_rgb8()),
        (false, true) => DynamicImage::ImageRgba8(image.to_rgba8()),
        (true, false) => DynamicImage::ImageRgb16(image.to_rgb16()),
        (true, true) => DynamicImage::ImageRgba16(image.to_rgba16()),
    };
    let file = std::fs::File::create(path).map_err(|e| format!("cannot write a work file: {e}"))?;
    let mut encoder = PngEncoder::new_with_quality(
        std::io::BufWriter::new(file),
        CompressionType::Fast,
        FilterType::NoFilter,
    );
    if let Some(icc) = icc {
        // A profile the PNG cannot hold is dropped, as when it is missing.
        let _ = encoder.set_icc_profile(icc.to_vec());
    }
    let color: ColorType = pixels.color();
    encoder
        .write_image(
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            color.into(),
        )
        .map_err(|e| format!("cannot write a work file: {e}"))
}

/// A JPEG with its metadata removed: APP0 (JFIF), the ICC profile (APP2), and APP14 (Adobe,
/// which tells how colors are stored) are kept, every other `APPn` segment and comments are
/// dropped. When `orientation` is not 1, an EXIF block holding only it is added. `None` when the
/// segments before the image data cannot be followed.
fn strip_jpeg_metadata(jpeg: &[u8], orientation: u8) -> Option<Vec<u8>> {
    if jpeg.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut out = Vec::with_capacity(jpeg.len());
    out.extend_from_slice(&[0xFF, 0xD8]);
    // The EXIF block goes after APP0 (JFIF), which must come first, and before everything else.
    let mut exif = (orientation != 1).then(|| orientation_exif(orientation));
    let mut at = 2;
    loop {
        if *jpeg.get(at)? != 0xFF {
            return None;
        }
        let marker = *jpeg.get(at + 1)?;
        if marker == 0xFF {
            at += 1; // fill byte
            continue;
        }
        if marker != 0xE0
            && let Some(exif) = exif.take()
        {
            out.extend_from_slice(&exif);
        }
        // Start of scan: the rest is image data.
        if marker == 0xDA {
            out.extend_from_slice(jpeg.get(at..)?);
            return Some(out);
        }
        let length = usize::from(u16::from_be_bytes([*jpeg.get(at + 2)?, *jpeg.get(at + 3)?]));
        let segment = jpeg.get(at..at + 2 + length)?;
        let body = segment.get(4..)?;
        let keep = match marker {
            0xE2 => body.starts_with(b"ICC_PROFILE\0"),
            0xEE => body.starts_with(b"Adobe"),
            0xE1 | 0xE3..=0xED | 0xEF | 0xFE => false,
            // APP0 (JFIF), tables, frame headers.
            _ => true,
        };
        if keep {
            out.extend_from_slice(segment);
        }
        at += 2 + length;
    }
}

/// An APP1 EXIF segment holding only an orientation tag.
fn orientation_exif(orientation: u8) -> Vec<u8> {
    let mut exif = vec![0xFF, 0xE1, 0, 34];
    exif.extend_from_slice(b"Exif\0\0");
    exif.extend_from_slice(b"MM\0\x2A\0\0\0\x08"); // big-endian TIFF header, IFD at 8
    exif.extend_from_slice(&[0, 1]); // one entry
    exif.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, orientation, 0, 0]); // SHORT
    exif.extend_from_slice(&[0, 0, 0, 0]); // no next IFD
    exif
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
        let length = u16::try_from(body.len() + 2).unwrap().to_be_bytes();
        [&[0xFF, marker], &length[..], body].concat()
    }

    #[test]
    fn jpeg_metadata_is_removed_but_profile_and_orientation_stay() {
        let jfif = segment(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        let exif = segment(0xE1, b"Exif\0\0secret GPS");
        let xmp = segment(0xE1, b"http://ns.adobe.com/xap/1.0/\0<x/>");
        let icc = segment(0xE2, b"ICC_PROFILE\0\x01\x01profile");
        let comment = segment(0xFE, b"made on my phone");
        let dqt = segment(0xDB, &[0; 65]);
        let scan = [0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9];
        let jpeg = [
            &[0xFF, 0xD8][..],
            &jfif,
            &exif,
            &xmp,
            &icc,
            &comment,
            &dqt,
            &scan,
        ]
        .concat();

        let plain = strip_jpeg_metadata(&jpeg, 1).unwrap();
        assert_eq!(
            plain,
            [&[0xFF, 0xD8][..], &jfif, &icc, &dqt, &scan].concat()
        );

        let turned = strip_jpeg_metadata(&jpeg, 6).unwrap();
        let exif = orientation_exif(6);
        assert_eq!(
            turned,
            [&[0xFF, 0xD8][..], &jfif, &exif, &icc, &dqt, &scan].concat()
        );
        assert_eq!(usize::from(exif[3]) + 2, exif.len());
        assert!(!turned.windows(6).any(|w| w == b"secret"));

        assert_eq!(strip_jpeg_metadata(b"not a jpeg", 1), None);
        assert_eq!(strip_jpeg_metadata(&jpeg[..20], 1), None);
    }

    #[test]
    fn finds_cjxl_and_djxl_side_by_side() {
        let dir = std::env::temp_dir().join(format!("peraz-jxl-locate-{}", std::process::id()));
        let bundled = dir.join(BUNDLED_DIR);
        std::fs::create_dir_all(&bundled).unwrap();
        assert_eq!(JxlTools::locate_with(None, Some(&bundled), None), None);
        for tool in ["cjxl", "djxl"] {
            std::fs::write(bundled.join(exe_name(tool)), b"").unwrap();
        }
        let tools = JxlTools::locate_with(None, Some(&bundled), None).unwrap();
        assert_eq!(tools.cjxl, bundled.join(exe_name("cjxl")));
        assert_eq!(tools.djxl, Some(bundled.join(exe_name("djxl"))));
        assert!(tools.is_bundled());
        // The environment variable wins.
        let other = dir.join(exe_name("cjxl"));
        std::fs::write(&other, b"").unwrap();
        let tools =
            JxlTools::locate_with(Some(other.clone().into()), Some(&bundled), None).unwrap();
        assert_eq!(tools.cjxl, other);
        assert!(!tools.is_bundled());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
