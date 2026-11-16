//! PDF details: pictures whose transparency depends on exact values or sizes, pictures that
//! expand beyond reason, and documents that must not be rewritten (signed) or must keep their
//! structure (PDF/A) or their protection (permission restrictions). The inputs are made here
//! with `lopdf`; nothing needs FFmpeg.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use lopdf::content::{Content, Operation};
use lopdf::encryption::crypt_filters::{Aes128CryptFilter, Aes256CryptFilter, CryptFilter};
use lopdf::{
    Document, EncryptionState, EncryptionVersion, Object, ObjectId, Permissions, Stream,
    StringFormat, dictionary,
};
use peraz_core::{ExistingOutput, Job, Outcome, OutputTarget, Recipe, Report, SkipReason};
use peraz_engine::compress::compress;
use peraz_engine::ffmpeg::Capabilities;

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(test: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!("peraz-pdf-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}

static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

fn run(input: &Path, output: &Path) -> Report {
    let job = Job {
        input: input.to_owned(),
        output: OutputTarget::File(output.to_owned()),
        recipe: Recipe::default(),
        existing: ExistingOutput::Replace,
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

/// A one-page PDF being put together: pictures are added, then the page that draws them.
struct Sample {
    doc: Document,
    pictures: Vec<ObjectId>,
}

impl Sample {
    fn new(version: &str) -> Self {
        Self {
            doc: Document::with_version(version),
            pictures: Vec::new(),
        }
    }

    /// Adds a picture that the page draws.
    fn picture(&mut self, stream: Stream) -> ObjectId {
        let id = self.doc.add_object(stream);
        self.pictures.push(id);
        id
    }

    /// Adds the page, and returns the document with the id of its catalog.
    fn finish(mut self) -> (Document, ObjectId) {
        let pages = self.doc.new_object_id();
        // Each picture in a band of its own, down the page.
        let mut operations = Vec::new();
        let mut names = lopdf::Dictionary::new();
        let band = 842 / i64::try_from(self.pictures.len().max(1)).unwrap();
        for (index, id) in self.pictures.iter().enumerate() {
            let name = format!("Im{index}");
            let bottom = 842 - band * (i64::try_from(index).unwrap() + 1);
            operations.push(Operation::new("q", vec![]));
            operations.push(Operation::new(
                "cm",
                vec![
                    595.into(),
                    0.into(),
                    0.into(),
                    band.into(),
                    0.into(),
                    bottom.into(),
                ],
            ));
            operations.push(Operation::new("Do", vec![name.as_str().into()]));
            operations.push(Operation::new("Q", vec![]));
            names.set(name, *id);
        }
        let content = Content { operations }.encode().unwrap();
        let content = self.doc.add_object(Stream::new(dictionary! {}, content));
        let page = self.doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "Contents" => content,
            "Resources" => dictionary! { "XObject" => names },
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        self.doc.objects.insert(
            pages,
            Object::Dictionary(
                dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
            ),
        );
        let catalog = self
            .doc
            .add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        self.doc.trailer.set("Root", catalog);
        self.doc.trailer.set(
            "ID",
            vec![
                Object::String(b"peraz test file!".to_vec(), StringFormat::Hexadecimal),
                Object::String(b"peraz test file!".to_vec(), StringFormat::Hexadecimal),
            ],
        );
        (self.doc, catalog)
    }
}

/// An uncompressed picture in `space` (`DeviceRGB` or `DeviceGray`).
fn picture(width: u32, height: u32, space: &str, pixels: Vec<u8>) -> Stream {
    Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image",
            "Width" => i64::from(width), "Height" => i64::from(height),
            "ColorSpace" => space, "BitsPerComponent" => 8,
        },
        pixels,
    )
    .with_compression(false)
}

/// A smooth RGB picture: one that compresses well as a JPEG.
#[allow(clippy::cast_possible_truncation)]
fn gradient(width: u32, height: u32) -> Vec<u8> {
    (0..height)
        .flat_map(|y| {
            (0..width).flat_map(move |x| {
                [
                    (x * 255 / width) as u8,
                    (y * 255 / height) as u8,
                    ((x + y) % 256) as u8,
                ]
            })
        })
        .collect()
}

/// Two flat colors side by side: one that a palette holds exactly.
fn flat(width: u32, height: u32) -> Vec<u8> {
    (0..height)
        .flat_map(|_| {
            (0..width).flat_map(move |x| {
                if x < width / 2 {
                    [200, 30, 30]
                } else {
                    [20, 20, 160]
                }
            })
        })
        .collect()
}

fn save(doc: &mut Document, path: &Path) {
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// The pictures of a document: every image stream, in the order of their ids.
fn pictures(doc: &Document) -> Vec<&Stream> {
    doc.objects
        .values()
        .filter_map(|object| object.as_stream().ok())
        .filter(|stream| {
            stream.dict.get(b"Subtype").and_then(Object::as_name).ok() == Some(b"Image")
        })
        .collect()
}

fn number(stream: &Stream, key: &[u8]) -> i64 {
    stream.dict.get(key).and_then(Object::as_i64).unwrap()
}

fn compressed(report: &Report) -> PathBuf {
    let Outcome::Compressed { output, .. } = &report.outcome else {
        panic!("unexpected outcome: {:?}", report.outcome)
    };
    output.clone()
}

#[test]
fn a_picture_with_a_color_key_stays_exact() {
    let dir = dir("key");
    let mut sample = Sample::new("1.7");
    sample.picture(picture(400, 300, "DeviceRGB", gradient(400, 300)));
    // One of the two colors of this graphic is see-through: every pixel that is exactly
    // (200, 30, 30). A palette or a JPEG would move those values.
    let mut keyed = picture(400, 300, "DeviceRGB", flat(400, 300));
    keyed.dict.set(
        "Mask",
        vec![
            200.into(),
            200.into(),
            30.into(),
            30.into(),
            30.into(),
            30.into(),
        ],
    );
    sample.picture(keyed);
    let (mut doc, _) = sample.finish();
    let input = dir.0.join("key.pdf");
    save(&mut doc, &input);

    let report = run(&input, &dir.0.join("key-1.pdf"));
    let output = Document::load(compressed(&report)).unwrap();
    let after = pictures(&output);
    let keyed = after
        .iter()
        .find(|stream| stream.dict.has(b"Mask"))
        .expect("the picture keeps its color key");
    assert_eq!(
        keyed.dict.get(b"ColorSpace").and_then(Object::as_name).ok(),
        Some(b"DeviceRGB".as_slice()),
        "no palette"
    );
    let pixels = if keyed.dict.has(b"Filter") {
        keyed.decompressed_content().unwrap()
    } else {
        keyed.content.clone()
    };
    assert_eq!(pixels, flat(400, 300), "every value is kept");
    // The other picture is compressed as usual.
    let stats = report.document.expect("document statistics");
    assert_eq!((stats.images, stats.recompressed, stats.skipped), (2, 1, 1));
}

#[test]
fn a_picture_blended_with_a_matte_keeps_the_size_of_its_mask() {
    let dir = dir("matte");
    let (width, height) = (2600, 40);
    let mut sample = Sample::new("1.7");
    // Wider than the largest side pictures get at this level (2400 pixels).
    let mut matted_mask = picture(width, height, "DeviceGray", vec![128; 2600 * 40]);
    matted_mask
        .dict
        .set("Matte", vec![1.into(), 1.into(), 1.into()]);
    let matted_mask = sample.doc.add_object(matted_mask);
    let mut matted = picture(width, height, "DeviceRGB", gradient(width, height));
    matted.dict.set("SMask", matted_mask);
    matted.dict.set("Name", "Matted");
    sample.picture(matted);
    // The same without a matte: the picture and its mask may differ in size.
    let plain_mask =
        sample
            .doc
            .add_object(picture(width, height, "DeviceGray", vec![128; 2600 * 40]));
    let mut plain = picture(width, height, "DeviceRGB", gradient(width, height));
    plain.dict.set("SMask", plain_mask);
    plain.dict.set("Name", "Plain");
    sample.picture(plain);
    let (mut doc, _) = sample.finish();
    let input = dir.0.join("matte.pdf");
    save(&mut doc, &input);

    let report = run(&input, &dir.0.join("matte-1.pdf"));
    let output = Document::load(compressed(&report)).unwrap();
    let after = pictures(&output);
    let named = |name: &[u8]| {
        *after
            .iter()
            .find(|stream| stream.dict.get(b"Name").and_then(Object::as_name).ok() == Some(name))
            .unwrap()
    };
    let matted = named(b"Matted");
    assert_eq!(
        (number(matted, b"Width"), number(matted, b"Height")),
        (2600, 40),
        "the same size as its mask"
    );
    assert!(matted.dict.has(b"Filter"), "still compressed");
    assert_eq!(number(named(b"Plain"), b"Width"), 2400);
}

/// `length` zero bytes, deflated.
fn deflated_zeros(length: usize) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    let block = vec![0u8; 1 << 16];
    let mut left = length;
    while left > 0 {
        let take = left.min(block.len());
        encoder.write_all(&block[..take]).unwrap();
        left -= take;
    }
    encoder.finish().unwrap()
}

#[test]
fn pictures_that_expand_beyond_reason_are_left_alone() {
    let dir = dir("bomb");
    let mut sample = Sample::new("1.7");
    sample.picture(picture(400, 300, "DeviceRGB", gradient(400, 300)));
    // A picture that says it is 40000 by 40000 pixels: 4.8 GB once decoded.
    let mut huge = picture(40_000, 40_000, "DeviceRGB", deflated_zeros(1_000_000));
    huge.dict.set("Filter", "FlateDecode");
    sample.picture(huge);
    // A small picture whose data expands to far more than its size holds.
    let mut stuffed = picture(100, 100, "DeviceRGB", deflated_zeros(64_000_000));
    stuffed.dict.set("Filter", "FlateDecode");
    sample.picture(stuffed);
    let (mut doc, _) = sample.finish();
    let input = dir.0.join("bomb.pdf");
    save(&mut doc, &input);

    let report = run(&input, &dir.0.join("bomb-1.pdf"));
    assert!(
        !matches!(report.outcome, Outcome::Failed { .. }),
        "{:?}",
        report.outcome
    );
    let stats = report.document.clone().expect("document statistics");
    assert_eq!((stats.images, stats.skipped), (3, 2), "{stats:?}");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("40000x40000") && w.contains("left as it is")),
        "{:?}",
        report.warnings
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("100x100") && w.contains("more data")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn a_signed_pdf_is_left_alone() {
    let dir = dir("signed");
    // Signed, as the form says (`SigFlags`), and as a signature itself shows (`ByteRange`).
    for (name, flags, with_signature) in [("flags", true, false), ("range", false, true)] {
        let mut sample = Sample::new("1.7");
        sample.picture(picture(400, 300, "DeviceRGB", gradient(400, 300)));
        let (mut doc, catalog) = sample.finish();
        let mut field = dictionary! { "FT" => "Sig", "T" => Object::string_literal("Signature1") };
        if with_signature {
            let signature = doc.add_object(dictionary! {
                "Type" => "Sig", "Filter" => "Adobe.PPKLite",
                "ByteRange" => vec![0.into(), 100.into(), 200.into(), 300.into()],
                "Contents" => Object::String(vec![0; 16], StringFormat::Hexadecimal),
            });
            field.set("V", signature);
        }
        let field = doc.add_object(field);
        let mut form = dictionary! { "Fields" => vec![field.into()] };
        if flags {
            form.set("SigFlags", 3);
        }
        let form = doc.add_object(form);
        doc.get_dictionary_mut(catalog)
            .unwrap()
            .set("AcroForm", form);
        let input = dir.0.join(format!("{name}.pdf"));
        save(&mut doc, &input);

        let output = dir.0.join(format!("{name}-1.pdf"));
        let report = run(&input, &output);
        let Outcome::Skipped { reason, message } = &report.outcome else {
            panic!("{name}: unexpected outcome: {:?}", report.outcome)
        };
        assert_eq!(*reason, SkipReason::Protected, "{name}");
        assert!(message.contains("signed"), "{name}: {message}");
        assert!(!output.exists(), "{name}");
    }
}

const PDFA_XMP: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
<rdf:Description rdf:about="" xmlns:pdfaid="http://www.aiim.org/pdfa/ns/id/">
<pdfaid:part>1</pdfaid:part><pdfaid:conformance>B</pdfaid:conformance>
</rdf:Description></rdf:RDF></x:xmpmeta>
<?xpacket end="w"?>"#;

#[test]
fn a_pdf_a_keeps_the_structure_it_needs() {
    let dir = dir("pdfa");
    let mut sample = Sample::new("1.4");
    sample.picture(picture(400, 300, "DeviceRGB", gradient(400, 300)));
    let (mut doc, catalog) = sample.finish();
    let metadata = doc.add_object(
        Stream::new(
            dictionary! { "Type" => "Metadata", "Subtype" => "XML" },
            PDFA_XMP.as_bytes().to_vec(),
        )
        .with_compression(false),
    );
    doc.get_dictionary_mut(catalog)
        .unwrap()
        .set("Metadata", metadata);
    // A PDF 1.4 file, as PDF/A-1 is: with a cross-reference table.
    doc.reference_table.cross_reference_type = lopdf::xref::XrefType::CrossReferenceTable;
    let input = dir.0.join("archive.pdf");
    save(&mut doc, &input);
    assert!(std::fs::read(&input).unwrap().starts_with(b"%PDF-1.4"));

    let report = run(&input, &dir.0.join("archive-1.pdf"));
    let path = compressed(&report);
    assert_eq!(report.document.as_ref().unwrap().recompressed, 1);
    let bytes = std::fs::read(&path).unwrap();
    let holds = |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
    // PDF/A-1 is PDF 1.4: no object streams, no cross-reference streams, and metadata that
    // can be read without decompressing anything.
    assert!(
        bytes.starts_with(b"%PDF-1.4"),
        "the version stays: {:?}",
        String::from_utf8_lossy(&bytes[..12])
    );
    assert!(!holds(b"ObjStm"), "no object streams");
    assert!(!holds(b"/XRef"), "no cross-reference stream");
    assert!(
        holds(b"\nxref\n") || holds(b"\nxref\r\n"),
        "a cross-reference table"
    );
    assert!(holds(b"<pdfaid:part>1</pdfaid:part>"), "plain metadata");
    let output = Document::load(&path).unwrap();
    let metadata = output
        .catalog()
        .unwrap()
        .get(b"Metadata")
        .and_then(Object::as_reference)
        .unwrap();
    let metadata = output.get_object(metadata).unwrap().as_stream().unwrap();
    assert!(!metadata.dict.has(b"Filter"));
    assert_eq!(metadata.content, PDFA_XMP.as_bytes());
}

#[test]
fn losing_fast_web_view_is_reported() {
    let dir = dir("linear");
    let mut sample = Sample::new("1.7");
    // A linearized file opens with this dictionary as its first object. The writer leaves
    // such a dictionary out, so it goes in under another name, which is then put right in
    // the file itself (the same length, so that nothing moves).
    sample.doc.add_object(dictionary! {
        "Xinearized" => 1, "L" => 20_000, "H" => vec![600.into(), 100.into()],
        "O" => 5, "E" => 4000, "N" => 1, "T" => 19_000,
    });
    sample.picture(picture(400, 300, "DeviceRGB", gradient(400, 300)));
    let (mut doc, _) = sample.finish();
    let input = dir.0.join("web.pdf");
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let at = bytes
        .windows(11)
        .position(|window| window == b"/Xinearized")
        .unwrap();
    bytes[at + 1] = b'L';
    std::fs::write(&input, bytes).unwrap();
    assert!(Document::load(&input).unwrap().linearization.is_some());

    let report = run(&input, &dir.0.join("web-1.pdf"));
    compressed(&report);
    assert!(
        report.warnings.iter().any(|w| w.contains("fast web view")),
        "{:?}",
        report.warnings
    );
    // A file that is not linearized has nothing to report.
    let mut sample = Sample::new("1.7");
    sample.picture(picture(400, 300, "DeviceRGB", gradient(400, 300)));
    let (mut doc, _) = sample.finish();
    let plain = dir.0.join("plain.pdf");
    save(&mut doc, &plain);
    let report = run(&plain, &dir.0.join("plain-1.pdf"));
    assert_eq!(report.warnings, Vec::<String>::new());
}

/// The sample document, protected in one of the ways PDF has: `kind` is `rc4`, `aes128`, or
/// `aes256`.
fn protected(kind: &str, user_password: &str, permissions: Permissions) -> Document {
    let mut sample = Sample::new("1.7");
    sample.picture(picture(400, 300, "DeviceRGB", gradient(400, 300)));
    sample.picture(picture(400, 300, "DeviceRGB", flat(400, 300)));
    let (mut doc, _) = sample.finish();
    let filters = |filter: Arc<dyn CryptFilter>| BTreeMap::from([(b"StdCF".to_vec(), filter)]);
    let key = [7u8; 32];
    let version = match kind {
        "rc4" => EncryptionVersion::V2 {
            document: &doc,
            owner_password: "owner",
            user_password,
            key_length: 128,
            permissions,
        },
        "aes128" => EncryptionVersion::V4 {
            document: &doc,
            encrypt_metadata: true,
            crypt_filters: filters(Arc::new(Aes128CryptFilter)),
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: "owner",
            user_password,
            permissions,
        },
        _ => EncryptionVersion::V5 {
            encrypt_metadata: true,
            crypt_filters: filters(Arc::new(Aes256CryptFilter)),
            file_encryption_key: &key,
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: "owner",
            user_password,
            permissions,
        },
    };
    let state = EncryptionState::try_from(version).unwrap();
    doc.encrypt(&state).unwrap();
    doc
}

#[test]
fn a_pdf_with_restrictions_is_compressed_and_keeps_them() {
    let dir = dir("restricted");
    // Printing is allowed; copying and changing are not.
    let permissions = Permissions::PRINTABLE | Permissions::COPYABLE_FOR_ACCESSIBILITY;
    for kind in ["rc4", "aes128", "aes256"] {
        let mut doc = protected(kind, "", permissions);
        let input = dir.0.join(format!("{kind}.pdf"));
        save(&mut doc, &input);
        let before = Document::load(&input).unwrap();
        let before_state = before
            .encryption_state
            .clone()
            .expect("opens without a password");

        let report = run(&input, &dir.0.join(format!("{kind}-1.pdf")));
        let path = compressed(&report);
        assert_eq!(report.document.as_ref().unwrap().recompressed, 2, "{kind}");
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes.windows(8).any(|window| window == b"/Encrypt"),
            "{kind}: still encrypted"
        );
        // It opens without a password, as before, with the same restrictions and the same
        // owner password.
        let after = Document::load(&path).unwrap();
        let state = after.encryption_state.clone().expect("still protected");
        assert_eq!(state.permissions(), before_state.permissions(), "{kind}");
        // (The stored value also has the bits that are reserved, and always set.)
        assert!(
            state.permissions().contains(Permissions::PRINTABLE),
            "{kind}"
        );
        assert!(
            !state
                .permissions()
                .intersects(Permissions::COPYABLE | Permissions::MODIFIABLE),
            "{kind}"
        );
        assert_eq!(
            (state.version(), state.revision(), state.key_length()),
            (
                before_state.version(),
                before_state.revision(),
                before_state.key_length()
            ),
            "{kind}"
        );
        assert_eq!(state.owner_value(), before_state.owner_value(), "{kind}");
        assert_eq!(state.user_value(), before_state.user_value(), "{kind}");
        assert_eq!(
            state.file_encryption_key(),
            before_state.file_encryption_key(),
            "{kind}"
        );
        // What it holds reads back: the page, and its pictures in their new form.
        assert_eq!(after.get_pages().len(), 1, "{kind}");
        let page = *after.get_pages().values().next().unwrap();
        let page_before = *before.get_pages().values().next().unwrap();
        assert_eq!(
            after.get_page_content(page),
            before.get_page_content(page_before),
            "{kind}"
        );
        let photo = pictures(&after)
            .into_iter()
            .find(|stream| {
                stream.dict.get(b"Filter").and_then(Object::as_name).ok() == Some(b"DCTDecode")
            })
            .unwrap_or_else(|| panic!("{kind}: no JPEG picture"));
        let decoded = image::load_from_memory(&photo.content).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (400, 300), "{kind}");
    }
}

