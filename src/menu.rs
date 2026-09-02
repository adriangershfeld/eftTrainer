//! Win98-themed UGUI overlay built at runtime. All scene-graph work is on the
//! main thread via the driver hook. Input is plain Win32, which avoids
//! unboxing Unity value types out of an invoke.
//!
//! Every name here is Unity's own, so this file ports for free.

#![allow(dead_code)]

use crate::runtime::{self, Class, Domain, Method, Object, ScriptRuntime};
use crate::symbols::{self, unity};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_INSERT, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
};

// ── public toggle ─────────────────────────────────────────────────────────────
pub static VISIBLE: AtomicBool = AtomicBool::new(true);

// Runtime comes from runtime::get(); only the domain needs passing in.
static DOMAIN: OnceLock<Domain> = OnceLock::new();

pub fn set_domain(domain: Domain) {
    let _ = DOMAIN.set(domain);
}

/// Live status row, written by the worker thread. Keeps per-poll values out of
/// the log ring buffer, which they would otherwise evict in half a minute.
static STATUS: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

pub fn set_status(s: String) {
    if let Ok(mut g) = STATUS.lock() {
        *g = s;
    }
}

fn status_snapshot() -> String {
    STATUS.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Set by the worker (END). Every unload funnels through on_frame because
/// destroying GameObjects is main-thread only.
static UNLOAD_PENDING: AtomicBool = AtomicBool::new(false);

pub fn request_unload() {
    UNLOAD_PENDING.store(true, Ordering::Relaxed);
}

/// Destroys the overlay. Main thread only, called just before on_frame hands
/// back `false`.
unsafe fn teardown(rt: &dyn ScriptRuntime) {
    let Some(ms) = (unsafe { (*(&raw const MENU_STATE)).as_ref() }) else { return };
    if ms.m.obj_destroy.is_null() || ms.h.root_go.is_null() {
        crate::elog!("[menu] no Destroy available -- overlay will linger until the scene changes");
        return;
    }
    let mut args = [ms.h.root_go.raw()];
    unsafe { invoke_static(rt, ms.m.obj_destroy, &mut args) };
    crate::elog!("[menu] overlay destroyed");
}

// ── per-frame throttle ────────────────────────────────────────────────────────
static INITIALIZED: AtomicBool = AtomicBool::new(false);
static mut LAST_FRAME: Option<Instant> = None;

fn throttle_ok() -> bool {
    unsafe {
        match *(&raw const LAST_FRAME) {
            Some(ref t) if t.elapsed() < Duration::from_millis(33) => false,
            _ => {
                LAST_FRAME = Some(Instant::now());
                true
            }
        }
    }
}

// Counted from the driver (once per frame) rather than asked of Unity, which
// avoids unboxing a float out of an invoke. 500ms window.

static FPS_VALUE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static mut FPS_FRAMES: u32 = 0;
static mut FPS_WINDOW: Option<Instant> = None;

fn tick_fps() {
    unsafe {
        FPS_FRAMES += 1;
        let start = match *(&raw const FPS_WINDOW) {
            Some(t) => t,
            None => {
                FPS_WINDOW = Some(Instant::now());
                return;
            }
        };
        let elapsed = start.elapsed();
        if elapsed >= Duration::from_millis(500) {
            let fps = (FPS_FRAMES as f64 / elapsed.as_secs_f64()).round() as u32;
            FPS_VALUE.store(fps, Ordering::Relaxed);
            FPS_FRAMES = 0;
            FPS_WINDOW = Some(Instant::now());
        }
    }
}

// ── Win32 helpers (no value-type unboxing needed) ─────────────────────────────

fn lmb_clicked() -> bool {
    static PREV: AtomicBool = AtomicBool::new(false);
    let now = unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 };
    let prev = PREV.swap(now, Ordering::Relaxed);
    now && !prev
}

fn cursor_pos() -> (i32, i32) {
    let mut pt = POINT { x: 0, y: 0 };
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    (pt.x, pt.y)
}

fn screen_wh() -> (f32, f32) {
    unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN) as f32,
            GetSystemMetrics(SM_CYSCREEN) as f32,
        )
    }
}

