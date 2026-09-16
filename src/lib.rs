//! eftTrainer: internal trainer for offline SPT/EFU.
//!
//!   lib/menu/hooks    features and plumbing, backend-agnostic
//!   symbols           every game- and engine-specific name
//!   runtime           ScriptRuntime trait
//!   il2cpp_runtime    trait impl over IL2CPP
//!   il2cpp_abi        struct layout, calibrated at load
//!   il2cpp            raw IL2CPP FFI (genuine exports only)
//!
//! IL2CPP only. The Mono backend was removed once EFU moved to 1.0+; the
//! trait boundary it established is what made this a two-file swap.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{BOOL, HMODULE, TRUE};
use windows::Win32::System::LibraryLoader::FreeLibraryAndExitThread;
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_END};

mod chams;
mod console;
mod control;
mod crash;
mod features;
mod hooks;
mod http;
mod il2cpp;
mod il2cpp_abi;
mod il2cpp_runtime;
mod input;
mod menu;
mod runtime;
mod symbols;
mod world;

use runtime::{Domain, Object, ScriptRuntime};
use symbols::key;

macro_rules! clog {
    ($($arg:tt)*) => {{
        crate::console::emit(&format!($($arg)*));
    }};
}

/// Resolved once at inject time. The runtime itself lives in runtime's
/// OnceLock so the main-thread menu can reach it without a stack address.
struct Session {
    rt: &'static dyn ScriptRuntime,
    domain: Domain,
    offsets: symbols::Offsets,
}

unsafe fn setup() -> Option<Session> {
    let boxed = unsafe { il2cpp_runtime::detect() }?;
    if !runtime::install(boxed) {
        clog!("[eftTrainer] runtime already installed, unexpected");
    }
    let rt = runtime::get()?;
    clog!("[eftTrainer] runtime: {}", rt.backend().name());

    let domain = unsafe { rt.root_domain() };
    if domain.is_null() {
        clog!("[eftTrainer] root domain is null");
        return None;
    }
    unsafe { rt.attach_thread(domain) };
    clog!("[eftTrainer] domain {:p}, thread attached", domain.raw());

    let offsets = unsafe { symbols::resolve_fields(rt, domain, symbols::STAMINA_CHAIN) };
    clog!(
        "[eftTrainer] resolved {}/{} stamina chain fields",
        offsets.len(),
        symbols::STAMINA_CHAIN.len()
    );

    Some(Session { rt, domain, offsets })
}

/// None on failure and on "no world loaded" alike; null at the menu is normal.
///
/// The GameWorld comes from the driver hook's captured `this` (set on the main
/// thread each frame), not FindObjectOfType. That keeps this worker-thread poll
/// to plain memory reads: calling a Unity API off the main thread is what used
/// to make this path fragile.
unsafe fn read_stamina(s: &Session) -> Option<f32> {
    let world = crate::hooks::game_world();
    if world.is_null() || !unsafe { runtime::is_alive(world) } {
        return None;
    }

    let player: Object = Object(unsafe { runtime::read_field(world, s.offsets.get(key::MAIN_PLAYER)?) });
    if player.is_null() {
        return None;
    }
    let physical: Object = Object(unsafe { runtime::read_field(player, s.offsets.get(key::PHYSICAL)?) });
    if physical.is_null() {
        return None;
    }
    let stamina: Object = Object(unsafe { runtime::read_field(physical, s.offsets.get(key::STAMINA)?) });
    if stamina.is_null() {
        return None;
    }
    Some(unsafe { runtime::read_field(stamina, s.offsets.get(key::STAMINA_CURRENT)?) })
}

