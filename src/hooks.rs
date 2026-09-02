//! Trampoline hooks. Backend-neutral via ScriptRuntime + symbols.

use crate::runtime::{Domain, Object, ScriptRuntime};
use crate::symbols;
use retour::GenericDetour;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

// Per-frame main-thread driver: a GameWorld MonoBehaviour message, so it runs
// once per frame for as long as a world exists (raid and hideout). No
// GameWorld means no driver, so nothing runs on the menu screens.
//
// IL2CPP note: methodPointer takes a trailing const MethodInfo*. Harmless to a
// detour with fewer params under the MS x64 ABI, but verify against a dump.

type DriverFn = unsafe extern "C" fn(*mut c_void);

static DRIVER_HOOK: OnceLock<GenericDetour<DriverFn>> = OnceLock::new();
static UNLOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

/// GameWorld instance from the driver's `this`. Unused for now: main-thread
/// only, and stale once a raid ends where FindObjectOfType returns null.
static GAME_WORLD: AtomicUsize = AtomicUsize::new(0);

#[allow(dead_code)]
pub fn game_world() -> Object {
    Object(GAME_WORLD.load(Ordering::Relaxed) as *mut c_void)
}

pub fn unload_requested() -> bool {
    UNLOAD_REQUESTED.load(Ordering::Relaxed)
}

/// Threads currently inside driver_detour. Disabling the hook stops new
/// entries but not in-flight ones, and freeing under one returns into a hole.
static IN_DETOUR: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn driver_detour(this: *mut c_void) {
    IN_DETOUR.fetch_add(1, Ordering::AcqRel);

    // Let the game run its own Update first, so the scene is settled.
    if let Some(h) = DRIVER_HOOK.get() {
        unsafe { h.call(this) };
    }
    GAME_WORLD.store(this as usize, Ordering::Relaxed);
    let keep_going =
        catch_unwind(AssertUnwindSafe(|| unsafe { crate::menu::on_frame() })).unwrap_or(true);
    if !keep_going {
        UNLOAD_REQUESTED.store(true, Ordering::Relaxed);
    }

    IN_DETOUR.fetch_sub(1, Ordering::AcqRel);
}

/// Restores patched prologues and waits for in-flight calls to drain. Must
/// complete before FreeLibraryAndExitThread. False means unloading is unsafe.
pub unsafe fn shutdown(timeout: std::time::Duration) -> bool {
    if let Some(h) = DRIVER_HOOK.get() {
        match unsafe { h.disable() } {
            Ok(()) => crate::elog!("[hook] driver detour disabled"),
            Err(e) => {
                crate::elog!("[hook] driver disable FAILED: {e:?} -- unsafe to unload");
                return false;
            }
        }
    }

    let start = std::time::Instant::now();
    loop {
        let n = IN_DETOUR.load(Ordering::Acquire);
        if n == 0 {
            crate::elog!("[hook] detour drained, safe to unload");
            return true;
        }
        if start.elapsed() >= timeout {
            crate::elog!("[hook] {} call(s) still inside the detour after {:?}", n, timeout);
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// First candidate in DRIVER_METHODS that resolves and yields an address.
pub unsafe fn install_driver_hook(rt: &dyn ScriptRuntime, domain: Domain) -> bool {
    if DRIVER_HOOK.get().is_some() {
        return true;
    }
    let Some(gw_cls) = (unsafe { symbols::find_class(rt, domain, symbols::eft::GAME_WORLD) })
    else {
        crate::elog!("[hook] GameWorld class not resolved -- no driver");
        return false;
    };

    for name in symbols::eft::DRIVER_METHODS {
        let Some(method) = (unsafe { rt.method_exact(gw_cls, name, 0) }) else {
            crate::elog!("[hook] driver candidate GameWorld.{} not found", name);
            continue;
        };
        let Some(ptr) = (unsafe { rt.native_ptr(method) }) else {
            crate::elog!("[hook] driver candidate GameWorld.{} has no native address", name);
            continue;
        };
        crate::elog!("[hook] driver candidate GameWorld.{} -> {:p}", name, ptr);

        let target: DriverFn = unsafe { std::mem::transmute::<*mut c_void, DriverFn>(ptr) };
        crate::elog!("[hook] building trampoline for {}...", name);
        let hook = match unsafe { GenericDetour::new(target, driver_detour) } {
            Ok(h) => h,
            Err(e) => {
                crate::elog!("[hook] GenericDetour::new failed for {}: {e:?}", name);
                continue;
            }
        };
        crate::elog!("[hook] trampoline built, enabling...");
        if let Err(e) = unsafe { hook.enable() } {
            crate::elog!("[hook] enable failed for {}: {e:?}", name);
            continue;
        }
        crate::elog!("[hook] enabled, storing...");
        if DRIVER_HOOK.set(hook).is_err() {
            crate::elog!("[hook] driver hook set concurrently, unexpected");
            return false;
        }
        crate::elog!("[hook] driver installed on GameWorld.{} -- arms on next world", name);
        return true;
    }

    crate::elog!("[hook] no usable driver method on GameWorld -- menu unavailable");
    false
}
