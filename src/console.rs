//! Ring buffer for in-game console text. Worker writes, main thread reads.
//! Every access is try_lock: dropping a line beats blocking the main thread
//! inside a detour.

use std::collections::VecDeque;
use std::sync::Mutex;

const MAX_LINES: usize = 64;

static BUFFER: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Push a line. Dropped rather than blocked if the lock is contended.
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
