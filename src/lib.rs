use std::panic::{catch_unwind, AssertUnwindSafe};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{BOOL, HMODULE, TRUE};
use windows::Win32::System::Console::{AllocConsole, FreeConsole};
use windows::Win32::System::LibraryLoader::{FreeLibraryAndExitThread, GetModuleFileNameA};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_END};

mod hooks;
mod mono;

/// Directory containing the running exe, so we can find
/// `EscapeFromTarkov_Data\Managed\*.dll` relative to it instead of
/// hardcoding an install path.
fn current_exe_dir() -> Option<std::path::PathBuf> {
    let mut buf = [0u8; 260]; // MAX_PATH
    let len = unsafe { GetModuleFileNameA(None, &mut buf) };
    if len == 0 {
        return None;
    }
    let path_str = std::str::from_utf8(&buf[..len as usize]).ok()?;
    std::path::Path::new(path_str).parent().map(|p| p.to_path_buf())
}

/// Everything the periodic stamina readout needs, resolved once at inject
/// time so the main loop's per-tick work is just one invoke plus three
/// pointer-chases -- no class/method lookups happen after setup.
struct MonoState {
    api: mono::MonoApi,
    find_object_of_type: *mut mono::MonoMethod,
    game_world_type_obj: *mut mono::MonoObject,
    table: mono::ResolverTable,
}

/// One-time setup: opens Assembly-CSharp.dll, resolves the stamina field
/// chain, opens UnityEngine.CoreModule.dll, resolves the (disambiguated)
/// FindObjectOfType(Type) method plus a cached typeof(GameWorld) object,
/// and installs the Debug.Log hook proof-of-concept. Every step logs what
/// it did, specifically so that if this crashes again, the last printed
/// line says exactly which call didn't come back.
unsafe fn setup_mono_state() -> Option<MonoState> {
    let exe_dir = current_exe_dir()?;
    let managed_dir = exe_dir.join("EscapeFromTarkov_Data").join("Managed");
    let assembly_csharp_path = managed_dir.join("Assembly-CSharp.dll");

    println!("[eftTrainer] mono smoke test starting: {}", assembly_csharp_path.display());
    let (api, domain, assembly_csharp_image) =
        unsafe { mono::smoke_test(&assembly_csharp_path.to_string_lossy()) }?;
    println!("[eftTrainer] mono smoke test: OK, resolution pipeline is live");

    let table = unsafe { mono::resolve(&api, domain, assembly_csharp_image, mono::STAMINA_CHAIN_FIELDS, &[]) };

    println!("[eftTrainer] opening UnityEngine.CoreModule.dll...");
    let unity_path = managed_dir.join("UnityEngine.CoreModule.dll");
    let Some(unity_assembly) = (unsafe { api.open_assembly(domain, &unity_path.to_string_lossy()) }) else {
        println!("[eftTrainer] could not open UnityEngine.CoreModule.dll, stamina readout disabled");
        return None;
    };
    println!("[eftTrainer] opened UnityEngine.CoreModule.dll");
    let unity_object_image = unsafe { api.image(unity_assembly) };

    println!("[eftTrainer] resolving FindObjectOfType(Type)...");
    let Some(find_object_of_type) = (unsafe { mono::resolve_find_object_of_type(&api, unity_object_image) }) else {
        println!("[eftTrainer] could not resolve FindObjectOfType(Type), stamina readout disabled");
        return None;
    };

    println!("[eftTrainer] resolving typeof(EFT.GameWorld)...");
    let Some(game_world_type_obj) =
        (unsafe { mono::resolve_type_object(&api, domain, assembly_csharp_image, "EFT", "GameWorld") })
    else {
        println!("[eftTrainer] could not resolve typeof(EFT.GameWorld), stamina readout disabled");
        return None;
    };

    // Hooking proof-of-concept: inert (Debug.Log still logs normally
    // through the trampoline), but a real inline hook on a real
    // JIT-compiled game method -- the other half of the "resolve by name,
    // hook the pointer Mono hands back" architecture, exercised now
    // separately from any actual gameplay feature.
    println!("[eftTrainer] installing Debug.Log hook (proof-of-concept, inert)...");
    if unsafe { hooks::install_debug_log_hook(&api, unity_object_image) } {
        unsafe { hooks::trigger_test_log(&api, domain, unity_object_image) };
    }

    println!("[eftTrainer] mono setup complete, stamina readout armed");
    Some(MonoState {
        api,
        find_object_of_type,
        game_world_type_obj,
        table,
    })
}

