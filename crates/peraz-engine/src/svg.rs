//! SVG minification: the same picture in fewer bytes.
//!
//! What is removed has no effect on how the file renders: comments, the XML declaration, a
//! doctype without entity definitions, processing instructions (except style sheets), editor
//! data (`<metadata>` and the elements, attributes, and namespace declarations of drawing
//! programs), and whitespace between elements. Numbers in geometry attributes are written in
//! their shortest form and, below the highest quality level, rounded.
//!
//! What is never touched: IDs and class names (style sheets and scripts may refer to them),
//! `viewBox`, style sheets and scripts, text content, and anything inside `<foreignObject>`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

/// Why an SVG was not minified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SvgError {
    /// The file is fine, but not one this minifier handles (its text encoding is not UTF-8).
    Unsupported(String),
    /// The file cannot be read as XML.
    Invalid(String),
}

const SVG: &str = "http://www.w3.org/2000/svg";
const XML: &str = "http://www.w3.org/XML/1998/namespace";

/// Namespaces that drawing programs use for their own data, which no renderer reads.
const EDITOR_NAMESPACES: &[&str] = &[
    "http://www.inkscape.org/namespaces/inkscape",
    "http://sodipodi.sourceforge.net/DTD/sodipodi-0.dtd",
    "http://ns.adobe.com/AdobeIllustrator/10.0/",
    "http://ns.adobe.com/AdobeSVGViewerExtensions/3.0/",
    "http://ns.adobe.com/Extensibility/1.0/",
    "http://ns.adobe.com/Flows/1.0/",
    "http://ns.adobe.com/GenericCustomNamespace/1.0/",
    "http://ns.adobe.com/Graphs/1.0/",
    "http://ns.adobe.com/ImageReplacement/1.0/",
    "http://ns.adobe.com/SaveForWeb/1.0/",
    "http://ns.adobe.com/Variables/1.0/",
    "http://ns.adobe.com/XPath/1.0/",
    "http://www.bohemiancoding.com/sketch/ns",
    "http://www.figma.com/figma/ns",
    "http://www.serif.com/",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
    "http://purl.org/dc/elements/1.1/",
    "http://creativecommons.org/ns#",
    "http://web.resource.org/cc/",
];

/// SVG elements whose text is content: whitespace inside them is kept as it is.
const TEXT_ELEMENTS: &[&str] = &[
    "text",
    "tspan",
    "tref",
    "textPath",
    "altGlyph",
    "title",
    "desc",
    "style",
    "script",
    "foreignObject",
];

/// Attributes that hold one number or one length.
const NUMBER_ATTRIBUTES: &[&str] = &[
    "x",
    "y",
    "width",
    "height",
    "cx",
    "cy",
    "r",
    "rx",
    "ry",
    "x1",
    "y1",
    "x2",
    "y2",
    "fx",
    "fy",
    "fr",
    "offset",
    "opacity",
    "fill-opacity",
    "stroke-opacity",
    "stop-opacity",
    "flood-opacity",
    "stroke-width",
    "stroke-miterlimit",
    "stroke-dashoffset",
    "font-size",
    "refX",
    "refY",
    "markerWidth",
    "markerHeight",
    "startOffset",
];

const TRANSFORM_ATTRIBUTES: &[&str] = &["transform", "gradientTransform", "patternTransform"];

