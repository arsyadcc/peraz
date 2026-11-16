//! EXIF as directories: reading the tags of an EXIF block or of a TIFF file, writing them as an
//! EXIF block, and adding them to a TIFF file.
//!
//! An EXIF block is a small TIFF structure: a first directory (camera, dates), which points to
//! the EXIF directory (exposure details), which may point to an interoperability directory,
//! and to the GPS directory. A TIFF file keeps the same tags, but among its picture's own: the
//! first directory's tags sit next to the width and the strip offsets, and the other
//! directories hang off it. So EXIF cannot be copied into or out of a TIFF as one block: the
//! directories are read, and laid out again.

/// The tags of a TIFF's first directory that are metadata, not a description of its pixel
/// data: what moves between an EXIF block and a TIFF file. The orientation is left out: the
/// pixels are stored upright.
const MAIN_TAGS: [u16; 8] = [
    0x010E, // ImageDescription
    0x010F, // Make
    0x0110, // Model
    0x0131, // Software
    0x0132, // DateTime
    0x013B, // Artist
    0x013C, // HostComputer
    0x8298, // Copyright
];
/// Points from the first directory to the EXIF directory.
const EXIF_POINTER: u16 = 0x8769;
/// Points from the first directory to the GPS directory.
const GPS_POINTER: u16 = 0x8825;
/// Points from the EXIF directory to the interoperability directory.
const INTEROP_POINTER: u16 = 0xA005;
/// The camera maker's own data, in a layout only the maker knows.
const MAKER_NOTE: u16 = 0x927C;

/// The most entries read from one directory, and the most bytes one value may hold: far more
/// than a camera writes, and a bound on what a damaged file can ask for.
const MAX_ENTRIES: usize = 4096;
const MAX_VALUE_BYTES: usize = 8_000_000;

/// One entry of a directory: its tag, its type, how many values it has, and the values' bytes
/// in the byte order they were read in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    value: Vec<u8>,
}

/// The size of one value of a type, in bytes; `None` for a type this does not know.
fn unit(kind: u16) -> Option<usize> {
    match kind {
        1 | 2 | 6 | 7 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 | 13 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

/// The directories of an EXIF block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Directories {
    big_endian: bool,
    main: Vec<Entry>,
    exif: Vec<Entry>,
    interop: Vec<Entry>,
    gps: Vec<Entry>,
}

/// Reads numbers from a TIFF structure in its byte order.
#[derive(Clone, Copy)]
struct Reader<'a> {
    bytes: &'a [u8],
    big_endian: bool,
}

impl<'a> Reader<'a> {
    /// A classic TIFF structure (not `BigTIFF`), with the offset of its first directory.
    fn new(bytes: &'a [u8]) -> Option<(Self, usize)> {
        let big_endian = match bytes.get(..4)? {
            b"II*\0" => false,
            b"MM\0*" => true,
            _ => return None,
        };
        let reader = Self { bytes, big_endian };
        Some((reader, reader.long(4)? as usize))
    }

    fn short(&self, at: usize) -> Option<u16> {
        let bytes: [u8; 2] = self.bytes.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    }

