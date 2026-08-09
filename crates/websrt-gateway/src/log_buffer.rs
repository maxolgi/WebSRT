//! Ring buffer + tracing-compatible writer for capturing log lines in the GUI.
//!
//! Used as a second `tracing_subscriber::fmt` layer (alongside stderr) so the
//! GUI log panel shows the same formatted output the CLI user would see.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// Thread-safe ring buffer of log lines, shared between the tracing layer and
/// the GUI.
pub struct LogBuffer {
    lines: Mutex<VecDeque<String>>,
    max: usize,
}

impl LogBuffer {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            lines: Mutex::new(VecDeque::with_capacity(max)),
            max,
        })
    }

    pub fn push(&self, line: String) {
        let mut g = self.lines.lock().unwrap();
        if g.len() >= self.max {
            g.pop_front();
        }
        g.push_back(line);
    }

    /// Return the last `n` lines in chronological order.
    pub fn recent(&self, n: usize) -> Vec<String> {
        let g = self.lines.lock().unwrap();
        let start = g.len().saturating_sub(n);
        g.iter().skip(start).cloned().collect()
    }
}

/// `MakeWriter` impl that produces a `BufferWriter` for each log event.
/// The formatted line (from `tracing_subscriber::fmt::layer`) is accumulated
/// in the writer and flushed to the `LogBuffer` on newline / drop.
pub struct BufferMaker {
    pub buffer: Arc<LogBuffer>,
}

pub struct BufferWriter {
    buffer: Arc<LogBuffer>,
    pending: String,
}

impl Write for BufferWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let s = std::str::from_utf8(buf).unwrap_or("");
        self.pending.push_str(s);
        while let Some(pos) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=pos).collect();
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                self.buffer.push(trimmed.to_string());
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for BufferWriter {
    fn drop(&mut self) {
        if !self.pending.trim_end().is_empty() {
            self.buffer.push(self.pending.trim_end().to_string());
        }
    }
}

impl<'a> MakeWriter<'a> for BufferMaker {
    type Writer = BufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        BufferWriter {
            buffer: self.buffer.clone(),
            pending: String::new(),
        }
    }
}