/// Minifies an SVG document. `decimals` is how many decimal places numbers keep; `None` keeps
/// every number's value exactly.
pub fn minify(text: &str, decimals: Option<u8>) -> Result<String, SvgError> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut minifier = Minifier {
        out: String::with_capacity(text.len()),
        open: Vec::new(),
        bindings: Vec::new(),
        entities: HashMap::new(),
        skipping: 0,
        unclosed_tag: false,
        precision: Precision { decimals },
    };
    let invalid = |e: &dyn std::fmt::Display| SvgError::Invalid(format!("not valid XML: {e}"));
    let mut reader = Reader::from_str(text);
    loop {
        match reader.read_event().map_err(|e| invalid(&e))? {
            Event::Eof => break,
            Event::Start(element) => minifier.start(&element, false)?,
            Event::Empty(element) => minifier.start(&element, true)?,
            Event::End(_) => minifier.end(),
            Event::Text(content) => minifier.text(&content),
            Event::CData(content) => {
                minifier.raw(&["<![CDATA[", &content, "]]>"]);
            }
            Event::GeneralRef(name) => {
                minifier.raw(&["&", &name, ";"]);
            }
            // A comment does nothing, also inside a style sheet or a script.
            Event::Comment(_) => {}
            Event::Decl(declaration) => {
                let encoding = match declaration.encoding() {
                    Some(encoding) => Some(encoding.map_err(|e| invalid(&e))?),
                    None => None,
                };
                if let Some(encoding) = encoding {
                    let name = encoding.to_ascii_lowercase();
                    if !matches!(name.as_str(), "utf-8" | "utf8" | "us-ascii" | "ascii") {
                        return Err(SvgError::Unsupported(format!(
                            "this SVG is written in {name}; only UTF-8 files are minified"
                        )));
                    }
                }
            }
            Event::PI(instruction) => {
                if instruction.target() == "xml-stylesheet" {
                    minifier.raw(&["<?", &instruction, "?>"]);
                }
            }
            Event::DocType(content) => {
                // An internal subset may define entities the document uses: kept whole.
                if content.contains('[') {
                    minifier.read_entities(&content);
                    minifier.raw(&["<!DOCTYPE ", content.trim(), ">"]);
                }
            }
        }
    }
    if !minifier.open.is_empty() || minifier.skipping > 0 {
        return Err(SvgError::Invalid(
            "not valid XML: the file ends inside an element".to_owned(),
        ));
    }
    Ok(minifier.out)
}

/// An element that is open in the output.
struct Open {
    name: String,
    /// Whitespace-only text inside is content (or may be), so it is kept.
    keeps_text: bool,
    /// Attributes inside are written as they are (HTML or another vocabulary).
    foreign: bool,
    /// Namespace bindings in effect before this element.
    bindings: usize,
}

struct Minifier {
    out: String,
    open: Vec<Open>,
    /// Namespace prefixes and what they are bound to, innermost last.
    bindings: Vec<(String, String)>,
    /// Entities defined in the doctype, for reading namespace declarations written with them.
    entities: HashMap<String, String>,
    /// Depth inside an element that is being removed; 0 when writing.
    skipping: usize,
    /// The last thing written is a start tag without its closing `>`: when its end tag comes
    /// next, the element is written as empty.
    unclosed_tag: bool,
    precision: Precision,
}

impl Minifier {
    fn close_tag(&mut self) {
        if self.unclosed_tag {
            self.out.push('>');
            self.unclosed_tag = false;
        }
    }

    fn raw(&mut self, parts: &[&str]) {
        if self.skipping > 0 {
            return;
        }
        self.close_tag();
        for part in parts {
            self.out.push_str(part);
        }
    }

    fn text(&mut self, content: &str) {
        if self.skipping > 0 {
            return;
        }
        let keep = match self.open.last() {
            Some(element) => element.keeps_text || !content.trim().is_empty(),
            // Around the root element there is only whitespace.
            None => false,
        };
        if keep {
            self.close_tag();
            self.out.push_str(content);
        }
    }