// ── Value types for invoke args ───────────────────────────────────────────────
// Passed as *mut _ in the args slice; the runtime reads them by pointer.
#[repr(C)]
struct Rgba(f32, f32, f32, f32);
#[repr(C)]
struct V2(f32, f32);

// ── Layout ────────────────────────────────────────────────────────────────────
const WIN_W: f32 = 520.0;
const WIN_H: f32 = 400.0;
const TITLE_H: f32 = 20.0;
const CLOSE_SZ: f32 = 20.0;
const FPS_W: f32 = 70.0;
const STATUS_H: f32 = 18.0;
const PAD: f32 = 3.0;

/// Lines the widget renders; the ring buffer keeps more. Bounds mesh rebuild
/// cost. Turn down first if the overlay eats frames.
const CONSOLE_LINES: usize = 24;

/// Window position as a screen fraction. (0.5, 0.5) is centre, (0, 1) is top
/// left. close_hit reads the same constants so the hit box cannot drift.
/// Clips off the left edge below ~1400px wide.
const MENU_ANCHOR_X: f32 = 0.17;
const MENU_ANCHOR_Y: f32 = 0.70;

// ── Cached lookups (resolved once at init) ────────────────────────────────────
struct M {
    go_cls: Class,
    // GameObject / Object
    go_ctor_name: Method,
    go_add_comp: Method,
    go_get_comp: Method,
    go_set_active: Method,
    go_dont_destroy: Method,
    obj_destroy: Method,
    // Transform
    tf_set_parent: Method,
    // RectTransform
    rt_anc_min: Method,
    rt_anc_max: Method,
    rt_anc_pos: Method,
    rt_size_delta: Method,
    rt_off_min: Method,
    rt_off_max: Method,
    rt_pivot: Method,
    // Canvas
    cv_render_mode: Method,
    cv_sort_order: Method,
    // Graphic (base of Image and Text)
    gr_color: Method,
    gr_raycast: Method,
    // Text
    tx_text: Method,
    tx_font: Method,
    tx_font_size: Method,
    tx_alignment: Method,
    tx_horiz: Method,
    tx_vert: Method,
    // Type objects for AddComponent / GetComponent
    ty_canvas: Object,
    ty_image: Object,
    ty_text: Object,
    ty_rect_tf: Object,
    /// A UI.Text with a null font renders nothing, silently. AddComponent at
    /// runtime does not assign one; only the editor does.
    font: Object,
}

struct Handles {
    root_go: Object,
    window_rt: Object,
    title_rt: Object,
    close_rt: Object,
    console_text: Object,
    fps_text: Object,
    status_text: Object,
}

struct MenuState {
    m: M,
    h: Handles,
    last_text: String,
    last_fps: u32,
    last_status: String,
}

// Only ever touched from the main-thread driver.
static mut MENU_STATE: Option<MenuState> = None;

// ── Invoke helpers ────────────────────────────────────────────────────────────

unsafe fn invoke(rt: &dyn ScriptRuntime, m: Method, obj: Object, args: &mut [*mut c_void]) {
    if m.is_null() {
        return;
    }
    let _ = unsafe { rt.invoke(m, obj, args) };
}

unsafe fn invoke_static(rt: &dyn ScriptRuntime, m: Method, args: &mut [*mut c_void]) {
    if m.is_null() {
        return;
    }
    let _ = unsafe { rt.invoke_static(m, args) };
}

unsafe fn set_color(rt: &dyn ScriptRuntime, m: Method, obj: Object, r: f32, g: f32, b: f32, a: f32) {
    let mut c = Rgba(r, g, b, a);
    let mut args = [&mut c as *mut Rgba as *mut c_void];
    unsafe { invoke(rt, m, obj, &mut args) };
}

unsafe fn set_v2(rt: &dyn ScriptRuntime, m: Method, obj: Object, x: f32, y: f32) {
    let mut v = V2(x, y);
    let mut args = [&mut v as *mut V2 as *mut c_void];
    unsafe { invoke(rt, m, obj, &mut args) };
}

unsafe fn set_int(rt: &dyn ScriptRuntime, m: Method, obj: Object, val: i32) {
    let mut v = val;
    let mut args = [&mut v as *mut i32 as *mut c_void];
    unsafe { invoke(rt, m, obj, &mut args) };
}