#[test]
fn a_pdf_that_needs_a_password_is_left_alone() {
    let dir = dir("locked");
    for kind in ["rc4", "aes256"] {
        let mut doc = protected(kind, "secret", Permissions::all());
        let input = dir.0.join(format!("{kind}.pdf"));
        save(&mut doc, &input);
        let output = dir.0.join(format!("{kind}-1.pdf"));
        let report = run(&input, &output);
        let Outcome::Skipped { reason, message } = &report.outcome else {
            panic!("{kind}: unexpected outcome: {:?}", report.outcome)
        };
        assert_eq!(*reason, SkipReason::Protected, "{kind}");
        assert!(message.contains("password"), "{kind}: {message}");
        assert!(!output.exists(), "{kind}");
    }
}

/// `data`, deflated the way a writer in a hurry does it.
fn deflated_fast(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// Text as a page's content stream has it: many similar drawing commands.
fn drawing_commands(lines: u32) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut text = String::new();
    for line in 0..lines {
        let (x, y) = (50 + line % 7 * 3, 800 - line % 60 * 12);
        let size = 9 + line % 4;
        let _ = writeln!(
            text,
            "BT /F1 {size} Tf {x} {y} Td (Line {line} of the sample document) Tj ET"
        );
        let _ = writeln!(text, "{x} {y} m {} {} l S", x + 200 + line % 13, y - 2);
    }
    text.into_bytes()
}

