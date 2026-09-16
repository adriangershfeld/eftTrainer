//! Chams: draw players through walls in a high-visibility material.
//!
//! Backend-neutral. Every name is Unity's own (Renderer, Material, Shader,
//! Object, Component) or a real, readable EFT field, so this ports across game
//! updates with only symbols.rs to touch.
//!
//! Design: every chammed renderer is pointed (via sharedMaterials) at ONE
//! material object. Because they all share it, a style or colour change is a
//! single SetColor/SetInt on that one material -- it restyles and recolours
//! every player at once, in place, with no array reassignment. That is what
//! makes cycling reliable for any number of changes.
//!
//! Three styles, all blend-mode variants of the guaranteed Hidden/Internal-
//! Colored shader (so all cycle identically):
//!   * flat -- opaque unlit fill (clean, solid, always the safe default).
//!   * glow -- additive; overlapping shells stack and EFT's bloom lifts it
//!     into a neon glow.
//!   * mesh -- alpha-blended, both faces, no depth write; you see through the
//!     body to its far side, which reads as the model's mesh/form.
//! A rim/fresnel shader is used instead if the game ships one (best-effort).
//!
//! Cost control:
//!   * players come from GameWorld's own live-player list (O(players)), not
//!     FindObjectsOfType (O(all objects)); the latter is only a fallback.
//!   * each player's renderers are enumerated once, each renderer touched once,
//!     so steady-state per tick is a list read and nothing else.
//!   * throttled to a couple of times a second.
//!   * off restores the exact original shared materials it captured.
//!
//! Controlled from the in-game menu. Runs only on the main thread, driven from
//! menu::on_frame.

#![allow(dead_code)]

use crate::il2cpp;
use crate::runtime::{self, Class, Domain, Method, Object, ScriptRuntime};
use crate::symbols::{self, eft, unity};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ── public state ──────────────────────────────────────────────────────────────
pub static ENABLED: AtomicBool = AtomicBool::new(false);
// Colour palette index (SCHEMES).
static SCHEME: AtomicUsize = AtomicUsize::new(0);
// Material style: 0 flat (opaque fill), 1 glow (additive, blooms through EFT's
// post fx), 2 mesh (translucent, both faces -- reads as a see-through
// hologram). All three are blend-mode variants of one guaranteed shader, so
// all cycle colour identically.
static STYLE: AtomicUsize = AtomicUsize::new(0);
const STYLE_FLAT: usize = 0;
const STYLE_GLOW: usize = 1;
const STYLE_MESH: usize = 2;
const STYLE_NAMES: &[&str] = &["flat", "glow", "mesh"];

// Any style or scheme change sets this. The next tick re-applies the current
// style+scheme to the ONE shared material every renderer points at, mutating
// it in place (blend factors + colour). No array reassignment -- that is what
// makes cycling reliable across unlimited changes.
static APPLY: AtomicBool = AtomicBool::new(false);

pub fn toggle() -> bool {
    let on = !ENABLED.fetch_xor(true, Ordering::Relaxed);
    crate::elog!("[chams] {}", if on { "ON" } else { "OFF" });
    // Restore (on -> off) happens on the next tick, main thread only.
    on
}

/// Explicit on/off, for the menu and the HTTP control endpoint.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    crate::elog!("[chams] {}", if on { "ON" } else { "OFF" });
}

pub fn cycle_scheme() {
    let n = (SCHEME.load(Ordering::Relaxed) + 1) % SCHEMES.len();
    set_scheme(n);
}

/// Select a scheme by index (wraps).
pub fn set_scheme(n: usize) {
    let n = n % SCHEMES.len();
    SCHEME.store(n, Ordering::Relaxed);
    crate::elog!("[chams] scheme -> {}", SCHEMES[n].name);
    APPLY.store(true, Ordering::Relaxed);
}

pub fn cycle_style() {
    let n = (STYLE.load(Ordering::Relaxed) + 1) % STYLE_NAMES.len();
    set_style(n);
}

/// Select a material style by index (wraps).
pub fn set_style(n: usize) {
    let n = n % STYLE_NAMES.len();
    STYLE.store(n, Ordering::Relaxed);
    crate::elog!("[chams] style -> {}", STYLE_NAMES[n]);
    APPLY.store(true, Ordering::Relaxed);
}

pub fn style_name() -> &'static str {
    STYLE_NAMES[STYLE.load(Ordering::Relaxed) % STYLE_NAMES.len()]
}

pub fn scheme_name() -> &'static str {
    SCHEMES[SCHEME.load(Ordering::Relaxed) % SCHEMES.len()].name
}

pub fn is_on() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// (enabled, scheme_index, scheme_name, scheme_count, style_name) for /status.
pub fn status() -> (bool, usize, &'static str, usize, &'static str) {
    let i = SCHEME.load(Ordering::Relaxed);
    (
        ENABLED.load(Ordering::Relaxed),
        i,
        SCHEMES[i].name,
        SCHEMES.len(),
        style_name(),
    )
}

// ── colour schemes ────────────────────────────────────────────────────────────
// Two tones per scheme: `vis` for the part in line of sight, `occ` for the part
// behind geometry. Internal-Colored is unlit, so values render full-bright and
// read as emissive. Pushed hot on purpose. No pink, no white. `occ` is the
// hotter, more saturated of the pair since it is the read that matters most.
// repr(C): passed by pointer straight into a Unity Color (four sequential
// f32). Rust field order is not otherwise guaranteed.
#[repr(C)]
#[derive(Clone, Copy)]
struct Rgba {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}
const fn col(r: f32, g: f32, b: f32, a: f32) -> Rgba {
    Rgba { r, g, b, a }
}

struct Scheme {
    name: &'static str,
    vis: Rgba,
    occ: Rgba,
}