#[unsafe(no_mangle)]
#[allow(non_snake_case, clippy::not_unsafe_ptr_arg_deref)]
extern "system" fn DllMain(hinst: HMODULE, reason: u32, _reserved: *mut core::ffi::c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        // HMODULE wraps a raw pointer, which isn't Send, so it can't cross
        // into the closure directly. The value itself is just an opaque
        // handle (no aliasing/lifetime concerns), so we pass it as a usize
        // and rebuild the HMODULE inside the thread.
        let hinst_raw = hinst.0 as usize;

        // Never do real work directly in DllMain — you're holding the loader
        // lock here, and calling into most Win32 APIs (or allocating) can
        // deadlock the process. Spawn a thread and do everything there.
        thread::spawn(move || unsafe {
            let hinst = HMODULE(hinst_raw as *mut core::ffi::c_void);
            let _ = AllocConsole();

            // Route panic messages through our own console rather than
            // relying on stderr having been re-pointed at CONOUT$ by
            // AllocConsole (inconsistent across CRTs) -- and this is a
            // process-wide hook, so it also covers anything in the retour
            // trampoline machinery or elsewhere in the process that panics
            // on a thread we don't otherwise wrap.
            std::panic::set_hook(Box::new(|info| {
                println!("[panic] {}", info);
            }));

            println!("[eftTrainer] injected OK, DllMain reached DLL_PROCESS_ATTACH");
            println!("[eftTrainer] press END to unload");

            // Setup is wrapped separately from the main loop: a panic here
            // just means "stamina readout unavailable this session" (same
            // as any other setup failure already handled below), not a
            // dead thread -- END-key unload must keep working regardless
            // of whether Mono setup succeeded.
            let mono_state = match catch_unwind(AssertUnwindSafe(|| setup_mono_state())) {
                Ok(state) => state,
                Err(_) => {
                    println!("[eftTrainer] setup_mono_state panicked (caught) -- stamina readout unavailable");
                    None
                }
            };
            if mono_state.is_none() {
                println!("[eftTrainer] stamina readout unavailable this session (see lines above)");
            }

            // Printed only on change, so sprinting-to-exhaustion in the
            // hideout (no HUD stamina bar there, but it drains the same as
            // in a raid) is visible in the console without spamming it
            // every tick.
            let mut last_printed: Option<f32> = None;
            let mut last_poll = Instant::now();

            loop {
                let key_down = (GetAsyncKeyState(VK_END.0 as i32) as u16 & 0x8000) != 0;
                if key_down {
                    println!("[eftTrainer] unloading...");
                    // brief pause so the print above actually flushes to the
                    // console before we tear down our own module
                    thread::sleep(Duration::from_millis(150));
                    let _ = FreeConsole();
                    // Frees this DLL and terminates this thread as one atomic
                    // step — calling FreeLibrary + ExitThread separately here
                    // would race, since this code itself lives in the module
                    // being freed.
                    FreeLibraryAndExitThread(hinst, 0);
                }

                if let Some(state) = &mono_state {
                    if last_poll.elapsed() >= Duration::from_millis(500) {
                        last_poll = Instant::now();
                        // Wrapped per-tick, not just at setup: a panic
                        // during a single poll (e.g. a future feature added
                        // here later) shouldn't kill the loop -- END-key
                        // unload has to keep working no matter what a poll
                        // does.
                        let reading = catch_unwind(AssertUnwindSafe(|| {
                            mono::read_current_stamina(
                                &state.api,
                                state.find_object_of_type,
                                state.game_world_type_obj,
                                &state.table,
                            )
                        }))
                        .unwrap_or_else(|_| {
                            println!("[eftTrainer] stamina poll panicked (caught), skipping this tick");
                            None
                        });
                        match reading {
                            Some(current) if Some(current) != last_printed => {
                                println!("[eftTrainer] current stamina: {}", current);
                                last_printed = Some(current);
                            }
                            None if last_printed.is_some() => {
                                // Left the raid/hideout (or GameWorld tore down).
                                println!("[eftTrainer] no active GameWorld/player right now");
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