unsafe fn set_bool(rt: &dyn ScriptRuntime, m: Method, obj: Object, val: bool) {
    let mut v: i32 = if val { 1 } else { 0 };
    let mut args = [&mut v as *mut i32 as *mut c_void];
    unsafe { invoke(rt, m, obj, &mut args) };
}

// ── GO construction helpers ───────────────────────────────────────────────────

unsafe fn new_go(
    rt: &dyn ScriptRuntime,
    domain: Domain,
    m: &M,
    name: &str,
) -> Option<Object> {
    let go = unsafe { rt.new_object(domain, m.go_cls) }?;
    let name_str = unsafe { rt.new_string(domain, name) }?;
    let mut args = [name_str.raw()];
    let _ = unsafe { rt.invoke(m.go_ctor_name, go, &mut args) };
    Some(go)
}

unsafe fn add_comp(rt: &dyn ScriptRuntime, go: Object, m: &M, type_obj: Object) -> Object {
    let mut args = [type_obj.raw()];
    unsafe { rt.invoke(m.go_add_comp, go, &mut args) }.unwrap_or(Object::NULL)
}

unsafe fn get_comp(rt: &dyn ScriptRuntime, go: Object, m: &M, type_obj: Object) -> Object {
    let mut args = [type_obj.raw()];
    unsafe { rt.invoke(m.go_get_comp, go, &mut args) }.unwrap_or(Object::NULL)
}

unsafe fn set_parent(rt: &dyn ScriptRuntime, m: &M, child_rt: Object, parent_rt: Object) {
    let mut world_pos_stays: i32 = 0;
    let mut args = [
        parent_rt.raw(),
        &mut world_pos_stays as *mut i32 as *mut c_void,
    ];
    unsafe { invoke(rt, m.tf_set_parent, child_rt, &mut args) };
}

unsafe fn setup_rt(
    rt: &dyn ScriptRuntime,
    m: &M,
    target: Object,
    anc_min: (f32, f32),
    anc_max: (f32, f32),
    pos: (f32, f32),
    size: (f32, f32),
) {
    unsafe {
        set_v2(rt, m.rt_anc_min, target, anc_min.0, anc_min.1);
        set_v2(rt, m.rt_anc_max, target, anc_max.0, anc_max.1);
        set_v2(rt, m.rt_pivot, target, 0.5, 0.5);
        set_v2(rt, m.rt_anc_pos, target, pos.0, pos.1);
        set_v2(rt, m.rt_size_delta, target, size.0, size.1);
    }
}

// ── Method / type resolution ──────────────────────────────────────────────────

/// Resources.GetBuiltinResource(typeof(Font), name), first candidate that hits.
unsafe fn resolve_font(rt: &dyn ScriptRuntime, domain: Domain) -> Option<Object> {
    let res_cls = unsafe { symbols::find_class(rt, domain, unity::RESOURCES) }?;
    let get_builtin =
        unsafe { symbols::find_method(rt, res_cls, unity::GET_BUILTIN_RESOURCE, 2) }?;
    let font_type = unsafe { symbols::find_type_object(rt, domain, unity::FONT) }?;

    for name in unity::BUILTIN_FONTS {
        let Some(name_str) = (unsafe { rt.new_string(domain, name) }) else { continue };
        let mut args = [font_type.raw(), name_str.raw()];
        match unsafe { rt.invoke_static(get_builtin, &mut args) } {
            Some(f) if !f.is_null() => {
                crate::elog!("[menu] font: {}", name);
                return Some(f);
            }
            _ => crate::elog!("[menu] builtin font {} not available", name),
        }
    }
    None
}