// Vivid, saturated, distinct hues at peak ~1.0. Pushing channels far above 1.0
// made EFT's tonemapper/bloom blow every scheme out to the same near-white, so
// cycling changed the material but not the visible colour. `occ` (used by solid
// mode) is the saturated hue; `vis` is a lighter tint for the two-tone pass.
const SCHEMES: &[Scheme] = &[
    // visible (lighter tint)                    occluded / solid (saturated hue)
    Scheme { name: "plasma", vis: col(0.55, 1.00, 1.00, 1.0), occ: col(0.00, 0.95, 1.00, 1.0) },
    Scheme { name: "molten", vis: col(1.00, 0.60, 0.25, 1.0), occ: col(1.00, 0.35, 0.00, 1.0) },
    Scheme { name: "toxic",  vis: col(0.65, 1.00, 0.35, 1.0), occ: col(0.25, 1.00, 0.00, 1.0) },
    Scheme { name: "void",   vis: col(0.80, 0.45, 1.00, 1.0), occ: col(0.65, 0.00, 1.00, 1.0) },
    Scheme { name: "ice",    vis: col(0.60, 0.85, 1.00, 1.0), occ: col(0.00, 0.55, 1.00, 1.0) },
    Scheme { name: "ember",  vis: col(1.00, 0.45, 0.40, 1.0), occ: col(1.00, 0.05, 0.00, 1.0) },
];

/// Rim power applied when a rim-capable shader is in use.
const RIM_POWER: f32 = 2.2;

/// Flat cham shaders, best first. Internal-Colored is guaranteed present in
/// every Unity build and is unlit, so the colour reads as emissive.
const FLAT_SHADERS: &[&str] = &[
    "Hidden/Internal-Colored",
    "Sprites/Default",
    "UI/Default",
    "Unlit/Color",
];

/// One-time dump of every loaded shader name, so a real rim/fresnel shader in
/// the game can be added to RIM_SHADER_CANDIDATES next iteration.
const LOG_LOADED_SHADERS: bool = true;

const TICK_INTERVAL: Duration = Duration::from_millis(500);
const MAX_SUBMESHES: usize = 64;
const MAX_RENDERERS_PER_PLAYER: usize = 4096;
const MAX_PLAYERS: usize = 4096;

// ── resolved handles (once) ───────────────────────────────────────────────────
// Object is deliberately !Send (managed handles are thread-bound), so Type
// objects are held as raw usize and rebuilt at the call site. Access is
// main-thread-only via the driver, so this is sound. Methods are Send + Sync.
struct Cache {
    ty_player: usize,
    ty_renderer: usize,
    ty_skinned: usize, // SkinnedMeshRenderer type for the walk filter
    ty_shader: usize,  // 0 if unresolved (shader logging skipped)
    mat_class: Class,

    find_objects: Method,
    find_objects_argc: u32,
    find_objects_all: Method, // NULL if unavailable
    instantiate: Method,
    shader_find: Method,
    get_name: Method, // NULL if unavailable

    // Transform-tree walk (no non-generic GetComponentsInChildren in this build)
    comp_get_transform: Method,
    comp_get_component: Method, // GetComponent(Type)
    tf_child_count: Method,
    tf_get_child: Method,

    r_get_materials: Method,
    r_get_shared_materials: Method,
    r_set_materials: Method,
    r_set_shared_materials: Method,

    m_set_shader: Method,
    m_set_color: Method,       // SetColor(string, Color)
    m_set_int: Method,         // SetInt(string, int)
    m_set_float: Method,       // SetFloat(string, float)
    m_set_render_queue: Method,

    /// Whether array_new passed its self-test; enables two-tone.
    dual_ok: bool,
    /// Offset of a GameWorld live-player List<> field, if one resolved.
    gw_players_off: Option<i32>,
    /// Offset of Player._renderers (Renderer[]), the game's own renderer array.
    /// Preferred over the transform-tree walk. None -> walk only.
    player_renderers_off: Option<i32>,
    /// Offset of GameWorld.MainPlayer, used to identify the local player so the
    /// transform walk (needed only for first-person arms/weapon) is skipped for
    /// remote players, whose body renderers all come from Player._renderers.
    /// None -> walk every player, as before.
    main_player_off: Option<i32>,
}

static CACHE: OnceLock<Option<Box<Cache>>> = OnceLock::new();

// The ONE cham material every chammed renderer shares (0 = not built). Every
// renderer's sharedMaterials array points at this same object, so a single
// SetColor / SetInt on it restyles and recolours all of them at once -- that
// is the whole trick behind reliable cycling. Rooted by the renderers plus
// this static (conservatively scanned).
static MATERIAL: Mutex<usize> = Mutex::new(0);

// Chosen base shader, decided once when the first material is built.
#[derive(Clone, Copy)]
struct ShaderChoice {
    shader: usize,
    rim: bool,
}
static SHADER_CHOICE: Mutex<Option<ShaderChoice>> = Mutex::new(None);

// Renderers we have chammed -> their original sharedMaterials array pointer,
// so F5-off restores exactly. Doubles as the "already touched" set. The array
// pointers stored here keep the originals rooted (conservative GC scan).
static ORIGINALS: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);
// Detects a scene/raid change so stale caches are dropped.
static LAST_WORLD: AtomicUsize = AtomicUsize::new(0);
static LAST_TICK: Mutex<Option<Instant>> = Mutex::new(None);
// Cached transform-walk result per player, so the tree is not re-traversed
// every tick. Re-walked at most every RE_WALK_INTERVAL to catch new gear.
static PLAYER_WALK: Mutex<Option<HashMap<usize, (Vec<usize>, Instant)>>> = Mutex::new(None);
const RE_WALK_INTERVAL: Duration = Duration::from_secs(3);

/// A safe liveness check: confirm the object memory is mapped before reading
/// its m_CachedPtr, so a stale/freed pointer can never fault. Used on stored
/// pointers (restore/reassign) where the renderer may have been destroyed.
#[inline]
unsafe fn alive(o: Object) -> bool {
    !o.is_null()
        && unsafe { il2cpp::readable_now(o.raw() as *const u8, 0x18) }
        && unsafe { runtime::is_alive(o) }
}