    fn long(&self, at: usize) -> Option<u32> {
        let bytes: [u8; 4] = self.bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        })
    }

    /// The entries of the directory at `at` that `keep` lets through. An entry whose value
    /// cannot be read is left out; a directory that cannot be read at all is empty.
    fn directory(&self, at: usize, keep: impl Fn(u16) -> bool) -> Vec<Entry> {
        let count = usize::from(self.short(at).unwrap_or(0)).min(MAX_ENTRIES);
        (0..count)
            .filter_map(|index| {
                let entry = at.checked_add(2 + index * 12)?;
                let tag = self.short(entry)?;
                if !keep(tag) {
                    return None;
                }
                let kind = self.short(entry + 2)?;
                let count = self.long(entry + 4)?;
                let length = unit(kind)?.checked_mul(count as usize)?;
                if length > MAX_VALUE_BYTES {
                    return None;
                }
                // Up to four bytes are in the entry itself; more are where it points.
                let start = if length <= 4 {
                    entry + 8
                } else {
                    self.long(entry + 8)? as usize
                };
                let value = self.bytes.get(start..start.checked_add(length)?)?.to_vec();
                Some(Entry {
                    tag,
                    kind,
                    count,
                    value,
                })
            })
            .collect()
    }

    /// Where the directory that `pointer` names starts, as an entry of `entries` says.
    fn pointed(&self, entries: &[Entry], pointer: u16) -> Option<usize> {
        let entry = entries.iter().find(|entry| entry.tag == pointer)?;
        let bytes: [u8; 4] = entry.value.get(..4)?.try_into().ok()?;
        Some(if self.big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        } as usize)
    }

    /// The metadata directories of the structure. `main` says which tags of the first
    /// directory are taken; the pointers are followed either way.
    fn directories(&self, first: usize, main: impl Fn(u16) -> bool) -> Directories {
        let pointers = self.directory(first, |tag| tag == EXIF_POINTER || tag == GPS_POINTER);
        let exif_at = self.pointed(&pointers, EXIF_POINTER);
        let all_exif = exif_at.map_or_else(Vec::new, |at| self.directory(at, |_| true));
        let interop_at = self.pointed(&all_exif, INTEROP_POINTER);
        Directories {
            big_endian: self.big_endian,
            main: self.directory(first, main),
            exif: all_exif
                .into_iter()
                .filter(|entry| entry.tag != INTEROP_POINTER)
                .collect(),
            interop: interop_at.map_or_else(Vec::new, |at| self.directory(at, |_| true)),
            gps: self
                .pointed(&pointers, GPS_POINTER)
                .map_or_else(Vec::new, |at| self.directory(at, |_| true)),
        }
    }
}

impl Directories {
    fn is_empty(&self) -> bool {
        self.main.is_empty() && self.exif.is_empty() && self.gps.is_empty()
    }

    /// The same directories with their numbers in the other byte order. The maker's own data
    /// is left out: its layout is not known, and some makers write it in the byte order of
    /// the block around it.
    fn in_order(mut self, big_endian: bool) -> Self {
        if self.big_endian == big_endian {
            return self;
        }
        self.big_endian = big_endian;
        self.exif.retain(|entry| entry.tag != MAKER_NOTE);
        for entry in self
            .main
            .iter_mut()
            .chain(&mut self.exif)
            .chain(&mut self.interop)
            .chain(&mut self.gps)
        {
            // Rationals are two four-byte numbers; everything else turns as a whole.
            let width = match unit(entry.kind) {
                Some(8) if matches!(entry.kind, 5 | 10) => 4,
                Some(width) => width,
                None => 1,
            };
            for number in entry.value.chunks_exact_mut(width) {
                number.reverse();
            }
        }
        self
    }

    fn short(&self, value: u16) -> [u8; 2] {
        if self.big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    }

    fn long(&self, value: u32) -> [u8; 4] {
        if self.big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    }

    /// One directory as bytes, to stand at `at`: its entries in tag order, then the values
    /// that do not fit in an entry. `None` when an offset does not fit in 32 bits.
    fn directory(&self, entries: &[Entry], at: usize) -> Option<Vec<u8>> {
        let mut entries: Vec<&Entry> = entries.iter().collect();
        entries.sort_by_key(|entry| entry.tag);
        let mut head = self.short(u16::try_from(entries.len()).ok()?).to_vec();
        let mut values = Vec::new();
        let values_at = at + 2 + entries.len() * 12 + 4;
        for entry in entries {
            head.extend(self.short(entry.tag));
            head.extend(self.short(entry.kind));
            head.extend(self.long(entry.count));
            if entry.value.len() <= 4 {
                let mut inline = entry.value.clone();
                inline.resize(4, 0);
                head.extend(inline);
            } else {
                head.extend(self.long(u32::try_from(values_at + values.len()).ok()?));
                values.extend_from_slice(&entry.value);
                // Values start on even offsets.
                if values.len() % 2 == 1 {
                    values.push(0);
                }
            }
        }
        head.extend([0; 4]); // no directory follows
        head.extend(values);
        Some(head)
    }