unsafe fn resolve_methods(rt: &dyn ScriptRuntime, domain: Domain) -> Option<M> {
    // Log every miss: invoke() no-ops on a null method, so a silent null
    // builds the whole tree with every setter dead and no diagnostics.
    macro_rules! need_class {
        ($c:expr) => {
            match unsafe { symbols::find_class(rt, domain, $c) } {
                Some(v) => v,
                None => return None,
            }
        };
    }

    let go_cls = need_class!(unity::GAME_OBJECT);
    let obj_cls = need_class!(unity::OBJECT);
    let tf_cls = need_class!(unity::TRANSFORM);
    let rt_cls = need_class!(unity::RECT_TRANSFORM);
    let cv_cls = need_class!(unity::CANVAS);
    let gr_cls = need_class!(unity::GRAPHIC);
    let tx_cls = need_class!(unity::TEXT);
    crate::elog!("[menu] classes resolved");

    let ty_canvas = unsafe { symbols::find_type_object(rt, domain, unity::CANVAS) }
        .unwrap_or(Object::NULL);
    let ty_image = unsafe { symbols::find_type_object(rt, domain, unity::IMAGE) }
        .unwrap_or(Object::NULL);
    let ty_text = unsafe { symbols::find_type_object(rt, domain, unity::TEXT) }
        .unwrap_or(Object::NULL);
    let ty_rect_tf = unsafe { symbols::find_type_object(rt, domain, unity::RECT_TRANSFORM) }
        .unwrap_or(Object::NULL);

    let font = unsafe { resolve_font(rt, domain) }.unwrap_or(Object::NULL);
    if font.is_null() {
        crate::elog!("[menu] NO FONT -- all labels will be invisible");
    }

    let mut missing: u32 = 0;
    macro_rules! mex {
        ($cls:expr, $name:expr, $n:expr) => {{
            match unsafe { rt.method_exact($cls, $name, $n) } {
                Some(m) => m,
                None => {
                    crate::elog!("[menu] MISSING method: {}", $name);
                    missing += 1;
                    Method::NULL
                }
            }
        }};
    }
    macro_rules! mwalk {
        ($cls:expr, $name:expr, $n:expr) => {{
            match unsafe { rt.method($cls, $name, $n) } {
                Some(m) => m,
                None => {
                    crate::elog!("[menu] MISSING method (walk): {}", $name);
                    missing += 1;
                    Method::NULL
                }
            }
        }};
    }

    let resolved = M {
        go_cls,
        go_ctor_name: mex!(go_cls, unity::CTOR, 1),
        go_add_comp: mex!(go_cls, unity::ADD_COMPONENT, 1),
        go_get_comp: mex!(go_cls, unity::GET_COMPONENT, 1),
        go_set_active: mwalk!(go_cls, unity::SET_ACTIVE, 1),
        go_dont_destroy: mwalk!(obj_cls, unity::DONT_DESTROY_ON_LOAD, 1),
        obj_destroy: mex!(obj_cls, unity::DESTROY, 1),
        tf_set_parent: mex!(tf_cls, unity::SET_PARENT, 2),
        rt_anc_min: mex!(rt_cls, unity::SET_ANCHOR_MIN, 1),
        rt_anc_max: mex!(rt_cls, unity::SET_ANCHOR_MAX, 1),
        rt_anc_pos: mex!(rt_cls, unity::SET_ANCHORED_POSITION, 1),
        rt_size_delta: mex!(rt_cls, unity::SET_SIZE_DELTA, 1),
        rt_off_min: mex!(rt_cls, unity::SET_OFFSET_MIN, 1),
        rt_off_max: mex!(rt_cls, unity::SET_OFFSET_MAX, 1),
        rt_pivot: mex!(rt_cls, unity::SET_PIVOT, 1),
        cv_render_mode: mex!(cv_cls, unity::SET_RENDER_MODE, 1),
        cv_sort_order: mex!(cv_cls, unity::SET_SORTING_ORDER, 1),
        gr_color: mex!(gr_cls, unity::SET_COLOR, 1),
        gr_raycast: mex!(gr_cls, unity::SET_RAYCAST_TARGET, 1),
        tx_text: mex!(tx_cls, unity::SET_TEXT, 1),
        tx_font: mex!(tx_cls, unity::SET_FONT, 1),
        tx_font_size: mex!(tx_cls, unity::SET_FONT_SIZE, 1),
        tx_alignment: mex!(tx_cls, unity::SET_ALIGNMENT, 1),
        tx_horiz: mex!(tx_cls, unity::SET_HORIZONTAL_OVERFLOW, 1),
        tx_vert: mex!(tx_cls, unity::SET_VERTICAL_OVERFLOW, 1),
        ty_canvas,
        ty_image,
        ty_text,
        ty_rect_tf,
        font,
    };

    if ty_canvas.is_null() || ty_image.is_null() || ty_text.is_null() || ty_rect_tf.is_null() {
        crate::elog!("[menu] a required Type object is null -- AddComponent/GetComponent cannot work");
        return None;
    }
    // Without these four the tree is meaningless; anything else just makes it
    // ugly rather than invisible.
    if resolved.go_ctor_name.is_null()
        || resolved.go_add_comp.is_null()
        || resolved.go_get_comp.is_null()
        || resolved.tf_set_parent.is_null()
    {
        crate::elog!("[menu] core GameObject/Transform methods missing -- aborting build");
        return None;
    }
    if missing > 0 {
        crate::elog!("[menu] {} method(s) missing -- menu will build but look wrong", missing);
    } else {
        crate::elog!("[menu] all methods resolved");
    }
    Some(resolved)
}