    /// Reads `<!ENTITY name "value">` definitions from a doctype's internal subset.
    fn read_entities(&mut self, doctype: &str) {
        for declaration in doctype.split("<!ENTITY").skip(1) {
            let mut words = declaration.trim_start().splitn(2, char::is_whitespace);
            let (Some(name), Some(rest)) = (words.next(), words.next()) else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(quote) = rest.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
                continue;
            };
            if let Some(end) = rest[1..].find(quote) {
                self.entities
                    .insert(name.to_owned(), rest[1..=end].to_owned());
            }
        }
    }

    /// An attribute value with the doctype's entities filled in.
    fn expanded(&self, value: &str) -> String {
        let mut result = String::with_capacity(value.len());
        let mut rest = value;
        while let Some(start) = rest.find('&') {
            result.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            let known = after
                .find(';')
                .and_then(|end| Some((end, self.entities.get(&after[..end])?)));
            if let Some((end, replacement)) = known {
                result.push_str(replacement);
                rest = &after[end + 1..];
            } else {
                result.push('&');
                rest = after;
            }
        }
        result.push_str(rest);
        result
    }

    /// The namespace a prefix is bound to; an attribute without a prefix has none.
    fn namespace(&self, prefix: &str) -> &str {
        if prefix == "xml" {
            return XML;
        }
        self.bindings
            .iter()
            .rev()
            .find(|(bound, _)| bound == prefix)
            .map_or("", |(_, uri)| uri.as_str())
    }

    fn start(&mut self, element: &BytesStart, empty: bool) -> Result<(), SvgError> {
        if self.skipping > 0 {
            self.skipping += usize::from(!empty);
            return Ok(());
        }
        let invalid = |e: &dyn std::fmt::Display| SvgError::Invalid(format!("not valid XML: {e}"));
        let name = element.name().as_ref().to_owned();
        let mut attributes = Vec::new();
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|e| invalid(&e))?;
            attributes.push((
                attribute.key.as_ref().to_owned(),
                attribute.value.into_owned(),
            ));
        }
        let outer_bindings = self.bindings.len();
        for (key, value) in &attributes {
            if key == "xmlns" {
                self.bindings.push((String::new(), self.expanded(value)));
            } else if let Some(prefix) = key.strip_prefix("xmlns:") {
                self.bindings
                    .push((prefix.to_owned(), self.expanded(value)));
            }
        }
        let (prefix, local) = name.split_once(':').unwrap_or(("", &name));
        let namespace = self.namespace(prefix).to_owned();
        let is_svg = namespace == SVG;
        if EDITOR_NAMESPACES.contains(&namespace.as_str()) || (is_svg && local == "metadata") {
            self.bindings.truncate(outer_bindings);
            self.skipping = usize::from(!empty);
            return Ok(());
        }
        let parent = self.open.last();
        let foreign = parent.is_some_and(|p| p.foreign) || !is_svg || local == "foreignObject";
        let keeps_text = parent.is_some_and(|p| p.keeps_text)
            || foreign
            || TEXT_ELEMENTS.contains(&local)
            || attributes
                .iter()
                .any(|(key, value)| key == "xml:space" && value == "preserve");
        // Inside foreign content, an element's own attributes are left alone.
        let rewrite = is_svg && !parent.is_some_and(|p| p.foreign);

        self.close_tag();
        self.out.push('<');
        self.out.push_str(&name);
        for (key, value) in &attributes {
            let editor_data = match key.split_once(':') {
                Some(("xmlns", _)) => EDITOR_NAMESPACES.contains(&self.expanded(value).as_str()),
                Some((prefix, _)) => EDITOR_NAMESPACES.contains(&self.namespace(prefix)),
                None => false,
            };
            if editor_data {
                continue;
            }
            let shorter = if rewrite {
                self.precision.attribute(local, key, value)
            } else {
                None
            };
            let value = shorter.as_deref().unwrap_or(value);
            // The value is written as it was read, so it holds at most one kind of quote.
            let quote = if value.contains('"') { '\'' } else { '"' };
            let _ = write!(self.out, " {key}={quote}{value}{quote}");
        }
        if empty {
            self.out.push_str("/>");
            self.bindings.truncate(outer_bindings);
        } else {
            self.unclosed_tag = true;
            self.open.push(Open {
                name,
                keeps_text,
                foreign,
                bindings: outer_bindings,
            });
        }
        Ok(())
    }

    fn end(&mut self) {
        if self.skipping > 0 {
            self.skipping -= 1;
            return;
        }
        let Some(element) = self.open.pop() else {
            return;
        };
        if self.unclosed_tag {
            self.out.push_str("/>");
            self.unclosed_tag = false;
        } else {
            let _ = write!(self.out, "</{}>", element.name);
        }
        self.bindings.truncate(element.bindings);
    }
}

/// How numbers are written.
#[derive(Debug, Clone, Copy)]
struct Precision {
    decimals: Option<u8>,
}

impl Precision {
    /// A shorter way to write an attribute of the SVG element `element`, when there is one
    /// that means the same.
    fn attribute(self, element: &str, key: &str, value: &str) -> Option<String> {
        let shorter = if key == "d" && matches!(element, "path" | "glyph" | "missing-glyph") {
            self.path(value)?
        } else if key == "points" && matches!(element, "polygon" | "polyline") {
            self.number_list(value)?
        } else if TRANSFORM_ATTRIBUTES.contains(&key) {
            self.transforms(value)?
        } else if NUMBER_ATTRIBUTES.contains(&key) {
            self.length(value)?
        } else {
            return None;
        };
        (shorter.len() < value.len()).then_some(shorter)
    }

