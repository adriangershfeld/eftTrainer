//! Logging. Two sinks: a file, and a ring buffer for the in-game widget.
//!
//! No stdout. There is no console attached, so println! would panic on a
//! failed write, and a console write can block forever anyway once the window
//! is in selection mode.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Mutex;

const MAX_LINES: usize = 64;

static BUFFER: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn log_path() -> std::path::PathBuf {
    std::env::temp_dir().join("eftTrainer.log")
}

fn to_file(s: &str) {
    let Ok(mut g) = FILE.lock() else { return };
    if g.is_none() {
        *g = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())
            .ok();
    }
    if let Some(f) = g.as_mut() {
        let _ = writeln!(f, "{s}");
        let _ = f.flush();
    }
}

pub fn emit(s: &str) {
    to_file(s);
    push(s);
}

pub fn push(line: impl Into<String>) {
    if let Ok(mut buf) = BUFFER.try_lock() {
        buf.push_back(line.into());
        while buf.len() > MAX_LINES {
            buf.pop_front();
        }
    }
}

/// Last `n` lines, newline-joined. None on a contended lock, not "" -- the
/// caller would render an empty string as the content and blank the widget.
pub fn snapshot_tail(n: usize) -> Option<String> {
    let buf = BUFFER.try_lock().ok()?;
    let skip = buf.len().saturating_sub(n);
    Some(buf.iter().skip(skip).cloned().collect::<Vec<_>>().join("\n"))
}

#[macro_export]
macro_rules! elog {
    ($($arg:tt)*) => {{
        $crate::console::emit(&format!($($arg)*));
    }};
}
