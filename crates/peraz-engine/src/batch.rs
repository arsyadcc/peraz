//! Shared limits for compressing many files at once.
//!
//! Work runs in two lanes. The encode lane (videos, GIFs, animated images) holds few slots
//! because each FFmpeg encode already uses many cores; the image lane (still images) holds about
//! one slot per core. A batch also records every output path it hands out, so two inputs can
//! never write the same file at the same time.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

/// Which kind of work a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// FFmpeg encodes: videos, GIFs, animated WebP/AVIF.
    Encode,
    /// In-process still-image encoding.
    Image,
}

/// Limits and output reservations shared by every job of a batch.
#[derive(Debug)]
pub struct Batch {
    encode: Slots,
    image: Slots,
    claimed: Mutex<HashSet<PathBuf>>,
}

impl Batch {
    /// A batch allowing `encode` concurrent encodes and `image` concurrent still images.
    pub fn new(encode: usize, image: usize) -> Self {
        Self {
            encode: Slots::new(encode.max(1)),
            image: Slots::new(image.max(1)),
            claimed: Mutex::default(),
        }
    }

    /// Limits that suit this machine: one encode per 8 logical cores (at least one), one still
    /// image per logical core.
    pub fn for_this_machine() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        Self::new((cores / 8).max(1), cores)
    }

    /// No limits; for a single job.
    pub fn unlimited() -> Self {
        Self::new(usize::MAX, usize::MAX)
    }

    /// Waits for a slot in `lane`. Returns `None` if `cancel` is set while waiting.
    pub fn acquire<'a>(&'a self, lane: Lane, cancel: &AtomicBool) -> Option<Permit<'a>> {
        let slots = match lane {
            Lane::Encode => &self.encode,
            Lane::Image => &self.image,
        };
        let mut used = slots.used.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if cancel.load(Ordering::SeqCst) {
                return None;
            }
            if *used < slots.limit {
                *used += 1;
                return Some(Permit { slots });
            }
            // Wake up now and then to notice cancellation.
            used = slots
                .freed
                .wait_timeout(used, Duration::from_millis(100))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Reserves `output` for one job. Returns false when another job of this batch has it.
    pub fn claim(&self, output: &Path) -> bool {
        self.claimed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(normalize(output))
    }
}

/// An absolute path with `.` and `..` resolved lexically (the file may not exist yet).
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[derive(Debug)]
struct Slots {
    limit: usize,
    used: Mutex<usize>,
    freed: Condvar,
}

impl Slots {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            used: Mutex::new(0),
            freed: Condvar::new(),
        }
    }
}

/// A slot in a lane, released when dropped.
#[derive(Debug)]
pub struct Permit<'a> {
    slots: &'a Slots,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut used = self
            .slots
            .used
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *used -= 1;
        self.slots.freed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn lanes_never_exceed_their_limits() {
        let batch = Batch::new(1, 3);
        let cancel = AtomicBool::new(false);
        let (running, peak_encode, peak_image) = (
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        );
        let running_image = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for i in 0..12 {
                let lane = if i % 3 == 0 {
                    Lane::Encode
                } else {
                    Lane::Image
                };
                let (batch, cancel, running, running_image) =
                    (&batch, &cancel, &running, &running_image);
                let (peak_encode, peak_image) = (&peak_encode, &peak_image);
                scope.spawn(move || {
                    let _permit = batch.acquire(lane, cancel).unwrap();
                    let (counter, peak) = match lane {
                        Lane::Encode => (running, peak_encode),
                        Lane::Image => (running_image, peak_image),
                    };
                    let now = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    counter.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        assert_eq!(peak_encode.load(Ordering::SeqCst), 1);
        assert!(peak_image.load(Ordering::SeqCst) <= 3);
    }

    #[test]
    fn cancelling_releases_waiters() {
        let batch = Batch::new(1, 1);
        let cancel = AtomicBool::new(false);
        let held = batch.acquire(Lane::Encode, &cancel).unwrap();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| batch.acquire(Lane::Encode, &cancel).is_none());
            std::thread::sleep(Duration::from_millis(50));
            cancel.store(true, Ordering::SeqCst);
            assert!(
                waiter.join().unwrap(),
                "the waiter gives up on cancellation"
            );
        });
        drop(held);
    }

    #[test]
    fn outputs_are_claimed_once() {
        let batch = Batch::unlimited();
        assert!(batch.claim(Path::new("out/a-peraz.webp")));
        assert!(!batch.claim(Path::new("out/a-peraz.webp")));
        assert!(
            !batch.claim(Path::new("out/../out/./a-peraz.webp")),
            "same file, other spelling"
        );
        assert!(batch.claim(Path::new("out/b-peraz.webp")));
    }
}