#[unsafe(no_mangle)]
#[allow(non_snake_case, clippy::not_unsafe_ptr_arg_deref)]
extern "system" fn DllMain(hinst: HMODULE, reason: u32, _reserved: *mut core::ffi::c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        // HMODULE isn't Send, so it crosses as a usize.
        let hinst_raw = hinst.0 as usize;
        thread::spawn(move || unsafe {
            let hinst = HMODULE(hinst_raw as *mut core::ffi::c_void);

            console::attach();
            std::panic::set_hook(Box::new(|info| {
                console::emit(&format!("[panic] {info}"));
            }));
            crash::install();

            clog!("[eftTrainer] injected OK -- INSERT toggles menu, END or X unloads");
            clog!("[eftTrainer] log file: {}", console::log_path().display());

            // Setup runs on its own thread so END stays answerable while a long
            // metadata scan is in progress. This thread only supervises.
            let worker = thread::spawn(|| worker_main());

            // Unload is a handshake and skipping a step crashes the game:
            //   1. END flags it; the worker unwinds and the UI tears down
            //   2. disable the detour, wait for it to drain
            //   3. only then free
            let mut unload_deadline: Option<Instant> = None;

            loop {
                // Two unload triggers: END on this thread, or the menu's X
                // button, which lands as a driver ack. Both must abort the
                // worker -- the X path used to set the ack without aborting,
                // so the worker's `while !aborted()` loop ran forever and the
                // unload never completed.
                let end_key = (GetAsyncKeyState(VK_END.0 as i32) as u16 & 0x8000) != 0;
                let x_button = hooks::unload_requested();
                if (end_key || x_button) && unload_deadline.is_none() {
                    clog!(
                        "[eftTrainer] unload requested ({}) -- unwinding worker and tearing down...",
                        if end_key { "END" } else { "X button" }
                    );
                    crash::arm();
                    crash::phase(crash::UNLOAD_BEGIN);
                    control::request_abort();
                    menu::request_unload();
                    // No GameWorld means no driver, so no acknowledgement.
                    unload_deadline = Some(Instant::now() + Duration::from_secs(3));
                }

                let acked = hooks::unload_requested();
                let timed_out = unload_deadline.is_some_and(|d| Instant::now() >= d);

                if acked || timed_out {
                    if timed_out && !acked {
                        clog!("[eftTrainer] no driver acknowledgement (no live GameWorld?)");
                    }
                    // The worker is running our code; freeing under it crashes.
                    crash::phase(crash::WORKER_WAIT);
                    let wait_until = Instant::now() + Duration::from_secs(5);
                    while !worker.is_finished() && Instant::now() < wait_until {
                        thread::sleep(Duration::from_millis(50));
                    }
                    if !worker.is_finished() {
                        clog!("[eftTrainer] worker still running -- NOT unloading yet, will retry");
                        unload_deadline = Some(Instant::now() + Duration::from_secs(3));
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                    clog!("[eftTrainer] unloading...");
                    crash::phase(crash::HOOK_SHUTDOWN);
                    if !hooks::shutdown(Duration::from_secs(2)) {
                        clog!("[eftTrainer] hooks did not shut down cleanly.");
                        clog!("[eftTrainer] NOT unloading -- freeing now would crash the game.");
                        clog!("[eftTrainer] restart the game to clear it.");
                        unload_deadline = None;
                        thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    crash::phase(crash::FREE);
                    crash::uninstall(); // remove the VEH before the DLL is freed
                    console::detach();
                    thread::sleep(Duration::from_millis(100));
                    FreeLibraryAndExitThread(hinst, 0);
                }

                thread::sleep(Duration::from_millis(50));
            }
        });
    }
    TRUE
}

/// Everything that touches the runtime. Kept on its own thread because
/// Session holds managed object handles, which are deliberately not Send,
/// and because calibration can run long enough to need interrupting.
unsafe fn worker_main() {
    let session = match catch_unwind(AssertUnwindSafe(|| unsafe { setup() })) {
        Ok(s) => s,
        Err(_) => {
            clog!("[eftTrainer] setup panicked (caught)");
            None
        }
    };

    if control::aborted() {
        clog!("[eftTrainer] worker aborted during setup");
        return;
    }
    if session.is_none() {
        clog!("[eftTrainer] no scripting runtime this session -- idling, END to unload");
    }

    let mut http_handle: Option<thread::JoinHandle<()>> = None;
    if let Some(ref s) = session {
        menu::set_domain(s.domain);
        clog!("[eftTrainer] installing per-frame driver hook...");
        if unsafe { hooks::install_driver_hook(s.rt, s.domain) } {
            clog!("[eftTrainer] driver armed -- menu builds on first frame with a live GameWorld");
        } else {
            clog!("[eftTrainer] driver unavailable -- menu will not appear");
        }
        // Live introspection/control over loopback HTTP (curl-friendly).
        http_handle = Some(http::start(s.domain.raw() as usize));
    }

    let mut last_printed: Option<f32> = None;
    let mut last_poll = Instant::now();

    while !control::aborted() {
        if let Some(ref s) = session {
            if last_poll.elapsed() >= Duration::from_millis(500) {
                last_poll = Instant::now();
                let reading = catch_unwind(AssertUnwindSafe(|| unsafe { read_stamina(s) }))
                    .unwrap_or_else(|_| {
                        clog!("[eftTrainer] stamina poll panicked (caught)");
                        None
                    });
                // Status row, not clog!, or these evict the startup log.
                match reading {
                    Some(v) if Some(v) != last_printed => {
                        menu::set_status(format!("stamina  {:.1}", v));
                        last_printed = Some(v);
                    }
                    None if last_printed.is_some() => {
                        menu::set_status("no active GameWorld/player".to_string());
                        clog!("[eftTrainer] world gone (raid ended or menu)");
                        last_printed = None;
                    }
                    _ => {}
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    // The HTTP thread runs our code too; it must be gone before the supervisor
    // frees the library. It exits on control::aborted(), so this joins quickly.
    if let Some(h) = http_handle.take() {
        let _ = h.join();
    }
    clog!("[eftTrainer] worker exiting");
}
