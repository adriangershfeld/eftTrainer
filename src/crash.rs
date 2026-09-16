//! Fault logger. A vectored exception handler that, once armed, writes any
//! access violation (address, faulting module, current unload phase) straight
//! to the log file before the process dies. Disarmed during normal play so it
//! never touches the game's own exceptions.
//!
//! It writes with a fresh file handle, not the console mutexes, so it cannot
//! deadlock if the crash happened while we held one.

#![allow(dead_code)]

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, RemoveVectoredExceptionHandler, EXCEPTION_POINTERS,
};

static ARMED: AtomicBool = AtomicBool::new(false);
static PHASE: AtomicUsize = AtomicUsize::new(0);
// Handle from AddVectoredExceptionHandler, so we can remove it before the DLL
// is freed. A left-behind handler points into unmapped code and crashes the
// next exception (e.g. on re-inject).
static VEH_HANDLE: AtomicUsize = AtomicUsize::new(0);

pub const IDLE: usize = 0;
pub const UNLOAD_BEGIN: usize = 1;
pub const CHAMS_RESTORE: usize = 2;
pub const MENU_TEARDOWN: usize = 3;
pub const WORKER_WAIT: usize = 4;
pub const HOOK_SHUTDOWN: usize = 5;
pub const FREE: usize = 6;

fn phase_name(p: usize) -> &'static str {
    match p {
        UNLOAD_BEGIN => "unload-begin",
        CHAMS_RESTORE => "chams-restore",
        MENU_TEARDOWN => "menu-teardown",
        WORKER_WAIT => "worker-wait",
        HOOK_SHUTDOWN => "hook-shutdown",
        FREE => "free-library",
        _ => "idle",
    }
}

/// Start logging faults. Called when unload begins so play is never affected.
pub fn arm() {
    ARMED.store(true, Ordering::SeqCst);
}

pub fn phase(p: usize) {
    PHASE.store(p, Ordering::SeqCst);
    log_line(&format!("[crash] phase -> {}", phase_name(p)));
}

pub unsafe fn install() {
    let h = unsafe { AddVectoredExceptionHandler(1, Some(veh)) };
    if h.is_null() {
        log_line("[crash] fault logger install FAILED");
    } else {
        VEH_HANDLE.store(h as usize, Ordering::SeqCst);
        log_line("[crash] fault logger installed");
    }
}

/// Remove the handler before the DLL is freed. Must run on every unload path,
/// or a re-inject crashes when an exception calls into the unmapped handler.
pub unsafe fn uninstall() {
    ARMED.store(false, Ordering::SeqCst);
    let h = VEH_HANDLE.swap(0, Ordering::SeqCst);
    if h != 0 {
        unsafe { RemoveVectoredExceptionHandler(h as *const _) };
        log_line("[crash] fault logger removed");
    }
}

const ACCESS_VIOLATION: u32 = 0xC0000005;

unsafe extern "system" fn veh(info: *mut EXCEPTION_POINTERS) -> i32 {
    // 0 = EXCEPTION_CONTINUE_SEARCH: we only observe, never swallow.
    if !ARMED.load(Ordering::Relaxed) || info.is_null() {
        return 0;
    }
    let rec = unsafe { (*info).ExceptionRecord };
    if rec.is_null() {
        return 0;
    }
    let code = unsafe { (*rec).ExceptionCode }.0 as u32;
    if code != ACCESS_VIOLATION {
        return 0;
    }
    let ip = unsafe { (*rec).ExceptionAddress } as usize;
    let np = unsafe { (*rec).NumberParameters };
    let (op, target) = if np >= 2 {
        let info = unsafe { (*rec).ExceptionInformation };
        (info[0], info[1])
    } else {
        (99usize, 0usize)
    };
    let ops = match op {
        0 => "read",
        1 => "write",
        8 => "exec",
        _ => "?",
    };
    let p = PHASE.load(Ordering::Relaxed);
    log_line(&format!(
        "[crash] ACCESS_VIOLATION during {}: {} {:#x}",
        phase_name(p), ops, target
    ));
    log_line(&format!("[crash]   ip {:#x} in {}", ip, module_of(ip)));
    log_line(&format!("[crash]   target {:#x} in {}", target, module_of(target)));
    0
}

fn module_of(a: usize) -> String {
    if a == 0 {
        return "null".into();
    }
    for name in ["GameAssembly.dll", "UnityPlayer.dll", "eft_trainer.dll"] {
        if let Some((b, e)) = unsafe { crate::il2cpp::module_bounds(name) } {
            if a >= b && a < e {
                return format!("{}+{:#x}", name, a - b);
            }
        }
    }
    "other".into()
}

fn log_line(s: &str) {
    let path = std::env::temp_dir().join("eftTrainer.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{s}");
    }
}