    fn pointer(&self, tag: u16, at: usize) -> Option<Entry> {
        Some(Entry {
            tag,
            kind: 4,
            count: 1,
            value: self.long(u32::try_from(at).ok()?).to_vec(),
        })
    }

    /// The directories below the first one, laid out from `at` on: their bytes, and the
    /// entries that point to the EXIF and the GPS directory from the first one.
    fn below_main(&self, at: usize) -> Option<(Vec<u8>, Vec<Entry>)> {
        let mut bytes = Vec::new();
        let mut pointers = Vec::new();
        if !self.exif.is_empty() {
            let mut exif = self.exif.clone();
            if !self.interop.is_empty() {
                // The interoperability directory first, so that the EXIF directory can say
                // where it is.
                let interop_at = at + bytes.len();
                bytes.extend(self.directory(&self.interop, interop_at)?);
                exif.push(self.pointer(INTEROP_POINTER, interop_at)?);
            }
            let exif_at = at + bytes.len();
            bytes.extend(self.directory(&exif, exif_at)?);
            pointers.push(self.pointer(EXIF_POINTER, exif_at)?);
        }
        if !self.gps.is_empty() {
            let gps_at = at + bytes.len();
            bytes.extend(self.directory(&self.gps, gps_at)?);
            pointers.push(self.pointer(GPS_POINTER, gps_at)?);
        }
        Some((bytes, pointers))
    }

    /// The directories as one EXIF block.
    fn block(&self) -> Option<Vec<u8>> {
        // The first directory's size does not depend on where its pointers point.
        let placeholders = [
            (!self.exif.is_empty()).then(|| self.pointer(EXIF_POINTER, 0)),
            (!self.gps.is_empty()).then(|| self.pointer(GPS_POINTER, 0)),
        ];
        let with = |pointers: &[Entry]| [self.main.as_slice(), pointers].concat();
        let placeholders: Vec<Entry> = placeholders.into_iter().flatten().flatten().collect();
        let main_size = self.directory(&with(&placeholders), 8)?.len();
        let (below, pointers) = self.below_main(8 + main_size)?;
        let mut block = if self.big_endian {
            b"MM\0*".to_vec()
        } else {
            b"II*\0".to_vec()
        };
        block.extend(self.long(8));
        block.extend(self.directory(&with(&pointers), 8)?);
        block.extend(below);
        Some(block)
    }
}

/// How the maker note of an Apple camera starts. A directory follows, in big-endian byte
/// order, whose offsets count from the start of the note.
const APPLE_NOTE: &[u8] = b"Apple iOS\0\0\x01MM";
/// The two values in an Apple maker note that say how much brighter than standard white a
/// photo's HDR rendition goes. Apple's software computes the headroom of the photo's gain map
/// from them when the gain map does not say it itself.
const APPLE_HDR_TAGS: [u16; 2] = [0x0021, 0x0030];

/// An EXIF block with nothing but what an iPhone photo's gain map needs from `block`: an
/// Apple maker note with the two values that give the HDR headroom. `None` when `block` has
/// no such values.
#[must_use]
pub fn apple_hdr_note(block: &[u8]) -> Option<Vec<u8>> {
    let (reader, first) = Reader::new(block)?;
    let pointers = reader.directory(first, |tag| tag == EXIF_POINTER);
    let exif_at = reader.pointed(&pointers, EXIF_POINTER)?;
    let note = reader
        .directory(exif_at, |tag| tag == MAKER_NOTE)
        .into_iter()
        .next()?
        .value;
    if !note.starts_with(APPLE_NOTE) {
        return None;
    }
    let apple = Reader {
        bytes: &note,
        big_endian: true,
    };
    // Signed fractions, one each.
    let values: Vec<Entry> = apple
        .directory(APPLE_NOTE.len(), |tag| APPLE_HDR_TAGS.contains(&tag))
        .into_iter()
        .filter(|entry| entry.kind == 10 && entry.count == 1)
        .collect();
    if values.len() != APPLE_HDR_TAGS.len() {
        return None;
    }
    let layout = Directories {
        big_endian: true,
        ..Directories::default()
    };
    let mut note = APPLE_NOTE.to_vec();
    note.extend(layout.directory(&values, APPLE_NOTE.len())?);
    Directories {
        exif: vec![Entry {
            tag: MAKER_NOTE,
            kind: 7,
            count: u32::try_from(note.len()).ok()?,
            value: note,
        }],
        ..layout
    }
    .block()
}