// ── small lock helpers (poison-proof) ─────────────────────────────────────────
fn with_originals<R>(f: impl FnOnce(&mut HashMap<usize, usize>) -> R) -> Option<R> {
    let mut g = ORIGINALS.lock().ok()?;
    Some(f(g.get_or_insert_with(HashMap::new)))
}

/// Transform walk for a player, done once and frozen. Re-walking every tick is
/// what floods the set with transient child renderers (effects, casings), so
/// the first result is cached and reused until the scene changes or chams is
/// toggled off (which clears the cache, letting a re-enable re-capture).
unsafe fn cached_walk(rt: &dyn ScriptRuntime, c: &Cache, player: usize, pobj: Object) -> Vec<usize> {
    if let Ok(g) = PLAYER_WALK.lock() {
        if let Some((list, _)) = g.as_ref().and_then(|m| m.get(&player)) {
            return list.clone();
        }
    }
    let list = unsafe { collect_renderers(rt, c, pobj) };
    if let Ok(mut g) = PLAYER_WALK.lock() {
        g.get_or_insert_with(HashMap::new)
            .insert(player, (list.clone(), Instant::now()));
    }
    list
}

// ── resolution ────────────────────────────────────────────────────────────────

unsafe fn resolve(rt: &dyn ScriptRuntime, domain: Domain) -> Option<Box<Cache>> {
    let obj_cls: Class = unsafe { symbols::find_class(rt, domain, unity::OBJECT) }?;
    let comp_cls: Class = unsafe { symbols::find_class(rt, domain, unity::COMPONENT) }?;
    let rend_cls: Class = unsafe { symbols::find_class(rt, domain, unity::RENDERER) }?;
    let mat_cls: Class = unsafe { symbols::find_class(rt, domain, unity::MATERIAL) }?;
    let shader_cls: Class = unsafe { symbols::find_class(rt, domain, unity::SHADER) }?;

    let ty_player = unsafe { symbols::find_type_object(rt, domain, eft::PLAYER) }?;
    let ty_renderer = unsafe { symbols::find_type_object(rt, domain, unity::RENDERER) }?;
    // SkinnedMeshRenderer type for the walk filter; fall back to Renderer.
    let ty_skinned = unsafe { symbols::find_type_object(rt, domain, unity::SKINNED_MESH_RENDERER) }
        .map(|o| o.raw() as usize)
        .unwrap_or(ty_renderer.raw() as usize);
    let ty_shader = unsafe { symbols::find_type_object(rt, domain, unity::SHADER) }
        .map(|o| o.raw() as usize)
        .unwrap_or(0);

    // FindObjectsOfType: 1-arg on 2022.3, 2-arg (Type,bool) on some builds.
    let (find_objects, find_objects_argc) =
        match unsafe { rt.method_exact(obj_cls, unity::FIND_OBJECTS_OF_TYPE, 1) } {
            Some(m) => (m, 1u32),
            None => (
                unsafe { rt.method_exact(obj_cls, unity::FIND_OBJECTS_OF_TYPE, 2) }?,
                2u32,
            ),
        };
    let find_objects_all = unsafe { rt.method_exact(obj_cls, unity::FIND_OBJECTS_OF_TYPE_ALL, 1) }
        .unwrap_or(Method::NULL);
    let get_name = unsafe { rt.method(obj_cls, unity::GET_NAME, 0) }.unwrap_or(Method::NULL);

    let tf_cls: Class = unsafe { symbols::find_class(rt, domain, unity::TRANSFORM) }?;

    // Every required handle logs its own miss, so a resolution failure names
    // the culprit instead of a silent "resolution failed".
    macro_rules! need {
        ($e:expr, $what:expr) => {
            match unsafe { $e } {
                Some(m) => m,
                None => {
                    crate::elog!("[chams] MISSING method: {}", $what);
                    return None;
                }
            }
        };
    }

    let instantiate = need!(rt.method_exact(obj_cls, unity::INSTANTIATE, 1), "Object.Instantiate/1");
    let shader_find = need!(rt.method_exact(shader_cls, unity::SHADER_FIND, 1), "Shader.Find/1");

    let comp_get_transform = need!(rt.method(comp_cls, unity::GET_TRANSFORM, 0), "Component.get_transform");
    let comp_get_component = need!(rt.method_exact(comp_cls, unity::GET_COMPONENT, 1), "Component.GetComponent(Type)");
    let tf_child_count = need!(rt.method(tf_cls, unity::GET_CHILD_COUNT, 0), "Transform.get_childCount");
    let tf_get_child = need!(rt.method_exact(tf_cls, unity::GET_CHILD, 1), "Transform.GetChild/1");

    let r_get_materials = need!(rt.method(rend_cls, unity::GET_MATERIALS, 0), "Renderer.get_materials");
    let r_get_shared_materials = need!(rt.method(rend_cls, unity::GET_SHARED_MATERIALS, 0), "Renderer.get_sharedMaterials");
    let r_set_materials = need!(rt.method(rend_cls, unity::SET_MATERIALS, 1), "Renderer.set_materials");
    let r_set_shared_materials = need!(rt.method(rend_cls, unity::SET_SHARED_MATERIALS, 1), "Renderer.set_sharedMaterials");

    // SetColor/SetInt/SetFloat resolve to their (string, value) overloads,
    // verified on 46911 to precede the (int nameID, value) forms in metadata.
    let m_set_shader = need!(rt.method(mat_cls, unity::MAT_SET_SHADER, 1), "Material.set_shader");
    let m_set_color = need!(rt.method_exact(mat_cls, unity::MAT_SET_COLOR, 2), "Material.SetColor/2");
    let m_set_int = need!(rt.method_exact(mat_cls, unity::MAT_SET_INT, 2), "Material.SetInt/2");
    let m_set_float = need!(rt.method_exact(mat_cls, unity::MAT_SET_FLOAT, 2), "Material.SetFloat/2");
    let m_set_render_queue = need!(rt.method(mat_cls, unity::MAT_SET_RENDER_QUEUE, 1), "Material.set_renderQueue");

    // Assigning the shared material as sharedMaterials needs a managed
    // Material[] we allocate ourselves. Self-test array_new once: allocate a
    // Material[2] and confirm length and null elements. array_new is not in
    // the verified-genuine export set, so it is trusted only after this. If it
    // fails, assignment falls back to the instance path (recolour still works
    // per-cham, just not the shared in-place mutation).
    let dual_ok = unsafe { self_test_array_new(rt, mat_cls) };
    crate::elog!(
        "[chams] shared-material assign {}",
        if dual_ok { "enabled (array_new ok)" } else { "OFF (array_new unavailable) -- instance fallback" }
    );

    // GameWorld live-player list, for the O(players) fast path.
    let gw_players_off = unsafe { resolve_players_field(rt, domain) };
    match gw_players_off {
        Some(off) => crate::elog!("[chams] player list field @ {:#x}", off),
        None => crate::elog!("[chams] no player list field -- using FindObjectsOfType(Player)"),
    }

    // Player._renderers, the game's own renderer array (stable enumeration).
    let player_renderers_off = unsafe { symbols::find_class(rt, domain, eft::PLAYER) }
        .and_then(|pc| unsafe { rt.field(pc, eft::PLAYER_RENDERERS_FIELD) })
        .map(|f| unsafe { rt.field_offset(f) })
        .filter(|&o| o > 0 && o < 0x10000);
    match player_renderers_off {
        Some(off) => crate::elog!("[chams] Player._renderers @ {:#x}", off),
        None => crate::elog!("[chams] Player._renderers not found -- using transform walk"),
    }

    // GameWorld.MainPlayer: identifies the local player so the (invoke-heavy)
    // transform walk runs only for it, not for every remote player.
    let main_player_off = unsafe { symbols::find_class(rt, domain, eft::GAME_WORLD) }
        .and_then(|gc| unsafe { rt.field(gc, "MainPlayer") })
        .map(|f| unsafe { rt.field_offset(f) })
        .filter(|&o| o > 0 && o < 0x10000);
    match main_player_off {
        Some(off) => crate::elog!("[chams] GameWorld.MainPlayer @ {:#x} (walk local only)", off),
        None => crate::elog!("[chams] GameWorld.MainPlayer not found -- walking all players"),
    }

    crate::elog!("[chams] all handles resolved");
    Some(Box::new(Cache {
        ty_player: ty_player.raw() as usize,
        ty_renderer: ty_renderer.raw() as usize,
        ty_skinned,
        ty_shader,
        mat_class: mat_cls,
        find_objects,
        find_objects_argc,
        find_objects_all,
        instantiate,
        shader_find,
        get_name,
        comp_get_transform,
        comp_get_component,
        tf_child_count,
        tf_get_child,
        r_get_materials,
        r_get_shared_materials,
        r_set_materials,
        r_set_shared_materials,
        m_set_shader,
        m_set_color,
        m_set_int,
        m_set_float,
        m_set_render_queue,
        dual_ok,
        gw_players_off,
        player_renderers_off,
        main_player_off,
    }))
}

