//! One flag, so END is answerable no matter where the worker is.
//!
//! Setup can sit in a long metadata scan. Without a cooperative abort the
//! unload key does nothing until it finishes, which turns every iteration into
//! a game restart. Every long loop in the IL2CPP path checks `aborted()`.

use std::sync::atomic::{AtomicBool, Ordering};

static ABORT: AtomicBool = AtomicBool::new(false);

pub fn request_abort() {
    ABORT.store(true, Ordering::SeqCst);
}

pub fn aborted() -> bool {
    ABORT.load(Ordering::SeqCst)
}