/// How much brighter than standard white an iPhone photo's HDR rendition goes, from the two
/// values of its maker note, by the rule Apple documents for them. `block` is an EXIF block
/// as [`apple_hdr_note`] gives it (or any with those values).
#[must_use]
pub fn apple_headroom(block: &[u8]) -> Option<f64> {
    let note = apple_hdr_note(block)?;
    let (reader, first) = Reader::new(&note)?;
    let pointers = reader.directory(first, |tag| tag == EXIF_POINTER);
    let exif_at = reader.pointed(&pointers, EXIF_POINTER)?;
    let note = reader.directory(exif_at, |tag| tag == MAKER_NOTE);
    let apple = Reader {
        bytes: &note.first()?.value,
        big_endian: true,
    };
    let value = |tag: u16| {
        let entry = apple
            .directory(APPLE_NOTE.len(), |found| found == tag)
            .into_iter()
            .next()?;
        let count = i32::from_be_bytes(entry.value.get(..4)?.try_into().ok()?);
        let parts = i32::from_be_bytes(entry.value.get(4..8)?.try_into().ok()?);
        (parts != 0).then(|| f64::from(count) / f64::from(parts))
    };
    let (headroom, gain) = (value(APPLE_HDR_TAGS[0])?, value(APPLE_HDR_TAGS[1])?);
    // Stops of headroom, from the second value, on one of four lines.
    let stops = match (headroom < 1.0, gain <= 0.01) {
        (true, true) => -20.0 * gain + 1.8,
        (true, false) => -0.101 * gain + 1.601,
        (false, true) => -70.0 * gain + 3.0,
        (false, false) => -0.303 * gain + 2.303,
    };
    Some(stops.exp2())
}

/// The EXIF of a TIFF file, gathered from its directories into one EXIF block; `None` when it
/// has none.
#[must_use]
pub fn of_tiff(tiff: &[u8]) -> Option<Vec<u8>> {
    let (reader, first) = Reader::new(tiff)?;
    let directories = reader.directories(first, |tag| MAIN_TAGS.contains(&tag));
    if directories.is_empty() {
        return None;
    }
    directories.block()
}