/// Confirm array_new returns a real, correctly-sized, zero-filled array before
/// we trust it for two-tone. A decoyed export is caught here.
unsafe fn self_test_array_new(rt: &dyn ScriptRuntime, mat_cls: Class) -> bool {
    let Some(arr) = (unsafe { rt.new_array(mat_cls, 2) }) else { return false };
    if unsafe { runtime::array_len(arr) } != 2 {
        return false;
    }
    // A freshly-allocated managed array is zeroed.
    for i in 0..2 {
        if !unsafe { runtime::array_get(arr, i) }.is_null() {
            crate::elog!("[chams] array_new self-test: non-null element -- rejecting");
            return false;
        }
    }
    true
}

/// First GameWorld player-list field that resolves to an offset.
unsafe fn resolve_players_field(rt: &dyn ScriptRuntime, domain: Domain) -> Option<i32> {
    let gw_cls = unsafe { symbols::find_class(rt, domain, eft::GAME_WORLD) }?;
    for name in eft::PLAYER_LIST_FIELDS {
        if let Some(f) = unsafe { rt.field(gw_cls, name) } {
            let off = unsafe { rt.field_offset(f) };
            if off > 0 && off < 0x10000 {
                crate::elog!("[chams] player list via GameWorld.{}", name);
                return Some(off);
            }
        }
    }
    None
}

// ── material construction ─────────────────────────────────────────────────────

unsafe fn set_shader_of(rt: &dyn ScriptRuntime, c: &Cache, mat: Object, shader: Object) {
    let mut a = [shader.raw()];
    let _ = unsafe { rt.invoke(c.m_set_shader, mat, &mut a) };
}

unsafe fn set_str_color(
    rt: &dyn ScriptRuntime,
    c: &Cache,
    domain: Domain,
    mat: Object,
    name: &str,
    v: Rgba,
) {
    let Some(n) = (unsafe { rt.new_string(domain, name) }) else { return };
    let mut val = v;
    let mut args = [n.raw(), &mut val as *mut Rgba as *mut c_void];
    let _ = unsafe { rt.invoke(c.m_set_color, mat, &mut args) };
}

unsafe fn set_str_int(
    rt: &dyn ScriptRuntime,
    c: &Cache,
    domain: Domain,
    mat: Object,
    name: &str,
    v: i32,
) {
    let Some(n) = (unsafe { rt.new_string(domain, name) }) else { return };
    let mut val = v;
    let mut args = [n.raw(), &mut val as *mut i32 as *mut c_void];
    let _ = unsafe { rt.invoke(c.m_set_int, mat, &mut args) };
}