/// A stream that is in the document without being drawn: the catalog names it.
fn attach(doc: &mut Document, catalog: ObjectId, name: &str, stream: Stream) {
    let id = doc.add_object(stream);
    doc.get_dictionary_mut(catalog).unwrap().set(name, id);
}

/// The stream the catalog names `name`.
fn attached(doc: &Document, name: &str) -> Stream {
    let id = doc
        .catalog()
        .unwrap()
        .get(name.as_bytes())
        .and_then(Object::as_reference)
        .unwrap();
    doc.get_object(id).unwrap().as_stream().unwrap().clone()
}

const CMYK_SIZE: (usize, usize) = (300, 200);

/// A document whose streams were deflated in a hurry: `commands` and `table` as streams of
/// their own, and a CMYK picture stored as `rows` with a predictor. Peraz does not rewrite
/// CMYK pictures, so only its compression may change. Two more streams must stay as they are:
/// one with two filters (returned as it is stored), and one that does not inflate.
fn hurried_document(commands: &[u8], table: &[u8], rows: &[u8]) -> (Document, ObjectId, Vec<u8>) {
    let (width, height) = (
        i64::try_from(CMYK_SIZE.0).unwrap(),
        i64::try_from(CMYK_SIZE.1).unwrap(),
    );
    let mut sample = Sample::new("1.7");
    let mut cmyk = Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => width, "Height" => height,
            "ColorSpace" => "DeviceCMYK", "BitsPerComponent" => 8, "Filter" => "FlateDecode",
            "DecodeParms" => dictionary! {
                "Predictor" => 15, "Colors" => 4, "BitsPerComponent" => 8, "Columns" => width,
            },
        },
        deflated_fast(rows),
    );
    cmyk.allows_compression = false;
    let cmyk = sample.picture(cmyk);
    let (mut doc, catalog) = sample.finish();
    let fast = |data: &[u8]| {
        Stream::new(
            dictionary! { "Filter" => "FlateDecode" },
            deflated_fast(data),
        )
        .with_compression(false)
    };
    attach(&mut doc, catalog, "PerazText", fast(commands));
    attach(&mut doc, catalog, "PerazTable", fast(table));
    let hex: Vec<u8> = deflated_fast(commands)
        .iter()
        .flat_map(|byte| format!("{byte:02X}").into_bytes())
        .chain(*b">")
        .collect();
    attach(
        &mut doc,
        catalog,
        "PerazChained",
        Stream::new(
            dictionary! { "Filter" => vec!["ASCIIHexDecode".into(), "FlateDecode".into()] },
            hex.clone(),
        )
        .with_compression(false),
    );
    attach(
        &mut doc,
        catalog,
        "PerazBroken",
        Stream::new(
            dictionary! { "Filter" => "FlateDecode" },
            b"not deflated".to_vec(),
        )
        .with_compression(false),
    );
    (doc, cmyk, hex)
}

