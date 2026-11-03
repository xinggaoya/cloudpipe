//! The `cloudflared` stderr pump.
//!
//! One task per `cloudflared` generation, with two jobs:
//!
//! 1. turn stderr lines into SDK events, and
//! 2. keep a bounded **tail** of the most recent lines.
//!
//! The tail is what makes a crash explainable. Before it existed, the
//! supervisor could only report "cloudflared exited" — the reason lived
//! in the child's stderr, which the pump consumed and threw away. A user
//! whose `cloudflared` died on startup saw an endless restart loop and
//! no reason at all, because the fatal message did not survive the
//! pump.
//!
//! The tail is a plain `std::sync::Mutex` over a `VecDeque`: it is
//! locked only for the duration of a `push` or a `snapshot`, never
//! across an `.await`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStderr;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::binary::{self, LineKind};
use crate::event::{Event, LogLevel};

/// How many stderr lines are retained for post-mortem reporting.
///
/// Enough for a fatal message plus its context; bounded so a chatty
/// `cloudflared` cannot grow the buffer without limit.
pub(crate) const TAIL_CAPACITY: usize = 16;

/// The most recent stderr lines, oldest first.
#[derive(Debug, Default)]
pub(crate) struct StderrTail {
    lines: VecDeque<String>,
}

impl StderrTail {
    /// Appends a line, dropping the oldest one when full.
    fn push(&mut self, line: String) {
        if self.lines.len() >= TAIL_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// The retained lines, newest last, joined by newlines.
    pub(crate) fn snapshot(&self) -> String {
        self.lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

/// A running pump and the tail it fills.
pub(crate) struct StderrPump {
    /// The pump task. Awaiting it lets the caller wait for the child's
    /// stderr to reach EOF *before* reading the tail — otherwise the
    /// last line (the fatal message) can still be in flight.
    pub(crate) task: JoinHandle<()>,
    /// Shared with the supervisor, which reads the tail when the child
    /// exits.
    pub(crate) tail: Arc<StdMutex<StderrTail>>,
}

/// Starts the pump for one `cloudflared` generation.
///
/// The pump ends when the child's stderr reaches EOF, i.e. when the
/// child exits — which is exactly when the supervisor wants the tail.
pub(crate) fn spawn(stderr: ChildStderr, event_tx: broadcast::Sender<Event>) -> StderrPump {
    let tail = Arc::new(StdMutex::new(StderrTail::default()));
    let tail_for_task = Arc::clone(&tail);

    let connections = Arc::new(AtomicUsize::new(0));
    let connections_for_task = Arc::clone(&connections);

    let task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    // Every line is retained, whatever its classification:
                    // the tail is a post-mortem artefact, and the line
                    // that explains a crash is often a plain `INF` line
                    // sitting next to the fatal one.
                    if let Ok(mut guard) = tail_for_task.lock() {
                        guard.push(trimmed.to_string());
                    }
                    match binary::classify_line(trimmed) {
                        LineKind::Connection => {
                            let total = connections_for_task.fetch_add(1, Ordering::SeqCst) + 1;
                            let _ = event_tx.send(Event::EdgeConnected {
                                conn_index: total.saturating_sub(1) as u8,
                                total: total as u8,
                            });
                        }
                        LineKind::Log(level) => {
                            let _ = event_tx.send(Event::CloudflaredLog {
                                level,
                                line: trimmed.to_string(),
                            });
                        }
                        LineKind::Error => {
                            let _ = event_tx.send(Event::CloudflaredLog {
                                level: LogLevel::Error,
                                line: trimmed.to_string(),
                            });
                        }
                        LineKind::Ignore => {}
                    }
                }
            }
        }
    });

    StderrPump { task, tail }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_keeps_the_newest_lines() {
        let mut tail = StderrTail::default();
        for i in 0..(TAIL_CAPACITY + 5) {
            tail.push(format!("line {i}"));
        }
        let snapshot = tail.snapshot();
        let lines: Vec<&str> = snapshot.lines().collect();
        assert_eq!(lines.len(), TAIL_CAPACITY);
        assert_eq!(lines[0], "line 5");
        assert_eq!(lines[TAIL_CAPACITY - 1], "line 20");
    }

    #[test]
    fn an_empty_tail_snapshots_to_an_empty_string() {
        assert_eq!(StderrTail::default().snapshot(), "");
    }

    #[test]
    fn the_tail_keeps_a_fatal_message_that_follows_noise() {
        // The exact shape of the failure this module exists for: a
        // fatal message is the *last* thing the child printed.
        let mut tail = StderrTail::default();
        tail.push("2026-01-01 INF Starting tunnel".into());
        tail.push(
            "Failed to read token file: open /tmp/x: The system cannot find the file specified."
                .into(),
        );
        assert!(tail.snapshot().contains("Failed to read token file"));
    }
}
