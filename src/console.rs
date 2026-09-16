//! Logging. Three sinks: an allocated console, a file, and a ring buffer for
//! the in-game widget.
//!
//! The console is opened as CONOUT$ rather than through println!, because
//! stdout is not wired up in an injected DLL and a failed write would panic.
//! Note a console in QuickEdit selection mode blocks writers, so if output
//! stops dead, click the window and press Escape.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windows::Win32::System::Console::{AllocConsole, FreeConsole, SetConsoleTitleA};
use windows::core::s;

const MAX_LINES: usize = 64;

static BUFFER: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
static CONSOLE: Mutex<Option<std::fs::File>> = Mutex::new(None);
// Set when a new line lands; lets the menu skip rebuilding the console snapshot
// (a 22-string clone + join) on every frame when the log is quiet.
static DIRTY: AtomicBool = AtomicBool::new(true);

/// True once since the last call, then cleared. The menu only re-reads the log
/// tail when this reports a change.
pub fn take_dirty() -> bool {
    DIRTY.swap(false, Ordering::Relaxed)
}

/// Allocate a console for this process and point a writer at it. Safe to call
/// when one already exists: CONOUT$ just opens the existing buffer.
pub fn attach() {
    unsafe {
        let _ = AllocConsole();
        let _ = SetConsoleTitleA(s!("eftTrainer"));
    }
    let f = std::fs::OpenOptions::new().read(true).write(true).open("CONOUT$");
    if let (Ok(f), Ok(mut g)) = (f, CONSOLE.lock()) {
        *g = Some(f);
    }
}

pub fn detach() {
    if let Ok(mut g) = CONSOLE.lock() {
        *g = None;
    }
    unsafe {
        let _ = FreeConsole();
    }
}

fn to_console(s: &str) {
    let Ok(mut g) = CONSOLE.lock() else { return };
    if let Some(f) = g.as_mut() {
        let _ = writeln!(f, "{s}");
        let _ = f.flush();
    }
}

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
    to_console(s);
    to_file(s);
    push(s);
}

pub fn push(line: impl Into<String>) {
    if let Ok(mut buf) = BUFFER.try_lock() {
        buf.push_back(line.into());
        while buf.len() > MAX_LINES {
            buf.pop_front();
        }
        DIRTY.store(true, Ordering::Relaxed);
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