#[test]
fn flate_streams_are_deflated_again_at_the_best_level() {
    let dir = dir("deflate");
    let commands = drawing_commands(4000);
    let table: Vec<u8> = (0..60_000u32)
        .flat_map(|i| (i * i % 1021).to_be_bytes())
        .collect();
    // Each row starts with the predictor's tag, here "up" (2), and holds the differences
    // from the row above.
    let rows: Vec<u8> = (0..CMYK_SIZE.1)
        .flat_map(|y| {
            std::iter::once(2u8)
                .chain((0..CMYK_SIZE.0 * 4).map(move |x| u8::try_from((x / 4 + y) % 3).unwrap()))
        })
        .collect();
    let (mut doc, cmyk, hex) = hurried_document(&commands, &table, &rows);
    let input = dir.0.join("fast.pdf");
    save(&mut doc, &input);
    let before = Document::load(&input).unwrap();

    let report = run(&input, &dir.0.join("fast-1.pdf"));
    let after = Document::load(compressed(&report)).unwrap();
    // What the streams hold is the same, byte for byte; they take less room.
    for (name, holds) in [("PerazText", &commands), ("PerazTable", &table)] {
        let (was, is) = (attached(&before, name), attached(&after, name));
        assert_eq!(&is.decompressed_content().unwrap(), holds, "{name}");
        assert!(
            is.content.len() < was.content.len(),
            "{name}: {} bytes, was {}",
            is.content.len(),
            was.content.len()
        );
    }
    let picture_before = before.get_object(cmyk).unwrap().as_stream().unwrap();
    let picture = pictures(&after)
        .into_iter()
        .find(|stream| {
            stream
                .dict
                .get(b"ColorSpace")
                .and_then(Object::as_name)
                .ok()
                == Some(b"DeviceCMYK")
        })
        .unwrap();
    assert!(picture.content.len() < picture_before.content.len());
    assert_eq!(
        picture.dict.get(b"DecodeParms").unwrap(),
        picture_before.dict.get(b"DecodeParms").unwrap(),
        "the predictor stays"
    );
    let mut inflated = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::ZlibDecoder::new(picture.content.as_slice()),
        &mut inflated,
    )
    .unwrap();
    assert_eq!(inflated, rows, "the rows as the predictor left them");
    assert_eq!(
        picture.decompressed_content().unwrap(),
        picture_before.decompressed_content().unwrap(),
        "the same pixels"
    );
    assert_eq!(attached(&after, "PerazChained").content, hex);
    assert_eq!(attached(&after, "PerazBroken").content, b"not deflated");

    // Deflated at the best level already: nothing gets smaller, and the original is kept.
    let report = run(&compressed(&report), &dir.0.join("fast-2.pdf"));
    assert!(
        matches!(
            report.outcome,
            Outcome::KeptOriginal(peraz_core::KeepReason::NotSmaller { .. })
        ),
        "{:?}",
        report.outcome
    );
}
