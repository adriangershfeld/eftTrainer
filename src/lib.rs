//! eftTrainer: internal trainer for offline SPT/EFU.
//!
//!   lib/menu/hooks    features and plumbing, backend-agnostic
//!   symbols           every game- and engine-specific name
//!   runtime           ScriptRuntime trait
//!   mono_runtime      trait impl over Mono
//!   mono              raw Mono FFI
//!
//! Porting to IL2CPP (EFT 1.0+) means adding il2cpp_runtime.rs and diffing
//! symbols.rs. Nothing above those two should need to change.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{BOOL, HMODULE, TRUE};
use windows::Win32::System::Console::{AllocConsole, FreeConsole};
use windows::Win32::System::LibraryLoader::{FreeLibraryAndExitThread, GetModuleFileNameA};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_END};

mod console;
mod hooks;
mod menu;
mod mono;
mod mono_runtime;
mod runtime;
mod symbols;

use runtime::{Domain, Method, Object, ScriptRuntime};
use symbols::{key, unity};

macro_rules! clog {
    ($($arg:tt)*) => {{
        let s = format!($($arg)*);
        println!("{}", s);
        crate::console::push(s);
    }};
}

fn current_exe_dir() -> Option<std::path::PathBuf> {
    let mut buf = [0u8; 260];
    let len = unsafe { GetModuleFileNameA(None, &mut buf) };
    if len == 0 {
        return None;
    }
    let path_str = std::str::from_utf8(&buf[..len as usize]).ok()?;
    std::path::Path::new(path_str).parent().map(|p| p.to_path_buf())
}

/// QuickEdit means one click in the console blocks every write until it is
/// dismissed, which hangs the main thread inside any detour that prints.
unsafe fn disable_console_quick_edit() {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE, STD_INPUT_HANDLE,
    };
    const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
    const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
    unsafe {
        let Ok(h) = GetStdHandle(STD_INPUT_HANDLE) else { return };
        let mut mode = CONSOLE_MODE(0);
        if GetConsoleMode(h, &mut mode).is_ok() {
            let new = CONSOLE_MODE((mode.0 & !ENABLE_QUICK_EDIT_MODE) | ENABLE_EXTENDED_FLAGS);
            let _ = SetConsoleMode(h, new);
        }
    }
}

/// Resolved once at inject time. The runtime itself lives in runtime's
/// OnceLock so the main-thread menu can reach it without a stack address.
struct Session {
    rt: &'static dyn ScriptRuntime,
    domain: Domain,
    find_object_of_type: Method,
    game_world_type: Object,
    offsets: symbols::Offsets,
}

unsafe fn setup() -> Option<Session> {
    let exe_dir = current_exe_dir()?;
    let managed_dir = exe_dir
        .join("EscapeFromTarkov_Data")
        .join("Managed")
        .to_string_lossy()
        .into_owned();
    clog!("[eftTrainer] managed dir: {}", managed_dir);

    let boxed = unsafe { mono_runtime::detect(&managed_dir) }?;
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

    // Singleton<GameWorld>.Instance is a closed generic with no
    // class_from_name route, so go via FindObjectOfType(Type) instead.
    let obj_cls = unsafe { symbols::find_class(rt, domain, unity::OBJECT) }?;
    let find_object_of_type =
        unsafe { symbols::find_method(rt, obj_cls, unity::FIND_OBJECT_OF_TYPE, 1) }?;
    let game_world_type =
        unsafe { symbols::find_type_object(rt, domain, symbols::eft::GAME_WORLD) }?;
    clog!("[eftTrainer] FindObjectOfType(Type) + typeof(GameWorld) resolved");

    Some(Session { rt, domain, find_object_of_type, game_world_type, offsets })
}

/// None on failure and on "no world loaded" alike; null at the menu is normal.
unsafe fn read_stamina(s: &Session) -> Option<f32> {
    let mut args = [s.game_world_type.raw()];
    let world = unsafe { s.rt.invoke_static(s.find_object_of_type, &mut args) }?;
    if world.is_null() {
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
            let _ = AllocConsole();
            disable_console_quick_edit();

            std::panic::set_hook(Box::new(|info| {
                println!("[panic] {}", info);
            }));

            clog!("[eftTrainer] injected OK -- INSERT toggles menu, END or X unloads");

            let session = match catch_unwind(AssertUnwindSafe(|| setup())) {
                Ok(s) => s,
                Err(_) => {
                    clog!("[eftTrainer] setup panicked (caught)");
                    None
                }
            };
            if session.is_none() {
                clog!("[eftTrainer] no scripting runtime this session -- idling, END to unload");
            }

            if let Some(ref s) = session {
                menu::set_domain(s.domain);
                clog!("[eftTrainer] installing per-frame driver hook...");
                if hooks::install_driver_hook(s.rt, s.domain) {
                    clog!("[eftTrainer] driver armed -- menu builds on first frame with a live GameWorld");
                } else {
                    clog!("[eftTrainer] driver unavailable -- menu will not appear");
                }
            }

            let mut last_printed: Option<f32> = None;
            let mut last_poll = Instant::now();

            // Unload is a handshake and skipping a step crashes the game:
            //   1. END flags it; main thread tears down the UI, returns false
            //   2. disable the detour, wait for it to drain
            //   3. only then free
            let mut unload_deadline: Option<Instant> = None;

            loop {
                let end_key = (GetAsyncKeyState(VK_END.0 as i32) as u16 & 0x8000) != 0;
                if end_key && unload_deadline.is_none() {
                    clog!("[eftTrainer] END pressed, asking main thread to tear down...");
                    menu::request_unload();
                    // No GameWorld means no driver, so no acknowledgement.
                    unload_deadline = Some(Instant::now() + Duration::from_secs(2));
                }

                let acked = hooks::unload_requested();
                let timed_out = unload_deadline.is_some_and(|d| Instant::now() >= d);

                if acked || timed_out {
                    if timed_out && !acked {
                        clog!("[eftTrainer] no driver acknowledgement (no live GameWorld?)");
                    }
                    clog!("[eftTrainer] unloading...");
                    if !hooks::shutdown(Duration::from_secs(2)) {
                        clog!("[eftTrainer] hooks did not shut down cleanly.");
                        clog!("[eftTrainer] NOT unloading -- freeing now would crash the game.");
                        clog!("[eftTrainer] restart the game to clear it.");
                        unload_deadline = None;
                        thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    thread::sleep(Duration::from_millis(100));
                    let _ = FreeConsole();
                    FreeLibraryAndExitThread(hinst, 0);
                }

                if let Some(ref s) = session {
                    if last_poll.elapsed() >= Duration::from_millis(500) {
                        last_poll = Instant::now();
                        let reading = catch_unwind(AssertUnwindSafe(|| read_stamina(s)))
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
        });
    }
    TRUE
}