unsafe fn set_str_float(
    rt: &dyn ScriptRuntime,
    c: &Cache,
    domain: Domain,
    mat: Object,
    name: &str,
    v: f32,
) {
    let Some(n) = (unsafe { rt.new_string(domain, name) }) else { return };
    let mut val = v;
    let mut args = [n.raw(), &mut val as *mut f32 as *mut c_void];
    let _ = unsafe { rt.invoke(c.m_set_float, mat, &mut args) };
}

unsafe fn set_render_queue(rt: &dyn ScriptRuntime, c: &Cache, mat: Object, q: i32) {
    let mut v = q;
    let mut args = [&mut v as *mut i32 as *mut c_void];
    let _ = unsafe { rt.invoke(c.m_set_render_queue, mat, &mut args) };
}

/// Pick the base shader once. A rim-capable shader wins if the game ships one;
/// otherwise a reliable flat shader. None means "keep the template's shader".
unsafe fn choose_shader(rt: &dyn ScriptRuntime, c: &Cache, domain: Domain) -> Option<ShaderChoice> {
    if let Ok(g) = SHADER_CHOICE.lock() {
        if let Some(ch) = *g {
            return Some(ch);
        }
    }
    let find = |name: &str| -> Option<Object> {
        let s = unsafe { rt.new_string(domain, name) }?;
        let mut args = [s.raw()];
        let r = unsafe { rt.invoke_static(c.shader_find, &mut args) }?;
        (!r.is_null()).then_some(r)
    };

    let mut chosen: Option<ShaderChoice> = None;
    for name in unity::RIM_SHADER_CANDIDATES {
        if let Some(sh) = find(name) {
            crate::elog!("[chams] rim shader: {}", name);
            chosen = Some(ShaderChoice { shader: sh.raw() as usize, rim: true });
            break;
        }
    }
    if chosen.is_none() {
        for name in FLAT_SHADERS {
            if let Some(sh) = find(name) {
                crate::elog!("[chams] flat shader: {}", name);
                chosen = Some(ShaderChoice { shader: sh.raw() as usize, rim: false });
                break;
            }
        }
    }
    if chosen.is_none() {
        crate::elog!("[chams] no cham shader found -- keeping template shader");
    }
    if let (Ok(mut g), Some(ch)) = (SHADER_CHOICE.lock(), chosen) {
        *g = Some(ch);
    }
    chosen
}

/// The per-style material configuration, factored out so it is identical at
/// build time and on every restyle/recolour. Internal-Colored honours all of
/// these, so switching style never needs a new shader -- just new blend
/// factors and colour on the one shared material.
unsafe fn configure_material(
    rt: &dyn ScriptRuntime,
    c: &Cache,
    domain: Domain,
    mat: Object,
    style: usize,
    s: &Scheme,
) {
    use unity::*;
    unsafe {
        // Common to every style: draw through walls, both faces, on top.
        set_str_int(rt, c, domain, mat, PROP_ZTEST, ZTEST_ALWAYS);
        set_str_int(rt, c, domain, mat, PROP_CULL, CULL_OFF);
        set_render_queue(rt, c, mat, 4000);

        match style {
            STYLE_GLOW => {
                // Additive: colour adds onto the framebuffer, overlapping shells
                // stack, and EFT's bloom lifts it into a neon glow.
                set_str_int(rt, c, domain, mat, PROP_SRC_BLEND, BLEND_ONE);
                set_str_int(rt, c, domain, mat, PROP_DST_BLEND, BLEND_ONE);
                set_str_int(rt, c, domain, mat, PROP_ZWRITE, 0);
                set_str_color(rt, c, domain, mat, PROP_COLOR, s.occ);
            }
            STYLE_MESH => {
                // Standard alpha, no depth write, both faces: you see through the
                // body to its far side, which reads as the model's mesh/form.
                set_str_int(rt, c, domain, mat, PROP_SRC_BLEND, BLEND_SRC_ALPHA);
                set_str_int(rt, c, domain, mat, PROP_DST_BLEND, BLEND_ONE_MINUS_SRC_ALPHA);
                set_str_int(rt, c, domain, mat, PROP_ZWRITE, 0);
                let mut col = s.vis;
                col.a = 0.45;
                set_str_color(rt, c, domain, mat, PROP_COLOR, col);
            }
            _ => {
                // Flat: opaque fill that writes depth, so it reads as solid.
                set_str_int(rt, c, domain, mat, PROP_SRC_BLEND, BLEND_ONE);
                set_str_int(rt, c, domain, mat, PROP_DST_BLEND, BLEND_ZERO);
                set_str_int(rt, c, domain, mat, PROP_ZWRITE, 1);
                set_str_color(rt, c, domain, mat, PROP_COLOR, s.occ);
            }
        }

        // Rim props, best-effort; only meaningful if a rim shader was chosen.
        let rim = SHADER_CHOICE.lock().ok().and_then(|g| *g).map(|ch| ch.rim).unwrap_or(false);
        if rim {
            let col = if style == STYLE_MESH { s.vis } else { s.occ };
            for name in RIM_COLOR_PROPS {
                set_str_color(rt, c, domain, mat, name, col);
            }
            for name in RIM_POWER_PROPS {
                set_str_float(rt, c, domain, mat, name, RIM_POWER);
            }
        }
    }
}

/// Clone `template` into a fresh cham material and configure it for the current
/// style + scheme. The clone is what every renderer will share.
unsafe fn build_fill(
    rt: &dyn ScriptRuntime,
    c: &Cache,
    domain: Domain,
    template: Object,
) -> Option<Object> {
    let mut a = [template.raw()];
    let clone = unsafe { rt.invoke_static(c.instantiate, &mut a) }?;
    if clone.is_null() {
        return None;
    }
    if let Some(ch) = unsafe { choose_shader(rt, c, domain) } {
        unsafe { set_shader_of(rt, c, clone, Object(ch.shader as *mut c_void)) };
    }
    let s = &SCHEMES[SCHEME.load(Ordering::Relaxed)];
    unsafe { configure_material(rt, c, domain, clone, STYLE.load(Ordering::Relaxed), s) };
    Some(clone)
}

