//! Marking results as made by Peraz, so the same file is not compressed twice when a folder is
//! dropped or watched again.
//!
//! The mark does not touch the file's contents: it is an extended attribute on macOS and Linux
//! and an alternate data stream on Windows. It records the file's size and modification time,
//! so a result that was edited afterwards no longer counts as marked. Marking is best effort:
//! on a file system without such attributes, nothing is marked and nothing is skipped.

use std::path::Path;
use std::time::UNIX_EPOCH;

/// The name of the mark: an extended attribute in the user namespace on macOS and Linux, a
/// stream name on Windows.
#[cfg(unix)]
const NAME: &str = "user.peraz.compressed";
#[cfg(windows)]
const NAME: &str = "peraz.compressed";

/// What identifies the file's current contents: its size and when it was last modified.
fn stamp(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(format!("1 {} {}", metadata.len(), modified.as_nanos()))
}

/// Marks `path` as a result Peraz made. Failures are ignored: the file is fine without it.
pub fn mark(path: &Path) {
    if let Some(stamp) = stamp(path) {
        write(path, &stamp);
    }
}

/// Whether `path` is marked as a result Peraz made and has not changed since.
pub fn is_current(path: &Path) -> bool {
    read(path).is_some_and(|marked| Some(marked) == stamp(path))
}

#[cfg(unix)]
fn write(path: &Path, stamp: &str) {
    let _ = xattr::set(path, NAME, stamp.as_bytes());
}

#[cfg(unix)]
fn read(path: &Path) -> Option<String> {
    String::from_utf8(xattr::get(path, NAME).ok()??).ok()
}

/// The alternate data stream of `path` that holds the mark.
#[cfg(windows)]
fn stream(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(":");
    name.push(NAME);
    name.into()
}

#[cfg(windows)]
fn write(path: &Path, stamp: &str) {
    // Writing a stream counts as modifying the file: put the modification time back, so the
    // stamp still matches.
    let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        return;
    };
    if std::fs::write(stream(path), stamp).is_ok()
        && let Ok(file) = std::fs::File::options().write(true).open(path)
    {
        let _ = file.set_modified(modified);
    }
}

#[cfg(windows)]
fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(stream(path)).ok()
}

#[cfg(not(any(unix, windows)))]
fn write(_: &Path, _: &str) {}

#[cfg(not(any(unix, windows)))]
fn read(_: &Path) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mark_holds_until_the_file_changes() {
        let dir = std::env::temp_dir().join(format!("peraz-mark-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("result.jpg");
        std::fs::write(&file, b"a compressed picture").unwrap();
        assert!(!is_current(&file), "a new file is not marked");
        mark(&file);
        if !is_current(&file) {
            // A file system without extended attributes: nothing is marked, nothing skipped.
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }
        // The contents are untouched.
        assert_eq!(std::fs::read(&file).unwrap(), b"a compressed picture");
        // An edit of the same length still changes the modification time.
        let earlier = std::fs::metadata(&file).unwrap().modified().unwrap();
        let edited = std::fs::File::options().write(true).open(&file).unwrap();
        edited
            .set_modified(earlier + std::time::Duration::from_secs(2))
            .unwrap();
        drop(edited);
        assert!(!is_current(&file), "a changed time is a changed file");
        mark(&file);
        assert!(is_current(&file));
        // And so does a change of size.
        std::fs::write(&file, b"edited afterwards, longer").unwrap();
        assert!(!is_current(&file));
        assert!(!is_current(&dir.join("missing.jpg")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
