//! SVG minification, end to end: every sample is rendered before and after, and the pictures are
//! compared. SVG is handled as text, so these tests need neither FFmpeg nor the fixtures.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use peraz_core::{
    ExistingOutput, FailReason, ImageEncoding, ImageFormat, Job, KeepReason, MediaKind, Outcome,
    OutputTarget, Plan, Quality, Recipe, Report,
};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::Capabilities;
use peraz_engine::probe::probe_without_ffmpeg;
use peraz_engine::svg::minify;

/// A drawing as an editor exports it: editor data, comments, long numbers, gradients and clips
/// referred to by ID, and a style sheet that selects by class and by ID.
const DRAWING: &str = r##"<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<!-- Created with a drawing program -->
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<svg
   xmlns:dc="http://purl.org/dc/elements/1.1/"
   xmlns:cc="http://creativecommons.org/ns#"
   xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
   xmlns="http://www.w3.org/2000/svg"
   xmlns:xlink="http://www.w3.org/1999/xlink"
   xmlns:sodipodi="http://sodipodi.sourceforge.net/DTD/sodipodi-0.dtd"
   xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape"
   width="320.00000"
   height="240.00000"
   viewBox="0 0 320.000 240.000"
   id="svg8"
   version="1.1"
   inkscape:version="1.2 (dc2aedaf03, 2022-05-15)"
   sodipodi:docname="drawing.svg">
  <style type="text/css"><![CDATA[
    .outline { stroke: #223344; stroke-width: 2.5; fill: none }
    #sun { fill: url(#glow) }
  ]]></style>
  <defs
     id="defs2">
    <linearGradient
       inkscape:collect="always"
       id="sky">
      <stop
         style="stop-color:#88bbee;stop-opacity:1.0000000"
         offset="0.00000000"
         id="stop1" />
      <stop
         style="stop-color:#eef6ff;stop-opacity:1"
         offset="1.00000000"
         id="stop2" />
    </linearGradient>
    <linearGradient
       xlink:href="#sky"
       id="skyFill"
       x1="160.00000"
       y1="0.0000000"
       x2="160.00000"
       y2="240.00000"
       gradientUnits="userSpaceOnUse"
       gradientTransform="matrix(1.0000000, 0.0000000, 0.0000000, 1.0000000, 0.0000000, 0.0000000)" />
    <radialGradient id="glow" cx="0.50000" cy="0.50000" r="0.500000">
      <stop offset="0.200000" stop-color="#fff3a0" />
      <stop offset="1.000000" stop-color="#ffb347" stop-opacity="0.850000" />
    </radialGradient>
    <clipPath id="frame">
      <rect x="8.0000000" y="8.0000000" width="304.00000" height="224.00000" rx="12.500000" />
    </clipPath>
  </defs>
  <sodipodi:namedview
     id="base"
     pagecolor="#ffffff"
     inkscape:zoom="1.4"
     inkscape:current-layer="layer1" />
  <metadata
     id="metadata5">
    <rdf:RDF>
      <cc:Work
         rdf:about="">
        <dc:format>image/svg+xml</dc:format>
        <dc:title>Drawing</dc:title>
      </cc:Work>
    </rdf:RDF>
  </metadata>
  <g
     inkscape:label="Layer 1"
     inkscape:groupmode="layer"
     id="layer1"
     clip-path="url(#frame)">
    <rect
       style="fill:url(#skyFill);stroke:none"
       id="rect10"
       width="320.00000"
       height="240.00000"
       x="0.0000000"
       y="0.0000000" />
    <circle id="sun" cx="238.41739" cy="62.183746" r="28.345127" />
    <path
       class="outline"
       d="M 20.123456,200.654321 C 60.111111,140.222222 90.333333,150.444444 120.555555,180.666666 S 180.777777,120.888888 220.999999,170.101010 L 300.000000 , 190.500000"
       id="path12"
       inkscape:connector-curvature="0"
       sodipodi:nodetypes="cscc" />
    <path
       style="fill:#3a7d44;fill-opacity:0.90000000"
       d="m 0.0000000,240.00000 0.0000000,-40.000000 c 40.000000,-25.000000 80.000000,-10.000000 120.00000,-5.0000000 c 50.000000,6.2500000 100.00000,-30.000000 200.00000,-12.500000 l 0.0000000,57.500000 z"
       id="hill" />
    <g
       transform="translate( 60.000000 , 150.00000 ) rotate( -12.500000 ) scale( 1.2500000 )"
       id="tree">
      <rect x="-3.0000000" y="0.0000000" width="6.0000000" height="22.000000" fill="#6b4226" />
      <polygon
         points="0.0000000,-34.000000 -18.000000,4.0000000 18.000000,4.0000000"
         fill="#2e6b34" />
      <polyline
         class="outline"
         points="-18.000000,4.0000000  0.0000000,-34.000000  18.000000,4.0000000" />
    </g>
    <path
       d="M 250.00000,150.00000 a 20.000000,20.000000 0 1 1 30.000000,10.000000 A 12.500000 7.2500000 30.000000 0 0 250.00000 150.00000 Z"
       fill="#d94f4f"
       stroke="#7a1f1f"
       stroke-width="1.5000000"
       stroke-opacity="0.75000000" />
    <use xlink:href="#sun" transform="translate(-180.00000, 20.000000) scale(0.50000000)" opacity="0.50000000" />
  </g>
</svg>
"##;

/// A small icon: coordinates with few digits, arcs written without separators, and relative
/// commands, as icon sets ship them.
const ICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#1b1f23" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">
  <!-- bell -->
  <path d="M6 8a6 6 0 0112 0c0 7 3 9 3 9H3s3-2 3-9"/>
  <path d="M10.3 21a1.94 1.94 0 003.4 0"/>
  <circle cx="18.250" cy="5.750" r="2.250" fill="#e5484d" stroke="none"/>
  <path d="M2.5.5l1 1-1 1" transform="translate(0.125 0.125)"/>
</svg>
"##;

/// A chart drawn at a tiny scale and enlarged by a transform, so its numbers are all below 1.
const SCALED: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="120" viewBox="0 0 200 120">
  <g transform="matrix(1000 0 0 1000 10.000 10.000)">
    <path d="M0.00512345,0.09123456 L0.03254321,0.04765432 L0.06187654,0.07345678 L0.09876543,0.01234567 L0.14345678,0.05567891 L0.17654321,0.02123456" fill="none" stroke="#3366cc" stroke-width="0.00212345"/>
    <rect x="0.00100000" y="0.00100000" width="0.17800000" height="0.09800000" fill="none" stroke="#999999" stroke-width="0.00050000"/>
    <circle cx="0.09876543" cy="0.01234567" r="0.00423456" fill="#cc3333"/>
  </g>
</svg>
"##;

/// Renders an SVG at `scale` into RGBA pixels.
fn render(svg: &str, scale: f32) -> (u32, u32, Vec<u8>) {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).expect("the SVG parses");
    let size = tree.size().to_int_size().scale_by(scale).unwrap();
    let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height()).unwrap();
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    (size.width(), size.height(), pixmap.take())
}

