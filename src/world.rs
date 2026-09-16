//! Raid framework: enumerate live players, classify each as local / bot, read
//! world positions and project them to screen. The load-bearing foundation for
//! anything that reasons about entities in a raid (ESP, distance, aim, loot).
//!
//! Main-thread only -- every position read is a Unity invoke -- and driven from
//! menu::on_frame like chams. It publishes an immutable, handle-free snapshot
//! that other threads (the HTTP inspector, a future overlay) read without ever
//! touching Unity.
//!
//! By-name throughout: RegisteredPlayers, the IsYourPlayer / AIData backing
//! fields, Player.get_Position, Camera.main and WorldToScreenPoint all resolve
//! from metadata, so no native engine offset is baked in.

#![allow(dead_code)]

use crate::il2cpp;
use crate::runtime::{self, Class, Domain, Method, Object, ScriptRuntime};
use crate::symbols::{self, eft, unity};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// One entry in the raid snapshot. Plain data (Send): no managed handles, so
/// any thread can read it.
#[derive(Clone, Copy)]
pub struct Entry {
    pub ptr: usize,
    pub is_local: bool,
    pub is_bot: bool,
    pub pos: [f32; 3],    // world position (metres, world origin)
    pub screen: [f32; 3], // WorldToScreenPoint: x, y from bottom, z = depth
    pub on_screen: bool,  // depth > 0 (in front of the camera)
    pub dist: f32,        // metres from the local player
}

// Gate the work: nothing consumes the framework yet, so it stays idle (and
// spends no invokes) until a feature or the HTTP inspector turns it on.
static ENABLED: AtomicBool = AtomicBool::new(false);
static SNAPSHOT: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Immutable copy of the latest snapshot, for readers off the main thread.
pub fn snapshot() -> Vec<Entry> {
    SNAPSHOT.lock().map(|g| g.clone()).unwrap_or_default()
}

const TICK_INTERVAL: Duration = Duration::from_millis(250);
const MAX_PLAYERS: usize = 1024;
static LAST_TICK: Mutex<Option<Instant>> = Mutex::new(None);

struct Cache {
    gw_registered_off: i32,
    p_is_local_off: i32,
    p_aidata_off: Option<i32>,
    m_get_position: Method,
    m_cam_main: Method,
    m_w2s: Method,
}
static CACHE: OnceLock<Option<Box<Cache>>> = OnceLock::new();

unsafe fn resolve(rt: &dyn ScriptRuntime, domain: Domain) -> Option<Box<Cache>> {
    let gw_cls = unsafe { symbols::find_class(rt, domain, eft::GAME_WORLD) }?;
    let player_cls = unsafe { symbols::find_class(rt, domain, eft::PLAYER) }?;
    let cam_cls = unsafe { symbols::find_class(rt, domain, unity::CAMERA) }?;

    let off = |c: Class, name: &str| -> Option<i32> {
        unsafe { rt.field(c, name) }
            .map(|f| unsafe { rt.field_offset(f) })
            .filter(|&o| o > 0 && o < 0x10000)
    };

    // RegisteredPlayers (or AllAlivePlayersList) is required: no list, no frame.
    let mut gw_registered_off = 0;
    for name in eft::PLAYER_LIST_FIELDS {
        if let Some(o) = off(gw_cls, name) {
            gw_registered_off = o;
            crate::elog!("[world] player list via GameWorld.{} @ {:#x}", name, o);
            break;
        }
    }
    if gw_registered_off == 0 {
        crate::elog!("[world] no player list field -- framework disabled");
        return None;
    }
    let p_is_local_off = off(player_cls, eft::PLAYER_IS_LOCAL_FIELD)?;
    let p_aidata_off = off(player_cls, eft::PLAYER_AIDATA_FIELD);

    // Position + world-to-screen are best-effort: enumeration still works if a
    // getter is missing, positions just come back zeroed.
    let m_get_position =
        unsafe { rt.method(player_cls, eft::PLAYER_GET_POSITION, 0) }.unwrap_or(Method::NULL);
    let m_cam_main =
        unsafe { rt.method(cam_cls, unity::CAMERA_GET_MAIN, 0) }.unwrap_or(Method::NULL);
    let m_w2s =
        unsafe { rt.method_exact(cam_cls, unity::CAMERA_WORLD_TO_SCREEN, 1) }.unwrap_or(Method::NULL);

    crate::elog!(
        "[world] resolved: is_local @{:#x} aidata {} position {} w2s {}",
        p_is_local_off,
        p_aidata_off.map(|o| format!("@{:#x}", o)).unwrap_or_else(|| "MISSING".into()),
        if m_get_position.is_null() { "MISSING" } else { "ok" },
        if m_w2s.is_null() { "MISSING" } else { "ok" },
    );

    Some(Box::new(Cache {
        gw_registered_off,
        p_is_local_off,
        p_aidata_off,
        m_get_position,
        m_cam_main,
        m_w2s,
    }))
}