// ── UI tree construction ──────────────────────────────────────────────────────

/// (gameobject, Text component, RectTransform)
unsafe fn make_text_go(
    rt: &dyn ScriptRuntime,
    domain: Domain,
    m: &M,
    parent_rt: Object,
    name: &str,
    text: &str,
    fs: i32,
    r: f32,
    g: f32,
    b: f32,
    align: i32,
    horiz: i32,
    vert: i32,
) -> (Object, Object, Object) {
    let Some(go) = (unsafe { new_go(rt, domain, m, name) }) else {
        return (Object::NULL, Object::NULL, Object::NULL);
    };
    let tx = unsafe { add_comp(rt, go, m, m.ty_text) };
    let trt = unsafe { get_comp(rt, go, m, m.ty_rect_tf) };
    unsafe { set_parent(rt, m, trt, parent_rt) };

    // Font first: no font, no glyphs, whatever else is set.
    if !m.font.is_null() {
        let mut args = [m.font.raw()];
        unsafe { invoke(rt, m.tx_font, tx, &mut args) };
    }
    if let Some(s) = unsafe { rt.new_string(domain, text) } {
        let mut args = [s.raw()];
        unsafe { invoke(rt, m.tx_text, tx, &mut args) };
    }
    unsafe {
        set_int(rt, m.tx_font_size, tx, fs);
        set_color(rt, m.gr_color, tx, r, g, b, 1.0);
        set_int(rt, m.tx_alignment, tx, align);
        set_int(rt, m.tx_horiz, tx, horiz);
        set_int(rt, m.tx_vert, tx, vert);
        set_bool(rt, m.gr_raycast, tx, false);
    }
    (go, tx, trt)
}

/// (gameobject, Image component, RectTransform)
unsafe fn make_panel(
    rt: &dyn ScriptRuntime,
    domain: Domain,
    m: &M,
    name: &str,
    parent_rt: Object,
) -> (Object, Object, Object) {
    let Some(go) = (unsafe { new_go(rt, domain, m, name) }) else {
        return (Object::NULL, Object::NULL, Object::NULL);
    };
    let img = unsafe { add_comp(rt, go, m, m.ty_image) };
    let prt = unsafe { get_comp(rt, go, m, m.ty_rect_tf) };
    unsafe { set_bool(rt, m.gr_raycast, img, false) };
    if !parent_rt.is_null() {
        unsafe { set_parent(rt, m, prt, parent_rt) };
    }
    (go, img, prt)
}

