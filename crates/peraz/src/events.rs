//! `peraz compress --events`: what happens during a run, one JSON object per line (JSON Lines),
//! for programs that show progress while Peraz works. Documented in docs/json.md.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use peraz_core::{MediaInfo, MediaKind, Report};
use peraz_engine::compress::Observer;
use peraz_engine::ffmpeg::BuildInfo;
use serde::Serialize;

use crate::compress::{Check, Summary};
use crate::{Envelope, SCHEMA_VERSION};

/// One line of output.
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Event<'a> {
    /// Before any file: the inputs, in the order given (`index` refers to it).
    Start {
        ffmpeg: Option<&'a BuildInfo>,
        recipe: Option<&'a str>,
        files: &'a [PathBuf],
    },
    /// A file was read, or could not be; always before its progress and its report.
    FileStart {
        index: usize,
        input: &'a Path,
        kind: Option<MediaKind>,
        input_bytes: Option<u64>,
        duration_seconds: Option<f64>,
    },
    /// How far the work on a file is.
    Progress {
        index: usize,
        input: &'a Path,
        percent: f64,
        eta_seconds: Option<f64>,
    },
    /// A file is done: the same report as in `--json` results.
    FileDone { index: usize, report: &'a Report },
    /// The run is done.
    Done {
        #[serde(skip_serializing_if = "Option::is_none")]
        check: Option<&'a Check>,
        summary: Summary,
    },
}

/// Writes events to standard output, a whole line at a time, from any thread.
pub(crate) struct Events {
    lock: Mutex<()>,
}

impl Events {
    pub(crate) fn new() -> Self {
        Self {
            lock: Mutex::new(()),
        }
    }

    pub(crate) fn emit(&self, event: &Event) {
        let Ok(mut line) = serde_json::to_string(&Envelope {
            schema_version: SCHEMA_VERSION,
            body: event,
        }) else {
            return;
        };
        line.push('\n');
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }
}

/// The shortest time between two progress events of one file.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Turns one file's engine callbacks into events.
pub(crate) struct FileEvents<'a> {
    events: &'a Events,
    index: usize,
    input: &'a Path,
    /// When the first progress came, and how far it was then: the pace for the time left.
    first: Option<(Instant, f64)>,
    last: Option<(Instant, u16)>,
}

impl<'a> FileEvents<'a> {
    pub(crate) fn new(events: &'a Events, index: usize, input: &'a Path) -> Self {
        Self {
            events,
            index,
            input,
            first: None,
            last: None,
        }
    }
}

impl Observer for FileEvents<'_> {
    fn started(&mut self, info: Option<&MediaInfo>) {
        self.events.emit(&Event::FileStart {
            index: self.index,
            input: self.input,
            kind: info.map(|info| info.kind),
            input_bytes: info.map(|info| info.size_bytes),
            duration_seconds: info.and_then(|info| info.duration_seconds),
        });
    }

    fn progress(&mut self, fraction: f64) {
        let now = Instant::now();
        let fraction = fraction.clamp(0.0, 1.0);
        // In tenths of a percent.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 0..=1000
        let tenths = (fraction * 1000.0).round() as u16;
        let (start, start_fraction) = *self.first.get_or_insert((now, fraction));
        if let Some((at, last_tenths)) = self.last {
            let finished = fraction >= 1.0;
            if last_tenths == tenths || (!finished && now - at < PROGRESS_INTERVAL) {
                return;
            }
        }
        self.last = Some((now, tenths));
        // From the pace so far, once there is enough of it to go by.
        let elapsed = (now - start).as_secs_f64();
        let done = fraction - start_fraction;
        let eta_seconds = (elapsed >= 1.0 && done >= 0.01)
            .then(|| ((elapsed * (1.0 - fraction) / done) * 10.0).round() / 10.0);
        self.events.emit(&Event::Progress {
            index: self.index,
            input: self.input,
            percent: f64::from(tenths) / 10.0,
            eta_seconds,
        });
    }
}