    /// The shortest text for `value`, rounded when this precision rounds; `extra` adds decimal
    /// places (for transforms, whose numbers are multiplied with coordinates).
    fn number(self, value: f64, extra: u8) -> Option<String> {
        if !value.is_finite() {
            return None;
        }
        let mut text = match self.decimals {
            None => format!("{value}"),
            Some(decimals) => {
                let decimals = usize::from(decimals + extra);
                // Below 1, keep at least two significant digits (and as many as the decimal
                // places asked for), so small numbers do not collapse to nothing.
                let magnitude = value.abs();
                let places = if magnitude > 0.0 && magnitude < 1.0 {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let leading_zeros = (-magnitude.log10()).floor().max(0.0) as usize;
                    decimals.max(leading_zeros + decimals.max(2)).min(17)
                } else {
                    decimals
                };
                let mut rounded = format!("{value:.places$}");
                if rounded.contains('.') {
                    let kept = rounded.trim_end_matches('0').trim_end_matches('.').len();
                    rounded.truncate(kept);
                }
                rounded
            }
        };
        if text == "-0" {
            "0".clone_into(&mut text);
        }
        if let Some(fraction) = text.strip_prefix("0.") {
            text = format!(".{fraction}");
        } else if let Some(fraction) = text.strip_prefix("-0.") {
            text = format!("-.{fraction}");
        }
        Some(text)
    }

    /// One number with an optional unit, such as `1.50px` or `50%`.
    fn length(self, value: &str) -> Option<String> {
        let mut scanner = Scanner::new(value.trim());
        let number = scanner.number()?;
        let unit = scanner.rest();
        if !unit.chars().all(|c| c.is_ascii_alphabetic() || c == '%') {
            return None;
        }
        Some(format!("{}{unit}", self.number(number, 0)?))
    }

    /// Numbers separated by whitespace or commas, as in `points`.
    fn number_list(self, value: &str) -> Option<String> {
        let mut scanner = Scanner::new(value);
        let mut out = Joined::default();
        loop {
            scanner.skip_separators();
            if scanner.is_done() {
                break;
            }
            out.number(&self.number(scanner.number()?, 0)?);
        }
        Some(out.text)
    }

    /// A list of transforms such as `translate(10, 20) rotate(45)`.
    fn transforms(self, value: &str) -> Option<String> {
        let mut scanner = Scanner::new(value);
        let mut out = String::new();
        loop {
            scanner.skip_separators();
            if scanner.is_done() {
                break;
            }
            let name = scanner.word()?;
            if !matches!(
                name,
                "matrix" | "translate" | "scale" | "rotate" | "skewX" | "skewY"
            ) {
                return None;
            }
            scanner.skip_whitespace();
            scanner.expect(b'(')?;
            let mut arguments = Joined::default();
            loop {
                scanner.skip_separators();
                if scanner.peek() == Some(b')') {
                    break;
                }
                arguments.number(&self.number(scanner.number()?, 2)?);
            }
            scanner.expect(b')')?;
            if !out.is_empty() {
                out.push(' ');
            }
            let _ = write!(out, "{name}({})", arguments.text);
        }
        Some(out)
    }

    /// Path data: the same commands and numbers, without needless characters.
    fn path(self, value: &str) -> Option<String> {
        let mut scanner = Scanner::new(value);
        let mut out = Joined::default();
        let mut previous = None;
        loop {
            scanner.skip_separators();
            let Some(command) = scanner.peek() else {
                break;
            };
            let arguments = match command.to_ascii_lowercase() {
                b'z' => 0,
                b'h' | b'v' => 1,
                b'm' | b'l' | b't' => 2,
                b's' | b'q' => 4,
                b'c' => 6,
                b'a' => 7,
                _ => return None,
            };
            // A path starts with a move.
            if previous.is_none() && !command.eq_ignore_ascii_case(&b'm') {
                return None;
            }
            scanner.expect(command)?;
            // The same command again needs no letter, except a move (more pairs after a move
            // are lines) and a close.
            let repeated =
                previous == Some(command) && !matches!(command, b'M' | b'm' | b'Z' | b'z');
            if !repeated {
                out.command(char::from(command));
            }
            previous = Some(command);
            if arguments == 0 {
                continue;
            }
            // More groups of arguments may follow without the letter.
            loop {
                for index in 0..arguments {
                    scanner.skip_separators();
                    // The two flags of an arc are single digits, possibly without separators.
                    if arguments == 7 && matches!(index, 3 | 4) {
                        out.number(scanner.flag()?);
                    } else {
                        out.number(&self.number(scanner.number()?, 0)?);
                    }
                }
                scanner.skip_separators();
                if !scanner
                    .peek()
                    .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.'))
                {
                    break;
                }
            }
        }
        previous?;
        Some(out.text)
    }
}