unsafe fn build_ui(rt: &dyn ScriptRuntime, domain: Domain, m: &M) -> Option<Handles> {
    // ── Canvas (root) ─────────────────────────────────────────────────────────
    let root_go = unsafe { new_go(rt, domain, m, "eftMenu_canvas") }?;
    let canvas = unsafe { add_comp(rt, root_go, m, m.ty_canvas) };
    unsafe {
        set_int(rt, m.cv_render_mode, canvas, unity::RENDER_MODE_SCREEN_SPACE_OVERLAY);
        set_int(rt, m.cv_sort_order, canvas, 999);
        invoke_static(rt, m.go_dont_destroy, &mut [root_go.raw()]);
    }
    let canvas_rt = unsafe { get_comp(rt, root_go, m, m.ty_rect_tf) };
    unsafe { setup_rt(rt, m, canvas_rt, (0.0, 0.0), (1.0, 1.0), (0.0, 0.0), (0.0, 0.0)) };

    // ── Window (Win98 gray) ───────────────────────────────────────────────────
    let (_, win_img, window_rt) = unsafe { make_panel(rt, domain, m, "eftMenu_window", canvas_rt) };
    unsafe {
        set_color(rt, m.gr_color, win_img, 0.753, 0.753, 0.753, 1.0); // #C0C0C0
        setup_rt(
            rt, m, window_rt,
            (MENU_ANCHOR_X, MENU_ANCHOR_Y),
            (MENU_ANCHOR_X, MENU_ANCHOR_Y),
            (0.0, 0.0),
            (WIN_W, WIN_H),
        );
    }

    // ── Title bar (navy) ──────────────────────────────────────────────────────
    let (_, tb_img, title_rt) = unsafe { make_panel(rt, domain, m, "eftMenu_title", window_rt) };
    unsafe {
        set_color(rt, m.gr_color, tb_img, 0.0, 0.0, 0.502, 1.0); // #000080
        // sizeDelta IS the size where anchorMin.y == anchorMax.y, not an
        // offset. This was -TITLE_H, so the bar and its children never drew.
        setup_rt(rt, m, title_rt, (0.0, 1.0), (1.0, 1.0), (0.0, 0.0), (0.0, TITLE_H));
        set_v2(rt, m.rt_pivot, title_rt, 0.5, 1.0);
    }

    // ── Title text ────────────────────────────────────────────────────────────
    let (_, _, titletext_rt) = unsafe {
        make_text_go(
            rt, domain, m, title_rt, "eftMenu_titletxt", "eftTrainer", 11,
            1.0, 1.0, 1.0,
            unity::ANCHOR_MIDDLE_LEFT, unity::WRAP_OVERFLOW, unity::WRAP_TRUNCATE,
        )
    };
    // Fill the title bar, minus space for the close button.
    unsafe {
        set_v2(rt, m.rt_anc_min, titletext_rt, 0.0, 0.0);
        set_v2(rt, m.rt_anc_max, titletext_rt, 1.0, 1.0);
        set_v2(rt, m.rt_off_min, titletext_rt, PAD, 0.0);
        set_v2(rt, m.rt_off_max, titletext_rt, -(CLOSE_SZ + FPS_W + PAD), 0.0);
    }

    // ── FPS readout, right side of the title bar before the X ─────────────────
    let (_, fps_text, fps_rt) = unsafe {
        make_text_go(
            rt, domain, m, title_rt, "eftMenu_fps", "-- fps", 11,
            1.0, 1.0, 1.0,
            unity::ANCHOR_MIDDLE_RIGHT, unity::WRAP_OVERFLOW, unity::WRAP_TRUNCATE,
        )
    };
    unsafe {
        set_v2(rt, m.rt_anc_min, fps_rt, 1.0, 0.0);
        set_v2(rt, m.rt_anc_max, fps_rt, 1.0, 1.0);
        set_v2(rt, m.rt_pivot, fps_rt, 1.0, 0.5);
        set_v2(rt, m.rt_anc_pos, fps_rt, -(CLOSE_SZ + PAD), 0.0);
        set_v2(rt, m.rt_size_delta, fps_rt, FPS_W, 0.0);
    }

    // ── Close button ──────────────────────────────────────────────────────────
    let (_, close_img, close_rt) = unsafe { make_panel(rt, domain, m, "eftMenu_close", title_rt) };
    unsafe {
        set_color(rt, m.gr_color, close_img, 0.753, 0.753, 0.753, 1.0);
        set_v2(rt, m.rt_anc_min, close_rt, 1.0, 0.0);
        set_v2(rt, m.rt_anc_max, close_rt, 1.0, 1.0);
        set_v2(rt, m.rt_pivot, close_rt, 1.0, 0.5);
        set_v2(rt, m.rt_anc_pos, close_rt, 0.0, 0.0);
        set_v2(rt, m.rt_size_delta, close_rt, CLOSE_SZ, 0.0);
    }
    unsafe {
        make_text_go(
            rt, domain, m, close_rt, "eftMenu_closex", "\u{00D7}", 13,
            0.0, 0.0, 0.0,
            unity::ANCHOR_MIDDLE_CENTER, unity::WRAP_OVERFLOW, unity::WRAP_TRUNCATE,
        )
    };

    // ── Status row (live values, sits between title bar and console) ──────────
    let (_, status_text, status_rt) = unsafe {
        make_text_go(
            rt, domain, m, window_rt, "eftMenu_status", "", 11,
            0.9, 0.9, 0.35,
            unity::ANCHOR_MIDDLE_LEFT, unity::WRAP_OVERFLOW, unity::WRAP_TRUNCATE,
        )
    };
    unsafe {
        set_v2(rt, m.rt_anc_min, status_rt, 0.0, 1.0);
        set_v2(rt, m.rt_anc_max, status_rt, 1.0, 1.0);
        set_v2(rt, m.rt_pivot, status_rt, 0.5, 1.0);
        set_v2(rt, m.rt_anc_pos, status_rt, 0.0, -TITLE_H);
        set_v2(rt, m.rt_size_delta, status_rt, -(PAD * 2.0), STATUS_H);
    }

    // ── Console area ──────────────────────────────────────────────────────────
    let (_, con_img, con_rt) = unsafe { make_panel(rt, domain, m, "eftMenu_con", window_rt) };
    unsafe {
        set_color(rt, m.gr_color, con_img, 0.098, 0.098, 0.098, 0.98);
        set_v2(rt, m.rt_anc_min, con_rt, 0.0, 0.0);
        set_v2(rt, m.rt_anc_max, con_rt, 1.0, 1.0);
        set_v2(rt, m.rt_off_min, con_rt, PAD, PAD);
        set_v2(rt, m.rt_off_max, con_rt, -PAD, -(TITLE_H + STATUS_H + PAD));
    }

    // ── Console text ──────────────────────────────────────────────────────────
    let (_, console_text, con_txt_rt) = unsafe {
        make_text_go(
            rt, domain, m, con_rt, "eftMenu_contxt", "", 10,
            0.0, 0.867, 0.0,
            unity::ANCHOR_UPPER_LEFT, unity::WRAP_OVERFLOW, unity::WRAP_OVERFLOW,
        )
    };
    unsafe {
        set_v2(rt, m.rt_anc_min, con_txt_rt, 0.0, 0.0);
        set_v2(rt, m.rt_anc_max, con_txt_rt, 1.0, 1.0);
        set_v2(rt, m.rt_off_min, con_txt_rt, 2.0, 2.0);
        set_v2(rt, m.rt_off_max, con_txt_rt, -2.0, -2.0);
    }

    crate::elog!("[menu] UI tree constructed");
    Some(Handles {
        root_go, window_rt, title_rt, close_rt, console_text, fps_text, status_text,
    })
}

