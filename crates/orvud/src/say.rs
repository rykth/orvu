//! Writing to standard error without ever waiting on it.
//!
//! Whoever started the sidecar may stop reading its standard error, or go
//! away. A plain `eprintln!` then either fails, which makes it panic, or
//! waits until somebody reads, which may be never. Either way the sidecar
//! could no longer stop: a sidecar was seen ignoring SIGTERM for good because
//! the task that waits for the signal had panicked on a line it could not
//! print.
//!
//! So lines are handed to a thread of their own, which writes them. Handing a
//! line over never waits: when the thread is stuck and its queue is full, the
//! line is dropped. A line that cannot be written is dropped too. The log,
//! when it goes to standard error, is written the same way (see `Writer`).

use std::io::Write;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

/// How many lines may wait to be written. Past that, lines are dropped.
const WAITING: usize = 1024;

struct Queue {
    lines: SyncSender<Vec<u8>>,
    /// How many lines were handed over and are not written yet.
    pending: Arc<(Mutex<usize>, Condvar)>,
}

fn queue() -> &'static Queue {
    static QUEUE: OnceLock<Queue> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (lines, waiting) = sync_channel::<Vec<u8>>(WAITING);
        let pending = Arc::new((Mutex::new(0usize), Condvar::new()));
        let written = pending.clone();
        // Should the thread not start, nothing is written: every line is
        // dropped at once, as nobody is there to take it.
        let _ = std::thread::Builder::new().name("stderr".into()).spawn(move || {
            let mut stderr = std::io::stderr();
            for line in waiting {
                // A line that cannot be written is let go.
                let _ = stderr.write_all(&line);
                let (count, changed) = &*written;
                *count.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
                changed.notify_all();
            }
        });
        Queue { lines, pending }
    })
}

fn hand_over(line: Vec<u8>) {
    let queue = queue();
    let (count, _) = &*queue.pending;
    let mut pending = count.lock().unwrap_or_else(|e| e.into_inner());
    if queue.lines.try_send(line).is_ok() {
        *pending += 1;
    }
}

/// Writes `text` and a line break to standard error, soon. It never waits,
/// never fails and never panics; a line nobody can take is dropped.
pub fn line(text: impl std::fmt::Display) {
    hand_over(format!("{text}\n").into_bytes());
}

/// Waits until every line handed over so far is written, or `limit` has
/// passed, whichever comes first. Call it before the program exits, so the
/// last lines are not lost while standard error is still being read.
pub fn flush(limit: Duration) {
    let (count, changed) = &*queue().pending;
    let pending = count.lock().unwrap_or_else(|e| e.into_inner());
    let _ = changed.wait_timeout_while(pending, limit, |pending| *pending > 0);
}

/// A log writer for standard error that never waits, for `tracing`. Each log
/// line is handed over whole once it is written.
#[derive(Clone, Copy, Default)]
pub struct Writer;

/// One log line being written. It is handed over when dropped.
pub struct Line(Vec<u8>);

impl Write for Line {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            hand_over(std::mem::take(&mut self.0));
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Writer {
    type Writer = Line;
    fn make_writer(&'a self) -> Line {
        Line(Vec::new())
    }
}