/// Numbers joined with only the separators a reader needs.
#[derive(Default)]
struct Joined {
    text: String,
    /// The last thing written is a number, and whether it has a decimal point.
    last_number: Option<bool>,
}

impl Joined {
    fn command(&mut self, letter: char) {
        self.text.push(letter);
        self.last_number = None;
    }

    fn number(&mut self, number: &str) {
        // After a number, a sign starts the next one, and so does a decimal point when the
        // number before already had its own.
        let separate = match self.last_number {
            None => false,
            Some(had_point) => !(number.starts_with('-') || (number.starts_with('.') && had_point)),
        };
        if separate {
            self.text.push(' ');
        }
        self.text.push_str(number);
        self.last_number = Some(number.contains('.'));
    }
}

/// Reads numbers, words, and punctuation from an attribute value.
struct Scanner<'a> {
    bytes: &'a [u8],
    text: &'a str,
    at: usize,
}

impl<'a> Scanner<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            text,
            at: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn is_done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.at += 1;
        }
    }

    fn skip_separators(&mut self) {
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_whitespace() || c == b',')
        {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        (self.peek() == Some(byte)).then(|| self.at += 1)
    }

    fn word(&mut self) -> Option<&'a str> {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.at += 1;
        }
        (self.at > start).then(|| &self.text[start..self.at])
    }

    /// An arc flag: `0` or `1`.
    fn flag(&mut self) -> Option<&'static str> {
        let flag = match self.peek()? {
            b'0' => "0",
            b'1' => "1",
            _ => return None,
        };
        self.at += 1;
        Some(flag)
    }

    /// A number as SVG writes them: an optional sign, digits with an optional decimal point,
    /// and an optional exponent.
    fn number(&mut self) -> Option<f64> {
        let start = self.at;
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.at += 1;
        }
        let digits = |scanner: &mut Self| {
            let from = scanner.at;
            while scanner.peek().is_some_and(|c| c.is_ascii_digit()) {
                scanner.at += 1;
            }
            scanner.at - from
        };
        let mut count = digits(self);
        if self.peek() == Some(b'.') {
            self.at += 1;
            count += digits(self);
        }
        if count == 0 {
            self.at = start;
            return None;
        }
        // An exponent needs digits: `1em` is the number 1 with the unit `em`.
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let before = self.at;
            self.at += 1;
            if matches!(self.peek(), Some(b'-' | b'+')) {
                self.at += 1;
            }
            if digits(self) == 0 {
                self.at = before;
            }
        }
        self.text[start..self.at]
            .trim_start_matches('+')
            .parse()
            .ok()
    }
}

/// What the start of an SVG file says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SvgHeader {
    /// The file is gzip-compressed (`.svgz`).
    pub gzip: bool,
    /// The size the picture asks for, in pixels, from `width` and `height` or else from
    /// `viewBox`; 0 when the file does not say.
    pub width: u32,
    pub height: u32,
}

/// How much of a file is read to recognize an SVG: its root element comes after at most a
/// declaration, a doctype, and comments.
const SNIFF_BYTES: u64 = 256 * 1024;

/// Recognizes an SVG file, gzip-compressed or not, by its root element.
pub fn sniff(path: &Path) -> std::io::Result<Option<SvgHeader>> {
    let mut head = Vec::new();
    std::fs::File::open(path)?
        .take(SNIFF_BYTES)
        .read_to_end(&mut head)?;
    let gzip = head.starts_with(&[0x1F, 0x8B]);
    if gzip {
        let mut text = Vec::new();
        // A cut-off stream ends with an error after the bytes that could be read.
        let _ = flate2::read::MultiGzDecoder::new(head.as_slice())
            .take(SNIFF_BYTES)
            .read_to_end(&mut text);
        head = text;
    }
    // Text only: XML has no NUL bytes, and no other format here starts with `<`.
    let start = head.iter().position(|b| !b.is_ascii_whitespace());
    let text = String::from_utf8_lossy(&head);
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
    if start.is_none() || !text.trim_start().starts_with('<') {
        return Ok(None);
    }
    let mut reader = Reader::from_str(text);
    loop {
        match reader.read_event() {
            Ok(Event::Start(root) | Event::Empty(root)) => {
                let name = root.name().as_ref().to_owned();
                if name != "svg" && !name.ends_with(":svg") {
                    return Ok(None);
                }
                let attribute = |wanted: &str| {
                    root.attributes()
                        .flatten()
                        .find_map(|a| (a.key.as_ref() == wanted).then(|| a.value.into_owned()))
                };
                let view_box: Vec<f64> = attribute("viewBox")
                    .map(|v| {
                        v.split(|c: char| c.is_whitespace() || c == ',')
                            .filter_map(|n| n.parse().ok())
                            .collect()
                    })
                    .unwrap_or_default();
                let size = |name: &str, index: usize| {
                    let pixels = attribute(name)
                        .and_then(|v| v.trim().trim_end_matches("px").parse::<f64>().ok())
                        .or_else(|| view_box.get(index).copied())
                        .filter(|v| v.is_finite() && *v >= 0.0)
                        .unwrap_or(0.0);
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let pixels = pixels.round().min(f64::from(u32::MAX)) as u32;
                    pixels
                };
                return Ok(Some(SvgHeader {
                    gzip,
                    width: size("width", 2),
                    height: size("height", 3),
                }));
            }
            Ok(Event::Eof) | Err(_) => return Ok(None),
            Ok(_) => {}
        }
    }
}