/// Build (once, cached) the single shared cham material. Returns its pointer.
unsafe fn ensure_material(
    rt: &dyn ScriptRuntime,
    c: &Cache,
    domain: Domain,
    template: Object,
) -> Option<usize> {
    // Reuse the cached material only if it is still alive. It is rooted only by
    // the renderers using it (our Rust static is not a GC root), so if they were
    // all destroyed it may have been collected -- rebuild in that case.
    let cur = MATERIAL.lock().map(|g| *g).unwrap_or(0);
    if cur != 0 {
        if unsafe { alive(Object(cur as *mut c_void)) } {
            return Some(cur);
        }
        if let Ok(mut g) = MATERIAL.lock() {
            *g = 0;
        }
    }
    let fill = unsafe { build_fill(rt, c, domain, template) }?;
    let p = fill.raw() as usize;
    if let Ok(mut g) = MATERIAL.lock() {
        *g = p;
    }
    crate::elog!(
        "[chams] material built ({} / {}) fill={:#x}",
        style_name(), scheme_name(), p
    );
    Some(p)
}

// ── apply / restore ───────────────────────────────────────────────────────────

/// Point one renderer at the shared cham material for all of its submeshes, via
/// sharedMaterials (NO per-renderer instancing) so that a later SetColor on the
/// shared material shows up here too. `n` is the submesh count. Does not touch
/// ORIGINALS.
unsafe fn assign_fill(
    rt: &dyn ScriptRuntime,
    c: &Cache,
    renderer: Object,
    fill: usize,
    n: usize,
) -> bool {
    if fill == 0 || n == 0 || n > MAX_SUBMESHES {
        return false;
    }
    let fill_obj = Object(fill as *mut c_void);

    // Preferred: a fresh managed Material[n] filled with the shared material and
    // assigned as sharedMaterials. Every renderer then references the ONE
    // material object, so recolour/restyle is a single SetColor away.
    if c.dual_ok {
        if let Some(arr) = unsafe { rt.new_array(c.mat_class, n) } {
            for i in 0..n {
                unsafe { runtime::array_set(arr, i, fill_obj) };
            }
            let mut args = [arr.raw()];
            let _ = unsafe { rt.invoke(c.r_set_shared_materials, renderer, &mut args) };
            return true;
        }
    }

    // Fallback (array_new unavailable): overwrite the renderer's own instance
    // array in place. These become per-renderer instances, so in-place recolour
    // will not reach them -- only taken if array_new is missing.
    let Ok(inst) = (unsafe { rt.invoke(c.r_get_materials, renderer, &mut []) }) else {
        return false;
    };
    if inst.is_null() {
        return false;
    }
    let ni = unsafe { runtime::array_len(inst) };
    if ni == 0 || ni > MAX_SUBMESHES {
        return false;
    }
    for i in 0..ni {
        unsafe { runtime::array_set(inst, i, fill_obj) };
    }
    let mut args = [inst.raw()];
    let _ = unsafe { rt.invoke(c.r_set_materials, renderer, &mut args) };
    true
}

/// Cham a renderer we have not touched yet: capture its true original, then
/// assign the shared material. Returns true if newly chammed.
unsafe fn cham_renderer(
    rt: &dyn ScriptRuntime,
    c: &Cache,
    domain: Domain,
    renderer: Object,
    fill: &mut Option<usize>,
) -> bool {
    if !unsafe { runtime::is_alive(renderer) } {
        return false;
    }
    let key = renderer.raw() as usize;
    if with_originals(|m| m.contains_key(&key)).unwrap_or(true) {
        return false;
    }

    // sharedMaterials: the true originals, no per-instance clone.
    let Ok(shared) = (unsafe { rt.invoke(c.r_get_shared_materials, renderer, &mut []) }) else {
        return false;
    };
    if shared.is_null() {
        return false;
    }
    let n = unsafe { runtime::array_len(shared) };
    if n == 0 || n > MAX_SUBMESHES {
        return false;
    }
    let mut template = Object::NULL;
    for i in 0..n {
        let mm = unsafe { runtime::array_get(shared, i) };
        if !mm.is_null() {
            template = mm;
            break;
        }
    }
    if template.is_null() {
        return false;
    }

    if fill.is_none() {
        *fill = unsafe { ensure_material(rt, c, domain, template) };
    }
    let Some(fp) = *fill else { return false };

    // Capture the true original before overwriting (also roots it).
    with_originals(|map| map.insert(key, shared.raw() as usize));
    unsafe { assign_fill(rt, c, renderer, fp, n) }
}

/// Re-apply the current style + scheme to the ONE shared material, in place.
/// Because every chammed renderer points at this same material, this single
/// mutation restyles and recolours all of them at once -- no reassign, no
/// re-walk, nothing to revert. That is why cycling is reliable for any number
/// of changes. A no-op until the material has been built by the first cham.
unsafe fn apply_style(rt: &dyn ScriptRuntime, c: &Cache, domain: Domain) {
    let fill = MATERIAL.lock().map(|g| *g).unwrap_or(0);
    if fill == 0 {
        return; // not built yet; the next cham builds with the current style
    }
    // Defensive: if the material was destroyed out from under us (a scene event
    // that did not move the world pointer, say), do not touch freed memory --
    // drop the handle and let the next cham rebuild.
    let mat = Object(fill as *mut c_void);
    if !unsafe { alive(mat) } {
        if let Ok(mut g) = MATERIAL.lock() {
            *g = 0;
        }
        return;
    }
    let s = &SCHEMES[SCHEME.load(Ordering::Relaxed)];
    unsafe {
        configure_material(
            rt, c, domain,
            mat,
            STYLE.load(Ordering::Relaxed),
            s,
        )
    };
    crate::elog!("[chams] restyled -> {} / {}", style_name(), scheme_name());
}