// Bytes of a Player we read directly (past the IsYourPlayer/AIData fields).
const PLAYER_READ_SPAN: usize = 0xc00;

#[inline]
unsafe fn readable_player(p: Object) -> bool {
    !p.is_null()
        && unsafe { il2cpp::readable_now(p.raw() as *const u8, PLAYER_READ_SPAN) }
        && unsafe { runtime::is_alive(p) }
}

/// Player.get_Position() -> Vector3, unboxed. Zeroed if unavailable.
unsafe fn get_position(rt: &dyn ScriptRuntime, c: &Cache, player: Object) -> [f32; 3] {
    if c.m_get_position.is_null() {
        return [0.0; 3];
    }
    match unsafe { rt.invoke(c.m_get_position, player, &mut []) } {
        Ok(boxed) => unsafe { runtime::unbox_vec3(boxed) }.unwrap_or([0.0; 3]),
        Err(_) => [0.0; 3],
    }
}

/// Camera.WorldToScreenPoint(pos). x, y (from the bottom), z = depth. The
/// Vector3 argument is passed by pointer, as runtime_invoke expects for a
/// value-type parameter.
unsafe fn w2s(rt: &dyn ScriptRuntime, c: &Cache, cam: Object, pos: [f32; 3]) -> [f32; 3] {
    if c.m_w2s.is_null() {
        return [0.0; 3];
    }
    let mut p = pos;
    let mut args = [&mut p as *mut [f32; 3] as *mut c_void];
    match unsafe { rt.invoke(c.m_w2s, cam, &mut args) } {
        Ok(boxed) => unsafe { runtime::unbox_vec3(boxed) }.unwrap_or([0.0; 3]),
        Err(_) => [0.0; 3],
    }
}

#[inline]
fn dist3(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Called every frame from the driver (main thread). Cheap when disabled or
/// between ticks. Never propagates a failure to the caller.
pub unsafe fn tick(rt: &dyn ScriptRuntime, domain: Domain) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let cache = CACHE.get_or_init(|| unsafe { resolve(rt, domain) });
    let Some(c) = cache.as_deref() else { return };

    {
        let mut g = match LAST_TICK.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(t) = *g {
            if t.elapsed() < TICK_INTERVAL {
                return;
            }
        }
        *g = Some(Instant::now());
    }

    let world = crate::hooks::game_world();
    if !unsafe { runtime::is_alive(world) } {
        if let Ok(mut g) = SNAPSHOT.lock() {
            g.clear();
        }
        return;
    }

    let list = Object(unsafe { runtime::read_field::<*mut c_void>(world, c.gw_registered_off) });
    let items = unsafe { runtime::list_items(list) };
    let n = unsafe { runtime::list_count(list) };
    if items.is_null() || n == 0 || n > MAX_PLAYERS {
        if let Ok(mut g) = SNAPSHOT.lock() {
            g.clear();
        }
        return;
    }
    let cap = n.min(unsafe { runtime::array_len(items) });

    let cam = if !c.m_cam_main.is_null() {
        unsafe { rt.invoke_static(c.m_cam_main, &mut []) }.filter(|o| unsafe { runtime::is_alive(*o) })
    } else {
        None
    };

    // Pass one: the local player's position, for the distance column.
    let mut local_pos: Option<[f32; 3]> = None;
    for i in 0..cap {
        let p = unsafe { runtime::array_get(items, i) };
        if !unsafe { readable_player(p) } {
            continue;
        }
        if unsafe { runtime::read_field::<u8>(p, c.p_is_local_off) } != 0 {
            local_pos = Some(unsafe { get_position(rt, c, p) });
            break;
        }
    }

    // Pass two: classify and project everyone.
    let mut out: Vec<Entry> = Vec::with_capacity(cap);
    for i in 0..cap {
        let p = unsafe { runtime::array_get(items, i) };
        if !unsafe { readable_player(p) } {
            continue;
        }
        let is_local = unsafe { runtime::read_field::<u8>(p, c.p_is_local_off) } != 0;
        let is_bot = !is_local
            && c.p_aidata_off
                .map(|o| unsafe { runtime::read_field::<usize>(p, o) } != 0)
                .unwrap_or(false);
        let pos = unsafe { get_position(rt, c, p) };
        let screen = match cam {
            Some(cam) => unsafe { w2s(rt, c, cam, pos) },
            None => [0.0; 3],
        };
        let dist = local_pos.map(|lp| dist3(lp, pos)).unwrap_or(0.0);
        out.push(Entry {
            ptr: p.raw() as usize,
            is_local,
            is_bot,
            pos,
            screen,
            on_screen: screen[2] > 0.0,
            dist,
        });
    }

    if let Ok(mut g) = SNAPSHOT.lock() {
        *g = out;
    }
}