/// Largest SVG text read from a gzip-compressed file: more than any real drawing, and a limit
/// on what a small malicious file can expand to.
const MAX_UNZIPPED_BYTES: u64 = 1 << 30;

/// Reads an SVG file as text, unpacking it when it is gzip-compressed.
pub fn read(path: &Path) -> Result<(String, bool), SvgError> {
    let name = path.display();
    let bytes =
        std::fs::read(path).map_err(|e| SvgError::Invalid(format!("cannot read {name}: {e}")))?;
    let gzip = bytes.starts_with(&[0x1F, 0x8B]);
    let bytes = if gzip {
        let mut text = Vec::new();
        flate2::read::MultiGzDecoder::new(bytes.as_slice())
            .take(MAX_UNZIPPED_BYTES + 1)
            .read_to_end(&mut text)
            .map_err(|e| SvgError::Invalid(format!("cannot unpack {name}: {e}")))?;
        if text.len() as u64 > MAX_UNZIPPED_BYTES {
            return Err(SvgError::Unsupported(format!(
                "{name} unpacks to more than 1 GB"
            )));
        }
        text
    } else {
        bytes
    };
    let text = String::from_utf8(bytes).map_err(|_| {
        SvgError::Unsupported(
            "this SVG is not written in UTF-8; only UTF-8 files are minified".to_owned(),
        )
    })?;
    Ok((text, gzip))
}