/// Adds the tags of an EXIF block to a TIFF file: the camera and dates to its first directory,
/// and the EXIF and GPS directories behind the picture, with the first directory pointing to
/// them. The picture data stays where it is. `None` when the file is not a classic TIFF with
/// one of the two byte orders (a `BigTIFF`, for example), or grows past what 32-bit offsets
/// reach.
#[must_use]
pub fn into_tiff(mut tiff: Vec<u8>, block: &[u8]) -> Option<Vec<u8>> {
    let (target, first) = Reader::new(&tiff)?;
    let (source, source_first) = Reader::new(block)?;
    let existing = target.directory(first, |_| true);
    let directories = source
        .directories(source_first, |tag| MAIN_TAGS.contains(&tag))
        .in_order(target.big_endian);
    if directories.is_empty() {
        return Some(tiff);
    }
    // The entries of the first directory as they are, in the file's own bytes: their values
    // stay where they are.
    let count = usize::from(target.short(first)?);
    let raw = tiff.get(first + 2..first + 2 + count * 12)?.to_vec();
    if existing.len() != count {
        return None;
    }
    if tiff.len() % 2 == 1 {
        tiff.push(0);
    }
    // Behind everything else: the directories below the first one, the values of the new
    // tags, and then the first directory again, with the new entries.
    let (below, pointers) = directories.below_main(tiff.len())?;
    tiff.extend(below);
    let added: Vec<Entry> = directories
        .main
        .iter()
        .filter(|entry| existing.iter().all(|old| old.tag != entry.tag))
        .cloned()
        .chain(pointers)
        .collect();
    let mut entries: Vec<(u16, [u8; 12])> = raw
        .as_chunks::<12>()
        .0
        .iter()
        .zip(&existing)
        .filter(|(_, old)| old.tag != EXIF_POINTER && old.tag != GPS_POINTER)
        .map(|(bytes, old)| (old.tag, *bytes))
        .collect();
    for entry in &added {
        let mut bytes = [0u8; 12];
        bytes[..2].copy_from_slice(&directories.short(entry.tag));
        bytes[2..4].copy_from_slice(&directories.short(entry.kind));
        bytes[4..8].copy_from_slice(&directories.long(entry.count));
        if entry.value.len() <= 4 {
            bytes[8..8 + entry.value.len()].copy_from_slice(&entry.value);
        } else {
            bytes[8..].copy_from_slice(&directories.long(u32::try_from(tiff.len()).ok()?));
            tiff.extend_from_slice(&entry.value);
            if tiff.len() % 2 == 1 {
                tiff.push(0);
            }
        }
        entries.push((entry.tag, bytes));
    }
    entries.sort_by_key(|(tag, _)| *tag);
    let new_first = u32::try_from(tiff.len()).ok()?;
    tiff.extend(directories.short(u16::try_from(entries.len()).ok()?));
    for (_, bytes) in &entries {
        tiff.extend_from_slice(bytes);
    }
    tiff.extend([0; 4]);
    tiff[4..8].copy_from_slice(&directories.long(new_first));
    Some(tiff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tag: u16, kind: u16, count: u32, value: &[u8]) -> Entry {
        Entry {
            tag,
            kind,
            count,
            value: value.to_vec(),
        }
    }

    #[test]
    fn the_hdr_values_of_an_apple_maker_note_are_taken_by_themselves() {
        // A maker note as an iPhone writes it: other values around the two that matter.
        let fraction = |count: i32, parts: i32| [count.to_be_bytes(), parts.to_be_bytes()].concat();
        let layout = Directories {
            big_endian: true,
            ..Directories::default()
        };
        let values = [
            entry(0x0001, 9, 1, &14i32.to_be_bytes()),
            entry(0x0021, 10, 1, &fraction(117_926, 114_993)),
            entry(0x002B, 2, 12, b"an identity\0"),
            entry(0x0030, 10, 1, &fraction(769, 456_192)),
        ];
        let mut note = APPLE_NOTE.to_vec();
        note.extend(layout.directory(&values, APPLE_NOTE.len()).unwrap());
        // In a little-endian block, as some programs rewrite it: the note keeps its own order.
        let block = Directories {
            big_endian: false,
            main: vec![entry(0x010F, 2, 6, b"Apple\0")],
            exif: vec![
                entry(0x9003, 2, 20, b"2024:10:06 18:13:19\0"),
                entry(MAKER_NOTE, 7, u32::try_from(note.len()).unwrap(), &note),
            ],
            ..Directories::default()
        }
        .block()
        .unwrap();

        let small = apple_hdr_note(&block).expect("the HDR values");
        // Nothing else of the block is in it: no camera, no date, no identity.
        for gone in [&b"Apple\0"[..], b"2024:10:06", b"an identity"] {
            let found = small.windows(gone.len()).filter(|w| *w == gone).count();
            // The maker note's own name starts with "Apple iOS".
            assert_eq!(found, 0, "{}", String::from_utf8_lossy(gone));
        }
        let (reader, first) = Reader::new(&small).unwrap();
        let directories = reader.directories(first, |tag| tag != EXIF_POINTER);
        assert_eq!(directories.main, []);
        assert_eq!(directories.exif.len(), 1);
        let kept = Reader {
            bytes: &directories.exif[0].value,
            big_endian: true,
        }
        .directory(APPLE_NOTE.len(), |_| true);
        assert_eq!(kept, [values[1].clone(), values[3].clone()]);
        // Taken from its own result, it is the same block.
        assert_eq!(apple_hdr_note(&small), Some(small.clone()));
        // The two values give the headroom: 2^(3 - 70 × 0.0016857) = 7.37, from either block.
        let headroom = apple_headroom(&small).unwrap();
        assert!((headroom - 7.3717).abs() < 0.001, "{headroom}");
        assert_eq!(apple_headroom(&block), Some(headroom));

        // Without the two values, or without an Apple maker note, there is nothing to take.
        let other = Directories {
            big_endian: true,
            exif: vec![entry(MAKER_NOTE, 7, 8, b"Nikon\0\x02\x10")],
            ..Directories::default()
        }
        .block()
        .unwrap();
        assert_eq!(apple_hdr_note(&other), None);
        assert_eq!(apple_hdr_note(b"not EXIF"), None);
        assert_eq!(apple_headroom(&other), None);
    }

    fn sample(big_endian: bool) -> Directories {
        let number = |value: u32| {
            if big_endian {
                value.to_be_bytes()
            } else {
                value.to_le_bytes()
            }
        };
        let short = |value: u16| {
            if big_endian {
                value.to_be_bytes()
            } else {
                value.to_le_bytes()
            }
        };
        Directories {
            big_endian,
            main: vec![
                entry(0x010F, 2, 6, b"Peraz\0"),
                entry(0x0132, 2, 20, b"2026:05:04 10:20:30\0"),
            ],
            exif: vec![
                entry(0x829A, 5, 1, &[number(1), number(250)].concat()),
                entry(0x8827, 3, 1, &short(200)),
                entry(MAKER_NOTE, 7, 5, b"maker"),
            ],
            interop: vec![entry(1, 2, 4, b"R98\0")],
            gps: vec![entry(2, 5, 3, &[52, 1, 30, 1, 0, 1].map(number).concat())],
        }
    }

    #[test]
    fn a_block_reads_back_as_it_was_written() {
        for big_endian in [false, true] {
            let directories = sample(big_endian);
            let block = directories.block().unwrap();
            let (reader, first) = Reader::new(&block).unwrap();
            assert_eq!(first, 8);
            let mut read = reader.directories(first, |tag| MAIN_TAGS.contains(&tag));
            read.exif.sort_by_key(|entry| entry.tag);
            assert_eq!(read, directories, "big endian: {big_endian}");
            // Entries are in tag order, as readers expect.
            let tags: Vec<u16> = reader
                .directory(8, |_| true)
                .iter()
                .map(|e| e.tag)
                .collect();
            assert_eq!(tags, [0x010F, 0x0132, EXIF_POINTER, GPS_POINTER]);
        }
    }

    #[test]
    fn numbers_turn_with_the_byte_order_and_the_makers_data_is_left_out() {
        let turned = sample(true).in_order(false);
        let mut expected = sample(false);
        expected.exif.retain(|entry| entry.tag != MAKER_NOTE);
        assert_eq!(turned, expected);
        // The same order: nothing changes, and the maker's data stays.
        assert_eq!(sample(true).in_order(true), sample(true));
    }

    #[test]
    fn damaged_structures_give_nothing() {
        assert_eq!(of_tiff(b"not a TIFF"), None);
        assert_eq!(of_tiff(b"II*\0\xFF\xFF\xFF\xFF"), None);
        // A BigTIFF is not read, and is not written into.
        assert_eq!(of_tiff(b"II+\0\x08\0\0\0\x10\0\0\0\0\0\0\0"), None);
        let block = sample(false).block().unwrap();
        assert_eq!(into_tiff(b"II+\0\x08\0\0\0".to_vec(), &block), None);
        // A value that points outside the block is left out; the rest is read.
        let mut cut = block.clone();
        cut.truncate(block.len() - 8);
        let (reader, first) = Reader::new(&cut).unwrap();
        let read = reader.directories(first, |_| true);
        assert!(read.main.iter().any(|entry| entry.tag == 0x010F));
        // A directory that names itself as its EXIF directory does not loop.
        let mut looped = b"II*\0\x08\0\0\0\x01\0".to_vec();
        looped.extend([0x69, 0x87, 4, 0, 1, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0]);
        assert!(of_tiff(&looped).is_some());
    }
}