/// How two renderings differ: the largest difference of any channel, and the share of pixels
/// that differ by more than `noticeable`.
fn difference(a: &(u32, u32, Vec<u8>), b: &(u32, u32, Vec<u8>), noticeable: u8) -> (u8, f64) {
    assert_eq!((a.0, a.1), (b.0, b.1), "the size is the same");
    let mut largest = 0;
    let mut differing = 0usize;
    for (pa, pb) in a.2.as_chunks::<4>().0.iter().zip(b.2.as_chunks::<4>().0) {
        let worst = pa
            .iter()
            .zip(pb)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0);
        largest = largest.max(worst);
        differing += usize::from(worst > noticeable);
    }
    #[allow(clippy::cast_precision_loss)]
    let share = differing as f64 / (a.2.len() / 4) as f64;
    (largest, share)
}

#[test]
fn minified_drawings_render_the_same() {
    for (name, svg) in [("drawing", DRAWING), ("icon", ICON), ("scaled", SCALED)] {
        // Rendered larger than drawn, so small shifts would show.
        let before = render(svg, 3.0);
        assert!(
            before.2.iter().any(|b| *b != 0),
            "{name}: something is drawn"
        );

        // Without rounding, the picture is the same pixel for pixel.
        let exact = minify(svg, None).unwrap();
        assert!(exact.len() < svg.len(), "{name}: {} bytes", exact.len());
        assert_eq!(
            difference(&before, &render(&exact, 3.0), 0),
            (0, 0.0),
            "{name}"
        );

        // Rounded numbers move edges by a fraction of a pixel: only a few pixels along the
        // edges change noticeably, more of them at the lower levels.
        let mut previous = exact.len();
        for (decimals, most) in [(4, 0.001), (3, 0.005), (2, 0.03), (1, 0.05)] {
            let rounded = minify(svg, Some(decimals)).unwrap();
            assert!(rounded.len() <= previous, "{name}: {decimals} decimals");
            previous = rounded.len();
            let (_, differing) = difference(&before, &render(&rounded, 3.0), 32);
            assert!(
                differing <= most,
                "{name} at {decimals} decimals: {differing} of the pixels differ"
            );
        }

        // Minifying again changes nothing.
        assert_eq!(minify(&exact, None).unwrap(), exact, "{name}");
    }
    // The editor's data is where most of the bytes were.
    let minified = minify(DRAWING, Some(3)).unwrap();
    assert!(minified.len() * 2 < DRAWING.len(), "{}", minified.len());
    for gone in [
        "inkscape", "sodipodi", "metadata", "<!--", "<?xml", "DOCTYPE",
    ] {
        assert!(!minified.contains(gone), "{gone} is removed");
    }
    for kept in [
        "id=\"sun\"",
        "#sun { fill: url(#glow) }",
        "class=\"outline\"",
        "viewBox=\"0 0 320.000 240.000\"",
        "xlink:href=\"#sky\"",
    ] {
        assert!(minified.contains(kept), "{kept} is kept");
    }
}

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(test: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-svg-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

fn run(input: &Path, output: OutputTarget, recipe: Recipe) -> Report {
    let job = Job {
        input: input.to_owned(),
        output,
        recipe,
        existing: ExistingOutput::Fail,
        recompress: true,
    };
    compress(
        None,
        &Capabilities::default(),
        &job,
        &mut |_| {},
        &NOT_CANCELLED,
    )
}

#[test]
fn an_svg_file_is_minified_and_stays_an_svg() {
    let dir = dir("file");
    let input = dir.0.join("drawing.svg");
    std::fs::write(&input, DRAWING).unwrap();
    let info = probe_without_ffmpeg(&input).unwrap().unwrap();
    assert_eq!(
        (info.kind, info.container.as_str()),
        (MediaKind::Image, "svg")
    );
    let picture = info.video.unwrap();
    assert_eq!(
        (picture.codec.as_str(), picture.width, picture.height),
        ("svg", 320, 240)
    );

    let report = run(
        &input,
        OutputTarget::Folder(dir.0.clone()),
        Recipe::default(),
    );
    let Outcome::Compressed {
        output,
        output_bytes,
    } = &report.outcome
    else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert_eq!(output.file_name().unwrap(), "drawing-1.svg");
    assert!(*output_bytes * 2 < report.input_bytes.unwrap());
    assert_eq!(report.kind, Some(MediaKind::Image));
    assert_eq!(report.encoder.as_deref(), Some("SVG minifier"));
    let Some(Plan::Image(plan)) = &report.plan else {
        panic!("{:?}", report.plan)
    };
    assert_eq!(plan.encoding, ImageEncoding::Svg { decimals: Some(3) });
    let minified = std::fs::read_to_string(output).unwrap();
    assert_eq!(minified, minify(DRAWING, Some(3)).unwrap());

    // Minified once, there is nothing left to remove: the original is kept.
    let report = run(
        output,
        OutputTarget::File(dir.0.join("again.svg")),
        Recipe::default(),
    );
    assert!(
        matches!(
            report.outcome,
            Outcome::KeptOriginal(KeepReason::NotSmaller { .. })
        ),
        "{:?}",
        report.outcome
    );

    // An SVG has no pixels to turn into another format: it is skipped, with the reason.
    let mut recipe = Recipe::default();
    recipe.image.format = ImageFormat::Webp;
    let report = run(&input, OutputTarget::File(dir.0.join("out.webp")), recipe);
    let Outcome::Skipped { message, .. } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(message.contains("not converted to webp"), "{message}");
    assert!(!dir.0.join("out.webp").exists());
}

#[test]
fn a_gzipped_svg_stays_gzipped() {
    let dir = dir("svgz");
    let input = dir.0.join("drawing.svgz");
    // Packed quickly, as an export would be.
    {
        use std::io::Write as _;
        let file = std::fs::File::create(&input).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        encoder.write_all(DRAWING.as_bytes()).unwrap();
        encoder.finish().unwrap();
    }
    let info = probe_without_ffmpeg(&input).unwrap().unwrap();
    assert_eq!(
        (info.kind, info.container.as_str()),
        (MediaKind::Image, "svgz")
    );
    let report = run(
        &input,
        OutputTarget::Folder(dir.0.clone()),
        Recipe {
            quality: Quality::Highest,
            ..Recipe::default()
        },
    );
    let Outcome::Compressed {
        output,
        output_bytes,
    } = &report.outcome
    else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert_eq!(output.file_name().unwrap(), "drawing-1.svgz");
    assert!(*output_bytes < report.input_bytes.unwrap());
    assert_eq!(report.encoder.as_deref(), Some("SVG minifier + gzip"));
    let (text, gzip) = peraz_engine::svg::read(output).unwrap();
    assert!(gzip);
    assert_eq!(text, minify(DRAWING, None).unwrap());
}

#[test]
fn files_that_are_not_minified_say_why() {
    let dir = dir("refused");
    // Declared in another encoding: skipped, never rewritten as if it were UTF-8.
    let latin = dir.0.join("latin.svg");
    std::fs::write(
        &latin,
        "<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    )
    .unwrap();
    let report = run(
        &latin,
        OutputTarget::File(dir.0.join("latin-1.svg")),
        Recipe::default(),
    );
    let Outcome::Skipped { message, .. } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(message.contains("only UTF-8"), "{message}");

    // Broken XML fails, and nothing is written.
    let broken = dir.0.join("broken.svg");
    std::fs::write(
        &broken,
        "<svg xmlns=\"http://www.w3.org/2000/svg\"><g><rect/></svg>",
    )
    .unwrap();
    let report = run(
        &broken,
        OutputTarget::File(dir.0.join("broken-1.svg")),
        Recipe::default(),
    );
    let Outcome::Failed {
        reason: FailReason::CannotDecode,
        message,
    } = &report.outcome
    else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    assert!(message.contains("not valid XML"), "{message}");
    assert!(!dir.0.join("broken-1.svg").exists());

    // Other XML and text files are not SVG.
    for (name, text) in [
        ("page.html", "<html><body><svg></svg></body></html>"),
        ("notes.txt", "svg is a vector format"),
        ("feed.xml", "<?xml version=\"1.0\"?><rss/>"),
    ] {
        let path = dir.0.join(name);
        std::fs::write(&path, text).unwrap();
        assert!(peraz_engine::svg::sniff(&path).unwrap().is_none(), "{name}");
    }
}