// ── Close-button hit detection (Win32 coords, top-left origin) ────────────────

fn close_hit(sw: f32, sh: f32, cx: i32, cy: i32) -> bool {
    // Unity's Y runs up, Win32's runs down, hence 1.0 - ANCHOR_Y.
    let centre_x = MENU_ANCHOR_X * sw;
    let centre_y = (1.0 - MENU_ANCHOR_Y) * sh;
    let win_top = centre_y - WIN_H / 2.0;
    let win_right = centre_x + WIN_W / 2.0;
    let btn_left = win_right - CLOSE_SZ;
    let btn_top = win_top;
    let btn_bot = win_top + TITLE_H;
    let x = cx as f32;
    let y = cy as f32;
    x >= btn_left && x <= win_right && y >= btn_top && y <= btn_bot
}

// ── Public surface ────────────────────────────────────────────────────────────

/// Called from the driver hook, always main thread. False requests unload.
pub unsafe fn on_frame() -> bool {
    // build_ui makes hundreds of calls back into Unity; a reentrant one would
    // start a second resolve+build and shred MENU_STATE.
    static IN_FRAME: AtomicBool = AtomicBool::new(false);
    if IN_FRAME.swap(true, Ordering::Acquire) {
        return true;
    }
    struct FrameGuard;
    impl Drop for FrameGuard {
        fn drop(&mut self) {
            IN_FRAME.store(false, Ordering::Release);
        }
    }
    let _frame_guard = FrameGuard;

    // Separates "driver fires" from "UI builds".
    static FIRST_CALL: AtomicBool = AtomicBool::new(false);
    if !FIRST_CALL.swap(true, Ordering::Relaxed) {
        crate::elog!("[menu] on_frame: driver is alive (first call)");
    }

    // Before the throttle, or it measures the throttle.
    tick_fps();

    if !throttle_ok() {
        return true;
    }

    let Some(rt) = runtime::get() else { return true };
    let Some(&domain) = DOMAIN.get() else { return true };

    // Swap unconditionally. `now && !PREV.swap(now, ..)` short-circuits on key
    // release, so PREV latches true and the toggle only ever fires once.
    static INSERT_PREV: AtomicBool = AtomicBool::new(false);
    let insert_now = unsafe { (GetAsyncKeyState(VK_INSERT.0 as i32) as u16 & 0x8000) != 0 };
    let insert_prev = INSERT_PREV.swap(insert_now, Ordering::Relaxed);
    let insert_edge = insert_now && !insert_prev;
    if insert_edge {
        let now_visible = !VISIBLE.fetch_xor(true, Ordering::Relaxed);
        if let Some(ms) = unsafe { (*(&raw const MENU_STATE)).as_ref() } {
            let mut b: i32 = if now_visible { 1 } else { 0 };
            let mut args = [&mut b as *mut i32 as *mut c_void];
            unsafe { invoke(rt, ms.m.go_set_active, ms.h.root_go, &mut args) };
        }
    }

    // Lazy init, first call on the main thread.
    if !INITIALIZED.load(Ordering::Acquire) {
        crate::elog!("[menu] initialising UGUI tree ({} backend)...", rt.backend().name());
        let Some(m) = (unsafe { resolve_methods(rt, domain) }) else {
            crate::elog!("[menu] resolve_methods failed -- menu disabled");
            INITIALIZED.store(true, Ordering::Release);
            return true;
        };
        let Some(h) = (unsafe { build_ui(rt, domain, &m) }) else {
            crate::elog!("[menu] build_ui failed -- menu disabled");
            INITIALIZED.store(true, Ordering::Release);
            return true;
        };
        unsafe {
            MENU_STATE = Some(MenuState {
                m,
                h,
                last_text: String::new(),
                last_fps: u32::MAX,
                last_status: String::new(),
            })
        };
        INITIALIZED.store(true, Ordering::Release);
        crate::elog!("[menu] ready");
    }

    let Some(ms) = (unsafe { (*(&raw mut MENU_STATE)).as_mut() }) else { return true };

    // Only on change: a Text write forces a canvas rebuild.
    let fps = FPS_VALUE.load(Ordering::Relaxed);
    if fps != ms.last_fps {
        if let Some(s) = unsafe { rt.new_string(domain, &format!("{} fps", fps)) } {
            let mut args = [s.raw()];
            unsafe { invoke(rt, ms.m.tx_text, ms.h.fps_text, &mut args) };
        }
        ms.last_fps = fps;
    }

    // Live status row.
    let status = status_snapshot();
    if status != ms.last_status {
        if let Some(s) = unsafe { rt.new_string(domain, &status) } {
            let mut args = [s.raw()];
            unsafe { invoke(rt, ms.m.tx_text, ms.h.status_text, &mut args) };
        }
        ms.last_status = status;
    }

    // Mirror the tail of the console ring buffer into the in-game widget.
    // None means the buffer was locked this frame; skip rather than blank it.
    if let Some(snap) = crate::console::snapshot_tail(CONSOLE_LINES) {
        if snap != ms.last_text {
            if let Some(s) = unsafe { rt.new_string(domain, &snap) } {
                let mut args = [s.raw()];
                unsafe { invoke(rt, ms.m.tx_text, ms.h.console_text, &mut args) };
            }
            ms.last_text = snap;
        }
    }

    // Unload, from either the X button here or END on the worker thread.
    // Both land on this thread so the Unity teardown is legal.
    let x_clicked = lmb_clicked() && VISIBLE.load(Ordering::Relaxed) && {
        let (sw, sh) = screen_wh();
        let (cx, cy) = cursor_pos();
        close_hit(sw, sh, cx, cy)
    };
    if x_clicked || UNLOAD_PENDING.load(Ordering::Relaxed) {
        crate::elog!(
            "[menu] unload requested ({}) -- tearing down",
            if x_clicked { "X button" } else { "END key" }
        );
        unsafe { teardown(rt) };
        return false;
    }

    true
}
