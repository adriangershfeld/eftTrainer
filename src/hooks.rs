//! Proof-of-concept for the hooking half of the architecture: a real
//! inline trampoline hook (via `retour`), installed on the actual native
//! pointer `mono_compile_method` hands back for a live JIT-compiled game
//! method -- not a mock, not a test harness, the real thing.
//!
//! Target: `UnityEngine.Debug.Log(object)`. Chosen deliberately as an
//! inert first test, separate from any gameplay-affecting feature: the
//! detour logs a confirmation line and then always calls through to the
//! original via the trampoline, so nothing about how the game actually
//! logs is changed. This proves "resolve by name -> mono_compile_method
//! -> retour hook -> trampoline back to original" end to end without
//! touching anything that matters yet.

use crate::mono::{MonoApi, MonoDomain, MonoImage, MonoObject};
use retour::GenericDetour;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::OnceLock;

type DebugLogFn = unsafe extern "C" fn(*mut MonoObject);

// The hook has to live somewhere retour's trampoline and our detour can
// both reach it for the lifetime of the process -- OnceLock rather than a
// raw `static mut` (which edition 2024 makes deliberately awkward to
// touch, for good reason: a bare mutable static is exactly the kind of
// aliasing footgun this avoids).
static DEBUG_LOG_HOOK: OnceLock<GenericDetour<DebugLogFn>> = OnceLock::new();

/// The actual detour. This runs on whatever thread the game calls
/// Debug.Log from -- possibly the game's main thread, possibly not, but
/// either way NOT a thread we control, and it's re-entering straight back
/// into Mono JIT'd code. Two rules follow from that: never panic across
/// this boundary (an unwind hitting `extern "C"` aborts the process as of
/// modern Rust -- not memory-unsafe, but just as dead as a crash for our
/// purposes), and keep it fast/simple, since this now runs on every real
/// Debug.Log call in the game, not just our own test one.
unsafe extern "C" fn debug_log_detour(message: *mut MonoObject) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        println!("[hook] Debug.Log intercepted (msg obj: {:p})", message);
    }));
    if result.is_err() {
        println!("[hook] debug_log_detour panicked internally (caught, not propagated)");
    }

    if let Some(hook) = DEBUG_LOG_HOOK.get() {
        unsafe { hook.call(message) };
    }
}

/// Resolves `UnityEngine.Debug.Log(object)`, force-JITs it, and installs
/// the trampoline hook on the real native pointer Mono hands back. Safe
/// to call once; a second call is a no-op (OnceLock rejects it) rather
/// than double-hooking, which would corrupt the trampoline chain.
pub unsafe fn install_debug_log_hook(api: &MonoApi, unity_object_image: *mut MonoImage) -> bool {
    if DEBUG_LOG_HOOK.get().is_some() {
        println!("[hook] Debug.Log hook already installed, skipping");
        return true;
    }

    let Some(debug_klass) = (unsafe { api.class(unity_object_image, "UnityEngine", "Debug") }) else {
        println!("[hook] UnityEngine.Debug class not found");
        return false;
    };
    // param_count = 1 is unambiguous here (unlike FindObjectOfType) --
    // Debug.Log has no generic overload, just Log(object) vs the 2-param
    // Log(object, Object context).
    let Some(log_method) = (unsafe { api.find_method_exact(debug_klass, "Log", 1) }) else {
        println!("[hook] Debug.Log(object) method not found");
        return false;
    };
    let Some(log_ptr) = (unsafe { api.compile(log_method) }) else {
        println!("[hook] mono_compile_method for Debug.Log returned null");
        return false;
    };
    println!("[hook] Debug.Log compiled native ptr: {:p}", log_ptr);

    let target: DebugLogFn = unsafe { std::mem::transmute::<*mut c_void, DebugLogFn>(log_ptr) };
    let hook = match unsafe { GenericDetour::new(target, debug_log_detour) } {
        Ok(h) => h,
        Err(e) => {
            println!("[hook] GenericDetour::new failed: {e:?}");
            return false;
        }
    };
    if let Err(e) = unsafe { hook.enable() } {
        println!("[hook] hook.enable() failed: {e:?}");
        return false;
    }

    // OnceLock::set fails only if already set, which the check at the top
    // already ruled out (single-threaded call site during setup).
    if DEBUG_LOG_HOOK.set(hook).is_err() {
        println!("[hook] DEBUG_LOG_HOOK was set concurrently, unexpected");
        return false;
    }
    println!("[hook] Debug.Log hook installed and enabled");
    true
}

/// Calls Debug.Log ourselves with a known string, so the hook's proof
/// doesn't depend on waiting for the game to happen to log something --
/// we trigger it directly and should see our own detour's line appear
/// (from a call the *game's own compiled code path* just ran, hooked).
pub unsafe fn trigger_test_log(api: &MonoApi, domain: *mut MonoDomain, unity_object_image: *mut MonoImage) {
    let Some(debug_klass) = (unsafe { api.class(unity_object_image, "UnityEngine", "Debug") }) else {
        return;
    };
    let Some(log_method) = (unsafe { api.find_method_exact(debug_klass, "Log", 1) }) else {
        return;
    };
    let Some(test_string) = (unsafe { api.new_string(domain, "eftTrainer hook test") }) else {
        println!("[hook] could not allocate test string");
        return;
    };
    println!("[hook] invoking Debug.Log(\"eftTrainer hook test\") to exercise the hook...");
    let mut args = [test_string as *mut c_void];
    unsafe { api.invoke_static(log_method, &mut args) };
}