/// Restore every chammed renderer, for the unload path. Main thread only.
/// Without this, materials persist on the player and survive a re-inject.
pub unsafe fn on_unload() {
    let Some(rt) = runtime::get() else { return };
    if let Some(Some(c)) = CACHE.get() {
        unsafe { restore_all(rt, c) };
    }
    ENABLED.store(false, Ordering::Relaxed);
}

/// Put every captured original back, and forget them. Main thread only.
unsafe fn restore_all(rt: &dyn ScriptRuntime, c: &Cache) {
    let map = {
        let mut g = match ORIGINALS.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        g.take()
    };
    let Some(map) = map else { return };
    let mut n = 0u32;
    for (rk, ak) in map {
        let rend = Object(rk as *mut c_void);
        if ak == 0 || !unsafe { alive(rend) } {
            continue;
        }
        let mut args = [ak as *mut c_void];
        let _ = unsafe { rt.invoke(c.r_set_shared_materials, rend, &mut args) };
        n += 1;
    }
    if n > 0 {
        crate::elog!("[chams] restored {} renderer(s)", n);
    }
}

// ── player enumeration ────────────────────────────────────────────────────────

/// Live players, by pointer. Prefers GameWorld's own list; falls back to
/// FindObjectsOfType(Player). Empty when no world is loaded.
unsafe fn enumerate_players(rt: &dyn ScriptRuntime, c: &Cache, world: Object) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();

    if let Some(off) = c.gw_players_off {
        if unsafe { runtime::is_alive(world) } {
            let list = Object(unsafe { runtime::read_field::<*mut c_void>(world, off) });
            let items = unsafe { runtime::list_items(list) };
            let n = unsafe { runtime::list_count(list) };
            if !items.is_null() && n > 0 && n <= MAX_PLAYERS {
                let cap = n.min(unsafe { runtime::array_len(items) });
                for i in 0..cap {
                    let p = unsafe { runtime::array_get(items, i) };
                    if !p.is_null() {
                        out.push(p.raw() as usize);
                    }
                }
                if !out.is_empty() {
                    return out;
                }
            }
        }
    }

    // Fallback: FindObjectsOfType(Player).
    let players = if c.find_objects_argc == 1 {
        let mut args = [c.ty_player as *mut c_void];
        unsafe { rt.invoke_static(c.find_objects, &mut args) }
    } else {
        let mut inc = 1i32;
        let mut args = [c.ty_player as *mut c_void, &mut inc as *mut i32 as *mut c_void];
        unsafe { rt.invoke_static(c.find_objects, &mut args) }
    };
    let Some(players) = players else { return out };
    if players.is_null() {
        return out;
    }
    let n = unsafe { runtime::array_len(players) };
    if n == 0 || n > MAX_PLAYERS {
        return out;
    }
    for i in 0..n {
        let p = unsafe { runtime::array_get(players, i) };
        if !p.is_null() {
            out.push(p.raw() as usize);
        }
    }
    out
}

/// The player's own Renderer[] (Player._renderers). Stable and cheap: a direct
/// managed-array read, no invokes. Empty if the field is absent or null.
unsafe fn player_renderers(c: &Cache, player: Object) -> Vec<usize> {
    let mut out = Vec::new();
    let Some(off) = c.player_renderers_off else { return out };
    let arr = Object(unsafe { runtime::read_field::<*mut c_void>(player, off) });
    if arr.is_null() {
        return out;
    }
    let n = unsafe { runtime::array_len(arr) };
    if n == 0 || n > MAX_RENDERERS_PER_PLAYER {
        return out;
    }
    for i in 0..n {
        let r = unsafe { runtime::array_get(arr, i) };
        if !r.is_null() {
            out.push(r.raw() as usize);
        }
    }
    out
}

/// All Renderers under a player, by walking the transform tree. Fallback when
/// Player._renderers is unavailable. Bounded.
unsafe fn collect_renderers(rt: &dyn ScriptRuntime, c: &Cache, player: Object) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let Ok(root) = (unsafe { rt.invoke(c.comp_get_transform, player, &mut []) }) else {
        return out;
    };
    if root.is_null() {
        return out;
    }
    let mut stack: Vec<usize> = vec![root.raw() as usize];
    let mut visited = 0usize;
    while let Some(tp) = stack.pop() {
        if visited >= MAX_RENDERERS_PER_PLAYER {
            break;
        }
        visited += 1;
        let t = Object(tp as *mut c_void);
        if !unsafe { runtime::is_alive(t) } {
            continue;
        }
        // Any Renderer on this node (arms are skinned, weapon parts are mesh,
        // both need to show). The flood is avoided by walking once and freezing,
        // not by filtering the type.
        let mut a = [c.ty_renderer as *mut c_void];
        if let Ok(r) = unsafe { rt.invoke(c.comp_get_component, t, &mut a) } {
            if !r.is_null() {
                out.push(r.raw() as usize);
            }
        }
        // Queue children.
        let Ok(cc) = (unsafe { rt.invoke(c.tf_child_count, t, &mut []) }) else {
            continue;
        };
        let n = unsafe { runtime::unbox_i32(cc) }.unwrap_or(0);
        if n <= 0 || n > 1024 {
            continue;
        }
        for i in 0..n {
            let mut idx = i;
            let mut ca = [&mut idx as *mut i32 as *mut c_void];
            if let Ok(child) = unsafe { rt.invoke(c.tf_get_child, t, &mut ca) } {
                if !child.is_null() {
                    stack.push(child.raw() as usize);
                }
            }
        }
    }
    out
}