/// Compresses SVG text with gzip, for `.svgz`.
pub fn gzip(text: &str) -> Result<Vec<u8>, String> {
    use std::io::Write as _;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder
        .write_all(text.as_bytes())
        .and_then(|()| encoder.finish())
        .map_err(|e| format!("cannot compress the SVG: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = r#"xmlns="http://www.w3.org/2000/svg""#;

    fn exact(svg: &str) -> String {
        minify(svg, None).unwrap()
    }

    /// The `d` of a single path after minifying with `decimals`.
    fn path(d: &str, decimals: Option<u8>) -> String {
        let svg = format!(r#"<svg {NS}><path d="{d}"/></svg>"#);
        let out = minify(&svg, decimals).unwrap();
        let start = out.find("d=\"").unwrap() + 3;
        out[start..].split('"').next().unwrap().to_owned()
    }

    #[test]
    fn what_no_renderer_reads_is_removed() {
        let svg = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<!-- a comment -->
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<?editor data?>
<svg {NS} xmlns:ink="http://www.inkscape.org/namespaces/inkscape" ink:version="1.2">
  <metadata><a><b/></a></metadata>
  <ink:grid units="px"><ink:line/></ink:grid>
  <g ink:label="Layer">
    <!-- inside -->
    <rect width="1" height="1"></rect>
  </g>
</svg>
"#
        );
        assert_eq!(
            exact(&svg),
            format!(r#"<svg {NS}><g><rect width="1" height="1"/></g></svg>"#)
        );
    }

    #[test]
    fn what_a_page_may_refer_to_is_kept() {
        let svg = format!(
            r##"<?xml-stylesheet href="look.css" type="text/css"?><svg {NS} xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0  0  10.0  10.0" id="root" class="a b" data-name="x"><style>
  #unused-looking {{ fill: red }}
</style><script>document.getElementById("by-script")</script><defs><g id="unused-looking"/><g id="by-script"/></defs><title> A  title </title><use xlink:href="#by-script"/></svg>"##
        );
        // Nothing here can be removed or rewritten safely.
        assert_eq!(exact(&svg), svg);
    }

    #[test]
    fn text_and_foreign_content_keep_their_whitespace() {
        let svg = format!(
            r#"<svg {NS}>
  <text x="1.0"> two  <tspan dx="1.50 2.50"> words </tspan> </text>
  <g xml:space="preserve"> <g> </g> </g>
  <foreignObject width="10.0" height="10.0">
    <div xmlns="http://www.w3.org/1999/xhtml" width="1.0">
      <p> some text </p>
    </div>
  </foreignObject>
</svg>"#
        );
        assert_eq!(
            exact(&svg),
            format!(
                r#"<svg {NS}><text x="1"> two  <tspan dx="1.50 2.50"> words </tspan> </text><g xml:space="preserve"> <g> </g> </g><foreignObject width="10" height="10">
    <div xmlns="http://www.w3.org/1999/xhtml" width="1.0">
      <p> some text </p>
    </div>
  </foreignObject></svg>"#
            )
        );
    }

    #[test]
    fn entities_from_the_doctype_still_work() {
        // Illustrator writes its namespaces as entities.
        let svg = format!(
            r##"<!DOCTYPE svg [
  <!ENTITY ns_ai "http://ns.adobe.com/AdobeIllustrator/10.0/">
  <!ENTITY brand "#336699">
]>
<svg {NS} xmlns:i="&ns_ai;" i:viewOrigin="0 0">
  <i:pgf id="private">AAAA</i:pgf>
  <switch>
    <foreignObject requiredExtensions="&ns_ai;" width="1" height="1"><i:pgfRef/></foreignObject>
    <g i:extraneous="self"><rect fill="&brand;" width="5" height="5"/></g>
  </switch>
</svg>"##
        );
        let out = exact(&svg);
        assert!(out.starts_with("<!DOCTYPE svg ["), "{out}");
        assert!(out.ends_with(&format!(
            r#"]><svg {NS}><switch><foreignObject requiredExtensions="&ns_ai;" width="1" height="1"/><g><rect fill="&brand;" width="5" height="5"/></g></switch></svg>"#
        )), "{out}");
    }

    #[test]
    fn numbers_are_written_short_and_rounded_on_request() {
        let p = |decimals| Precision { decimals };
        let exact = p(None);
        assert_eq!(exact.number(1.5, 0).unwrap(), "1.5");
        assert_eq!(exact.number(0.25, 0).unwrap(), ".25");
        assert_eq!(exact.number(-0.25, 0).unwrap(), "-.25");
        assert_eq!(exact.number(-0.0, 0).unwrap(), "0");
        assert_eq!(exact.number(1e-7, 0).unwrap(), ".0000001");
        assert_eq!(
            exact.number(12_345.678_901_234, 0).unwrap(),
            "12345.678901234"
        );
        assert_eq!(exact.number(f64::NAN, 0), None);

        let three = p(Some(3));
        assert_eq!(three.number(12.345_678, 0).unwrap(), "12.346");
        assert_eq!(three.number(12.0004, 0).unwrap(), "12");
        assert_eq!(three.number(-0.0004, 0).unwrap(), "-.0004");
        assert_eq!(three.number(0.5, 0).unwrap(), ".5");
        // Below 1, three significant digits survive whatever the size.
        assert_eq!(three.number(0.001_234_56, 0).unwrap(), ".00123");
        assert_eq!(three.number(0.000_000_987_65, 0).unwrap(), ".000000988");
        // Transforms keep two more places.
        assert_eq!(three.number(0.707_114_2, 2).unwrap(), ".70711");
        let one = p(Some(1));
        assert_eq!(one.number(3.14059, 0).unwrap(), "3.1");
        assert_eq!(one.number(0.044, 0).unwrap(), ".044");
        assert_eq!(one.number(0.96, 0).unwrap(), ".96");
        assert_eq!(one.number(1.96, 0).unwrap(), "2");

        assert_eq!(exact.length("1.50px").unwrap(), "1.5px");
        assert_eq!(exact.length(" 50.0% ").unwrap(), "50%");
        assert_eq!(exact.length("1.0em").unwrap(), "1em");
        assert_eq!(exact.length("1e2").unwrap(), "100");
        for not_one_number in ["auto", "1 2", "calc(1px + 2px)", "", "1px solid"] {
            assert_eq!(exact.length(not_one_number), None, "{not_one_number}");
        }
        assert_eq!(
            three
                .transforms("translate( 10.00 , -20.5 )  rotate(45.000000,1,1),scale(0.123456789)")
                .unwrap(),
            "translate(10-20.5) rotate(45 1 1) scale(.12346)"
        );
        assert_eq!(exact.transforms("spin(3)"), None);
        assert_eq!(exact.transforms("rotate(3"), None);
        assert_eq!(exact.number_list(" 0.5,0.5  -1,2 ").unwrap(), ".5.5-1 2");
    }

    #[test]
    fn paths_keep_their_commands_and_lose_needless_characters() {
        assert_eq!(
            path("M 10.0 , 20.0 L 30.50 40.25 L 50 60 H 70 V 80 Z", None),
            "M10 20L30.5 40.25 50 60H70V80Z"
        );
        // More pairs after a move are lines; a second move is still a move.
        assert_eq!(path("M1 1 2 2 M 3 3 m 4 4", None), "M1 1 2 2M3 3m4 4");
        // A decimal point or a sign starts the next number.
        assert_eq!(
            path("m 0.5 0.5 l -0.5 0.25 0.125 -1", None),
            "m.5.5l-.5.25.125-1"
        );
        // Arc flags are single digits, with or without separators around them; they are
        // written apart, which every reader understands.
        let arcs = "M6 8a6 6 0 0112 0A1.50 1.50 0 1 0 2.5,2.5a1 1 0 10-1-1";
        assert_eq!(
            Precision { decimals: None }.path(arcs).unwrap(),
            "M6 8a6 6 0 0 1 12 0A1.5 1.5 0 1 0 2.5 2.5a1 1 0 1 0-1-1"
        );
        // When that is not shorter than what the file has, the file's own text stays.
        assert_eq!(path(arcs, None), arcs);
        assert_eq!(
            path("M 6 8 a 6.0 6.0 0 0 1 12.0 0", None),
            "M6 8a6 6 0 0 1 12 0"
        );
        assert_eq!(
            path("M0.123456 1.987654C1e1 2E0 3.5e-1 4 5 6", Some(2)),
            "M.12 1.99C10 2 .35 4 5 6"
        );
        assert_eq!(path("M1 1ZM2 2z", None), "M1 1ZM2 2z");
        // A path that is not understood is left exactly as it is.
        for odd in [
            "L1 1",
            "M1 1 X 2 2",
            "M1 1 L 2",
            "M1 1 A 1 1 0 2 0 3 3",
            "",
            "  ",
        ] {
            assert_eq!(path(odd, Some(1)), odd, "{odd:?}");
        }
    }

    #[test]
    fn only_geometry_of_svg_elements_is_rewritten() {
        let svg = format!(
            r#"<svg {NS} width="100.0%" height="50.0px"><rect x="1.0" y="+2.0" width="3.0" stroke-width="1.50" stroke-dasharray="1.0 2.0"/><animate attributeName="x" from="1.0" to="2.0" values="1.0;2.0"/><polygon points="1.0,2.0 3.0,4.0"/><rect points="1.0,2.0" d="M 1 1"/><text x="1.0 2.0" font-size="12.0px"/><g opacity='0.50' title='say "hi"'/></svg>"#
        );
        assert_eq!(
            exact(&svg),
            format!(
                r#"<svg {NS} width="100%" height="50px"><rect x="1" y="2" width="3" stroke-width="1.5" stroke-dasharray="1.0 2.0"/><animate attributeName="x" from="1.0" to="2.0" values="1.0;2.0"/><polygon points="1 2 3 4"/><rect points="1.0,2.0" d="M 1 1"/><text x="1.0 2.0" font-size="12px"/><g opacity=".5" title='say "hi"'/></svg>"#
            )
        );
    }

    #[test]
    fn files_that_cannot_be_minified_are_refused() {
        let latin = r#"<?xml version="1.0" encoding="ISO-8859-1"?><svg/>"#;
        assert!(matches!(minify(latin, None), Err(SvgError::Unsupported(_))));
        for broken in [
            "<svg><g></svg>",
            "<svg><g>",
            r#"<svg a="1" a="2"/>"#,
            "<svg></svg></svg>",
        ] {
            assert!(
                matches!(minify(broken, None), Err(SvgError::Invalid(_))),
                "{broken}"
            );
        }
        // A byte order mark is not content.
        assert_eq!(exact("\u{FEFF}<svg/>"), "<svg/>");
    }
}