/// Log every loaded shader name once, as a tuning aid for picking a real rim
/// shader. Best-effort; skipped if the needed handles are missing.
unsafe fn log_loaded_shaders(rt: &dyn ScriptRuntime, c: &Cache) {
    static DONE: AtomicBool = AtomicBool::new(false);
    if !LOG_LOADED_SHADERS || DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    if c.find_objects_all.is_null() || c.get_name.is_null() || c.ty_shader == 0 {
        return;
    }
    let mut args = [c.ty_shader as *mut c_void];
    let Some(arr) = (unsafe { rt.invoke_static(c.find_objects_all, &mut args) }) else { return };
    if arr.is_null() {
        return;
    }
    let n = unsafe { runtime::array_len(arr) };
    if n == 0 || n > 8192 {
        return;
    }
    crate::elog!("[chams] {} loaded shaders (dumping names for rim tuning):", n);
    let cap = n.min(400);
    for i in 0..cap {
        let sh = unsafe { runtime::array_get(arr, i) };
        if sh.is_null() {
            continue;
        }
        if let Ok(name) = unsafe { rt.invoke(c.get_name, sh, &mut []) } {
            if !name.is_null() {
                if let Some(s) = unsafe { rt.string_to_rust(name) } {
                    if s.to_lowercase().contains("rim")
                        || s.to_lowercase().contains("fresnel")
                        || s.to_lowercase().contains("outline")
                        || s.to_lowercase().contains("glow")
                    {
                        crate::elog!("[chams]   RIM? {}", s);
                    }
                }
            }
        }
    }
    crate::elog!("[chams] shader dump done (only rim/fresnel/outline/glow names shown)");
}

// ── per-frame entry ───────────────────────────────────────────────────────────

/// Called every frame from the driver (main thread). Cheap when disabled or
/// between ticks. Never propagates a failure to the caller.
pub unsafe fn tick(rt: &dyn ScriptRuntime, domain: Domain) {
    let enabled = ENABLED.load(Ordering::Relaxed);

    // Resolve once; needed for restore as well as apply.
    let cache = CACHE.get_or_init(|| unsafe { resolve(rt, domain) });
    let Some(c) = cache.as_deref() else {
        if enabled && ENABLED.swap(false, Ordering::Relaxed) {
            crate::elog!("[chams] resolution failed -- disabling");
        }
        return;
    };

    if !enabled {
        // Off: put originals back once, then idle. Drop the frozen walk so a
        // re-enable re-captures (useful if the first walk missed late arms).
        // Also drop the material handle: once no renderer references it, the
        // IL2CPP GC can collect it (our Rust static is not a scanned root), so
        // keeping the pointer would dangle. A re-enable rebuilds it fresh.
        let has = with_originals(|m| !m.is_empty()).unwrap_or(false);
        if has {
            unsafe { restore_all(rt, c) };
            if let Ok(mut g) = PLAYER_WALK.lock() {
                *g = None;
            }
            if let Ok(mut g) = MATERIAL.lock() {
                *g = 0;
            }
        }
        return;
    }

    // Throttle the active path.
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

    unsafe { log_loaded_shaders(rt, c) };

    let world = crate::hooks::game_world();

    // Scene/raid change: drop stale caches; our Instantiate'd material may have
    // been destroyed on unload, so it is rebuilt on the next cham.
    let wptr = world.raw() as usize;
    if wptr != LAST_WORLD.swap(wptr, Ordering::Relaxed) {
        if let Ok(mut g) = ORIGINALS.lock() {
            *g = None; // old renderers are gone; nothing to restore
        }
        if let Ok(mut g) = MATERIAL.lock() {
            *g = 0;
        }
        if let Ok(mut g) = PLAYER_WALK.lock() {
            *g = None;
        }
    }

    // Style/scheme change: mutate the one shared material in place. Instant and
    // reliable -- every chammed renderer already points at it, so nothing needs
    // reassigning.
    if APPLY.swap(false, Ordering::Relaxed) {
        unsafe { apply_style(rt, c, domain) };
    }

    let players = unsafe { enumerate_players(rt, c, world) };
    let np = players.len();
    if np == 0 {
        return;
    }

    // The local player, if GameWorld.MainPlayer resolved. The transform walk is
    // only needed for its first-person arms/weapon; every remote player's body
    // renderers come straight from Player._renderers (a direct array read, no
    // invokes), so walking them is a storm of wasted invokes on the first tick.
    let main_player = if c.main_player_off.is_some() && unsafe { runtime::is_alive(world) } {
        c.main_player_off
            .map(|o| unsafe { runtime::read_field::<usize>(world, o) })
            .filter(|&p| p != 0)
    } else {
        None
    };

    // Cham renderers we have not touched yet. Renderers already chammed are
    // skipped (in ORIGINALS), so re-walking each tick only ever adds new ones
    // (e.g. gear/weapon parts that stream in) and never reverts existing ones.
    let mut fill: Option<usize> = None;
    let mut touched = 0u32;
    for ptr in players {
        let pobj = Object(ptr as *mut c_void);
        if !unsafe { runtime::is_alive(pobj) } {
            continue;
        }

        // Union of Player._renderers (stable body, visible on other players) and
        // the transform-tree walk (first-person arms/weapon, visible on self).
        // The walk runs only for the local player -- or for everyone if
        // MainPlayer did not resolve -- since remote bodies need only _renderers.
        let mut seen: HashSet<usize> = HashSet::new();
        let mut renderers: Vec<usize> = Vec::new();
        for r in unsafe { player_renderers(c, pobj) } {
            if seen.insert(r) {
                renderers.push(r);
            }
        }
        let walk_this = main_player.map(|mp| mp == ptr).unwrap_or(true);
        if walk_this {
            for r in unsafe { cached_walk(rt, c, ptr, pobj) } {
                if seen.insert(r) {
                    renderers.push(r);
                }
            }
        }

        for rp in renderers {
            let renderer = Object(rp as *mut c_void);
            if renderer.is_null() {
                continue;
            }
            if unsafe { cham_renderer(rt, c, domain, renderer, &mut fill) } {
                touched += 1;
            }
        }
    }

    // touched > 0 only when new renderers were chammed, so this does not spam.
    if touched > 0 {
        crate::elog!(
            "[chams] chammed {} new renderers across {} player(s) [{} / {}]",
            touched, np, scheme_name(), style_name()
        );
    }
}
