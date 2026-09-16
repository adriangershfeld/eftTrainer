//! Win98-themed tabbed overlay built at runtime. All scene-graph work is on the
//! main thread via the driver hook. Input is plain Win32 against a self-drawn
//! cursor (input.rs), which avoids unboxing Unity value types out of an invoke.
//!
//! Layout is declarative: every clickable region is an LRect in window-local
//! (or content-local) pixels, and the same rect both places the widget and
//! hit-tests the click, so the two can never drift.

#![allow(dead_code)]

use crate::runtime::{self, Class, Domain, Method, Object, ScriptRuntime};
use crate::symbols::{self, unity};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_INSERT, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
};

// ── public toggle ─────────────────────────────────────────────────────────────
pub static VISIBLE: AtomicBool = AtomicBool::new(true);
static ACTIVE_TAB: AtomicUsize = AtomicUsize::new(1); // start on Visuals

// Frames on_frame has processed. Exposed via HTTP so the driver's liveness and
// the real frame rate can be checked without seeing the screen (diagnostics).
pub static FRAME_COUNT: AtomicU64 = AtomicU64::new(0);
// on_frame body duration, microseconds. Tells whether the trainer is what
// costs the frame (max/avg) without needing to see the screen.
static ONFRAME_LAST_US: AtomicU64 = AtomicU64::new(0);
static ONFRAME_MAX_US: AtomicU64 = AtomicU64::new(0);
static ONFRAME_TOTAL_US: AtomicU64 = AtomicU64::new(0);
pub fn frame_count() -> u64 {
    FRAME_COUNT.load(Ordering::Relaxed)
}
pub fn is_visible() -> bool {
    VISIBLE.load(Ordering::Relaxed)
}
/// Set menu visibility (e.g. from the HTTP control endpoint). The per-frame
/// reconcile in on_frame applies the actual SetActive on the main thread.
pub fn set_visible(on: bool) {
    VISIBLE.store(on, Ordering::Relaxed);
}
pub fn fps() -> u32 {
    FPS_VALUE.load(Ordering::Relaxed)
}
/// (last, max, avg) on_frame duration in microseconds.
pub fn onframe_us() -> (u64, u64, u64) {
    let frames = FRAME_COUNT.load(Ordering::Relaxed).max(1);
    (
        ONFRAME_LAST_US.load(Ordering::Relaxed),
        ONFRAME_MAX_US.load(Ordering::Relaxed),
        ONFRAME_TOTAL_US.load(Ordering::Relaxed) / frames,
    )
}
/// Live virtual cursor, so its tracking can be checked from HTTP.
pub fn cursor() -> (i32, i32) {
    crate::input::virtual_cursor()
}

static DOMAIN: OnceLock<Domain> = OnceLock::new();
pub fn set_domain(domain: Domain) {
    let _ = DOMAIN.set(domain);
}

/// Live status row, written by the worker thread.
static STATUS: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static STATUS_DIRTY: AtomicBool = AtomicBool::new(false);
pub fn set_status(s: String) {
    if let Ok(mut g) = STATUS.lock() {
        *g = s;
        STATUS_DIRTY.store(true, Ordering::Relaxed);
    }
}
fn status_dirty() -> bool {
    STATUS_DIRTY.swap(false, Ordering::Relaxed)
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

// ── per-frame throttle + fps ──────────────────────────────────────────────────
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

// ── Win32 helpers ─────────────────────────────────────────────────────────────
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

// Screen size is fixed for the session, but GetSystemMetrics is a syscall and
// this was being called twice every frame. Resolve once, then serve from
// atomics (matches input.rs, which already caches the same pair).
static SCR_W: AtomicI32 = AtomicI32::new(0);
static SCR_H: AtomicI32 = AtomicI32::new(0);
fn screen_wh() -> (f32, f32) {
    let w = SCR_W.load(Ordering::Relaxed);
    if w != 0 {
        return (w as f32, SCR_H.load(Ordering::Relaxed) as f32);
    }
    let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    SCR_W.store(w, Ordering::Relaxed);
    SCR_H.store(h, Ordering::Relaxed);
    (w as f32, h as f32)
}

// ── value types for invoke args ───────────────────────────────────────────────
#[repr(C)]
struct Rgba(f32, f32, f32, f32);
#[repr(C)]
struct V2(f32, f32);

// ── layout (window-local pixels, y down from the window's top-left) ───────────
const WIN_W: f32 = 520.0;
const WIN_H: f32 = 400.0;
const TITLE_H: f32 = 20.0;
const CLOSE_SZ: f32 = 18.0;
const FPS_W: f32 = 66.0;
const STATUS_H: f32 = 16.0;
const PAD: f32 = 3.0;
const CONSOLE_LINES: usize = 22;

const TAB_Y: f32 = TITLE_H;
const TAB_H: f32 = 22.0;
const TAB_W: f32 = 84.0;
const TAB_GAP: f32 = 1.0;
const TAB_X0: f32 = 4.0;
const N_TABS: usize = 4;
const TAB_NAMES: [&str; N_TABS] = ["Weapon", "Visuals", "Misc", "Console"];

const CONTENT_X: f32 = 3.0;
const CONTENT_Y: f32 = TAB_Y + TAB_H;
const CONTENT_W: f32 = WIN_W - CONTENT_X * 2.0;
// Leave a status strip at the very bottom of the window.
const CONTENT_H: f32 = WIN_H - CONTENT_Y - STATUS_H - PAD * 2.0;
const STATUS_Y: f32 = CONTENT_Y + CONTENT_H + PAD;

const MENU_ANCHOR_X: f32 = 0.17;
const MENU_ANCHOR_Y: f32 = 0.70;

// ── Win98 palette ─────────────────────────────────────────────────────────────
const C_FACE: (f32, f32, f32) = (0.753, 0.753, 0.753);
const C_LIGHT: (f32, f32, f32) = (1.0, 1.0, 1.0);
const C_SHADOW: (f32, f32, f32) = (0.5, 0.5, 0.5);
const C_DARK: (f32, f32, f32) = (0.0, 0.0, 0.0);
const C_NAVY: (f32, f32, f32) = (0.0, 0.0, 0.502);
const C_FIELD: (f32, f32, f32) = (1.0, 1.0, 1.0);
const C_TAB_OFF: (f32, f32, f32) = (0.66, 0.66, 0.66);
// Checkbox fill when checked: a solid navy square (alpha toggled on/off).
const C_CHECK: (f32, f32, f32) = (0.0, 0.0, 0.502);

// A window/content-local rectangle. One value both places a widget and
// hit-tests the click against it.
#[derive(Clone, Copy)]
struct LRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}
const fn lr(x: f32, y: f32, w: f32, h: f32) -> LRect {
    LRect { x, y, w, h }
}

fn win_origin(sw: f32, sh: f32) -> (f32, f32) {
    (
        MENU_ANCHOR_X * sw - WIN_W / 2.0,
        (1.0 - MENU_ANCHOR_Y) * sh - WIN_H / 2.0,
    )
}

/// Screen-pixel centre of the window, so the pointer opens on the menu instead
/// of at screen centre (which is nowhere near it).
fn window_center() -> (i32, i32) {
    let (sw, sh) = screen_wh();
    let (ox, oy) = win_origin(sw, sh);
    ((ox + WIN_W / 2.0) as i32, (oy + WIN_H / 2.0) as i32)
}

/// Hit-test a window-local rect against a Win32 screen point.
fn hit(r: LRect, sw: f32, sh: f32, cx: i32, cy: i32) -> bool {
    let (ox, oy) = win_origin(sw, sh);
    let (x, y) = (cx as f32, cy as f32);
    x >= ox + r.x && x <= ox + r.x + r.w && y >= oy + r.y && y <= oy + r.y + r.h
}

/// Hit-test a content-local rect (adds the content panel's offset).
fn hit_content(r: LRect, sw: f32, sh: f32, cx: i32, cy: i32) -> bool {
    hit(lr(r.x + CONTENT_X, r.y + CONTENT_Y, r.w, r.h), sw, sh, cx, cy)
}

fn tab_rect(i: usize) -> LRect {
    lr(TAB_X0 + i as f32 * (TAB_W + TAB_GAP), TAB_Y, TAB_W, TAB_H)
}

const CLOSE_R: LRect = lr(WIN_W - CLOSE_SZ - 2.0, 2.0, CLOSE_SZ, CLOSE_SZ);

// Visuals tab controls (content-local).
const V_CHAMS: LRect = lr(14.0, 16.0, 240.0, 18.0);
const V_STYLE: LRect = lr(14.0, 42.0, 210.0, 22.0);
const V_SCHEME: LRect = lr(14.0, 70.0, 210.0, 22.0);

// Misc tab controls (content-local).
const M_STAM: LRect = lr(14.0, 16.0, 240.0, 18.0);
const M_SPEED: LRect = lr(14.0, 44.0, 130.0, 18.0);
const M_SPEED_MINUS: LRect = lr(150.0, 42.0, 24.0, 22.0);
const M_SPEED_VAL: LRect = lr(178.0, 44.0, 52.0, 18.0);
const M_SPEED_PLUS: LRect = lr(232.0, 42.0, 24.0, 22.0);
const M_CONFIG: LRect = lr(14.0, 78.0, 210.0, 22.0);

// ── cached lookups (resolved once at init) ────────────────────────────────────
struct M {
    go_cls: Class,
    go_ctor_name: Method,
    go_add_comp: Method,
    go_get_comp: Method,
    go_set_active: Method,
    go_dont_destroy: Method,
    obj_destroy: Method,
    tf_set_parent: Method,
    rt_anc_min: Method,
    rt_anc_max: Method,
    rt_anc_pos: Method,
    rt_size_delta: Method,
    rt_off_min: Method,
    rt_off_max: Method,
    rt_pivot: Method,
    cv_render_mode: Method,
    cv_sort_order: Method,
    gr_color: Method,
    gr_raycast: Method,
    tx_text: Method,
    tx_font: Method,
    tx_font_size: Method,
    tx_alignment: Method,
    tx_horiz: Method,
    tx_vert: Method,
    cur_set_visible: Method,
    ty_canvas: Object,
    ty_image: Object,
    ty_text: Object,
    ty_rect_tf: Object,
    font: Object,
}

struct Handles {
    root_go: Object,
    window_rt: Object,
    title_rt: Object,
    tab_img: [Object; N_TABS],
    tab_content: [Object; N_TABS],
    console_text: Object,
    fps_text: Object,
    status_text: Object,
    // Visuals
    chams_check: Object,
    style_txt: Object,
    scheme_txt: Object,
    // Misc
    stam_check: Object,
    speed_check: Object,
    speed_txt: Object,
    // pointer (on its own canvas so moving it never rebuilds the menu canvas)
    cursor_canvas: Object,
    cursor_rt: Object,
}

struct MenuState {
    m: M,
    h: Handles,
    last_text: String,
    last_fps: u32,
    last_status: String,
    last_tab: usize,
    last_chams_on: Option<bool>,
    last_style: String,
    last_scheme: String,
    last_stam: Option<bool>,
    last_speed_on: Option<bool>,
    last_speed_mult: f32,
    last_cur: (i32, i32),
}

static mut MENU_STATE: Option<MenuState> = None;

// ── invoke helpers ────────────────────────────────────────────────────────────
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

unsafe fn set_color(rt: &dyn ScriptRuntime, m: Method, obj: Object, c: (f32, f32, f32), a: f32) {
    let mut v = Rgba(c.0, c.1, c.2, a);
    let mut args = [&mut v as *mut Rgba as *mut c_void];
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
    unsafe { set_int(rt, m, obj, if val { 1 } else { 0 }) };
}

// ── GO construction ───────────────────────────────────────────────────────────
unsafe fn new_go(rt: &dyn ScriptRuntime, domain: Domain, m: &M, name: &str) -> Option<Object> {
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
    let mut args = [parent_rt.raw(), &mut world_pos_stays as *mut i32 as *mut c_void];
    unsafe { invoke(rt, m.tf_set_parent, child_rt, &mut args) };
}

/// Place a child at (r.x, r.y) parent-local, size (r.w, r.h). Anchor + pivot at
/// the parent's top-left, so a parent whose top-left is local (0,0) lines up.
unsafe fn place(rt: &dyn ScriptRuntime, m: &M, target: Object, r: LRect) {
    unsafe {
        set_v2(rt, m.rt_anc_min, target, 0.0, 1.0);
        set_v2(rt, m.rt_anc_max, target, 0.0, 1.0);
        set_v2(rt, m.rt_pivot, target, 0.0, 1.0);
        set_v2(rt, m.rt_anc_pos, target, r.x, -r.y);
        set_v2(rt, m.rt_size_delta, target, r.w, r.h);
    }
}

/// Stretch a child to fill its parent, with an even inset on every edge.
unsafe fn fill(rt: &dyn ScriptRuntime, m: &M, target: Object, inset: f32) {
    unsafe {
        set_v2(rt, m.rt_anc_min, target, 0.0, 0.0);
        set_v2(rt, m.rt_anc_max, target, 1.0, 1.0);
        set_v2(rt, m.rt_off_min, target, inset, inset);
        set_v2(rt, m.rt_off_max, target, -inset, -inset);
    }
}

unsafe fn make_text_go(
    rt: &dyn ScriptRuntime,
    domain: Domain,
    m: &M,
    parent_rt: Object,
    name: &str,
    text: &str,
    fs: i32,
    col: (f32, f32, f32),
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
        set_color(rt, m.gr_color, tx, col, 1.0);
        set_int(rt, m.tx_alignment, tx, align);
        set_int(rt, m.tx_horiz, tx, horiz);
        set_int(rt, m.tx_vert, tx, vert);
        set_bool(rt, m.gr_raycast, tx, false);
    }
    (go, tx, trt)
}

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

// ── Win98 3D bevel: four 1px edge strips on a panel ───────────────────────────
#[allow(clippy::too_many_arguments)]
unsafe fn strip(
    rt: &dyn ScriptRuntime,
    domain: Domain,
    m: &M,
    parent: Object,
    col: (f32, f32, f32),
    amin: (f32, f32),
    amax: (f32, f32),
    piv: (f32, f32),
    size: (f32, f32),
) {
    let (_, img, prt) = unsafe { make_panel(rt, domain, m, "bev", parent) };
    unsafe {
        set_color(rt, m.gr_color, img, col, 1.0);
        set_v2(rt, m.rt_anc_min, prt, amin.0, amin.1);
        set_v2(rt, m.rt_anc_max, prt, amax.0, amax.1);
        set_v2(rt, m.rt_pivot, prt, piv.0, piv.1);
        set_v2(rt, m.rt_anc_pos, prt, 0.0, 0.0);
        set_v2(rt, m.rt_size_delta, prt, size.0, size.1);
    }
}

/// Raised = light top/left, shadow bottom/right. Sunken = the inverse.
unsafe fn bevel(rt: &dyn ScriptRuntime, domain: Domain, m: &M, parent: Object, raised: bool) {
    let (tl, br) = if raised { (C_LIGHT, C_SHADOW) } else { (C_SHADOW, C_LIGHT) };
    unsafe {
        strip(rt, domain, m, parent, tl, (0.0, 1.0), (1.0, 1.0), (0.5, 1.0), (0.0, 1.0)); // top
        strip(rt, domain, m, parent, tl, (0.0, 0.0), (0.0, 1.0), (0.0, 0.5), (1.0, 0.0)); // left
        strip(rt, domain, m, parent, br, (0.0, 0.0), (1.0, 0.0), (0.5, 0.0), (0.0, 1.0)); // bottom
        strip(rt, domain, m, parent, br, (1.0, 0.0), (1.0, 1.0), (1.0, 0.5), (1.0, 0.0)); // right
    }
}

/// A raised gray button with a centred label. Returns the label Text object.
unsafe fn make_button(
    rt: &dyn ScriptRuntime,
    domain: Domain,
    m: &M,
    parent: Object,
    name: &str,
    label: &str,
    r: LRect,
) -> Object {
    let (_, img, prt) = unsafe { make_panel(rt, domain, m, name, parent) };
    unsafe {
        set_color(rt, m.gr_color, img, C_FACE, 1.0);
        place(rt, m, prt, r);
        bevel(rt, domain, m, prt, true);
    }
    let (_, txt, trt) = unsafe {
        make_text_go(
            rt, domain, m, prt, name, label, 11, C_DARK,
            unity::ANCHOR_MIDDLE_CENTER, unity::WRAP_OVERFLOW, unity::WRAP_TRUNCATE,
        )
    };
    unsafe { fill(rt, m, trt, 1.0) };
    txt
}

/// A Win98 checkbox: a sunken white box, a check glyph (empty until on), and a
/// label to the right. Returns the check-glyph Text object so on_frame can set
/// its text to "X" / "".
unsafe fn make_checkbox(
    rt: &dyn ScriptRuntime,
    domain: Domain,
    m: &M,
    parent: Object,
    name: &str,
    label: &str,
    r: LRect,
) -> Object {
    let (_, boximg, boxrt) = unsafe { make_panel(rt, domain, m, name, parent) };
    unsafe {
        set_color(rt, m.gr_color, boximg, C_FIELD, 1.0);
        place(rt, m, boxrt, lr(r.x, r.y + 2.0, 14.0, 14.0));
        bevel(rt, domain, m, boxrt, false);
    }
    // Inner fill square: a solid Image, hidden (alpha 0) until checked. An
    // alpha toggle on an Image is the same call the whole menu renders with, so
    // it is far more reliable than drawing an "X" glyph inside a 14px box.
    let (_, fill_img, fill_rt) = unsafe { make_panel(rt, domain, m, name, boxrt) };
    unsafe {
        set_color(rt, m.gr_color, fill_img, C_CHECK, 0.0);
        fill(rt, m, fill_rt, 3.0);
    }
    let (_, _l, lblrt) = unsafe {
        make_text_go(
            rt, domain, m, parent, name, label, 11, C_DARK,
            unity::ANCHOR_MIDDLE_LEFT, unity::WRAP_OVERFLOW, unity::WRAP_TRUNCATE,
        )
    };
    unsafe { place(rt, m, lblrt, lr(r.x + 20.0, r.y, r.w - 20.0, r.h)) };
    fill_img
}

/// A tab in the strip. Returns its background Image (recoloured active/inactive).
unsafe fn make_tab(rt: &dyn ScriptRuntime, domain: Domain, m: &M, parent: Object, i: usize) -> Object {
    let (_, img, prt) = unsafe { make_panel(rt, domain, m, "tab", parent) };
    unsafe {
        set_color(rt, m.gr_color, img, C_TAB_OFF, 1.0);
        place(rt, m, prt, tab_rect(i));
        bevel(rt, domain, m, prt, true);
    }
    let (_, _t, trt) = unsafe {
        make_text_go(
            rt, domain, m, prt, "tablbl", TAB_NAMES[i], 11, C_DARK,
            unity::ANCHOR_MIDDLE_CENTER, unity::WRAP_OVERFLOW, unity::WRAP_TRUNCATE,
        )
    };
    unsafe { fill(rt, m, trt, 1.0) };
    img
}

// ── method / type resolution ──────────────────────────────────────────────────
unsafe fn resolve_font(rt: &dyn ScriptRuntime, domain: Domain) -> Option<Object> {
    let res_cls = unsafe { symbols::find_class(rt, domain, unity::RESOURCES) }?;
    let get_builtin = unsafe { symbols::find_method(rt, res_cls, unity::GET_BUILTIN_RESOURCE, 2) }?;
    let font_type = unsafe { symbols::find_type_object(rt, domain, unity::FONT) }?;
    for name in unity::BUILTIN_FONTS {
        let Some(name_str) = (unsafe { rt.new_string(domain, name) }) else { continue };
        let mut args = [font_type.raw(), name_str.raw()];
        match unsafe { rt.invoke_static(get_builtin, &mut args) } {
            Some(f) if !f.is_null() => {
                crate::elog!("[menu] font: {}", name);
                return Some(f);
            }
            _ => {}
        }
    }
    None
}

unsafe fn resolve_methods(rt: &dyn ScriptRuntime, domain: Domain) -> Option<M> {
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

    let ty_canvas = unsafe { symbols::find_type_object(rt, domain, unity::CANVAS) }.unwrap_or(Object::NULL);
    let ty_image = unsafe { symbols::find_type_object(rt, domain, unity::IMAGE) }.unwrap_or(Object::NULL);
    let ty_text = unsafe { symbols::find_type_object(rt, domain, unity::TEXT) }.unwrap_or(Object::NULL);
    let ty_rect_tf = unsafe { symbols::find_type_object(rt, domain, unity::RECT_TRANSFORM) }.unwrap_or(Object::NULL);
    let font = unsafe { resolve_font(rt, domain) }.unwrap_or(Object::NULL);
    if font.is_null() {
        crate::elog!("[menu] NO FONT -- labels will be invisible");
    }
    let cur_set_visible = unsafe { symbols::find_class(rt, domain, unity::CURSOR) }
        .and_then(|cc| unsafe { rt.method_exact(cc, unity::CURSOR_SET_VISIBLE, 1) })
        .unwrap_or(Method::NULL);

    let mut missing = 0u32;
    macro_rules! mex {
        ($cls:expr, $name:expr, $n:expr) => {{
            match unsafe { rt.method_exact($cls, $name, $n) } {
                Some(m) => m,
                None => { crate::elog!("[menu] MISSING method: {}", $name); missing += 1; Method::NULL }
            }
        }};
    }
    macro_rules! mwalk {
        ($cls:expr, $name:expr, $n:expr) => {{
            match unsafe { rt.method($cls, $name, $n) } {
                Some(m) => m,
                None => { crate::elog!("[menu] MISSING method (walk): {}", $name); missing += 1; Method::NULL }
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
        cur_set_visible,
        ty_canvas,
        ty_image,
        ty_text,
        ty_rect_tf,
        font,
    };

    if ty_canvas.is_null() || ty_image.is_null() || ty_text.is_null() || ty_rect_tf.is_null() {
        crate::elog!("[menu] a required Type object is null -- aborting build");
        return None;
    }
    if resolved.go_ctor_name.is_null() || resolved.go_add_comp.is_null()
        || resolved.go_get_comp.is_null() || resolved.tf_set_parent.is_null()
    {
        crate::elog!("[menu] core GameObject/Transform methods missing -- aborting build");
        return None;
    }
    crate::elog!("[menu] methods resolved ({} missing, non-fatal)", missing);
    Some(resolved)
}

// ── UI tree construction ──────────────────────────────────────────────────────
unsafe fn build_ui(rt: &dyn ScriptRuntime, domain: Domain, m: &M) -> Option<Handles> {
    // Canvas (root, full screen overlay).
    let root_go = unsafe { new_go(rt, domain, m, "eftMenu_canvas") }?;
    let canvas = unsafe { add_comp(rt, root_go, m, m.ty_canvas) };
    unsafe {
        set_int(rt, m.cv_render_mode, canvas, unity::RENDER_MODE_SCREEN_SPACE_OVERLAY);
        set_int(rt, m.cv_sort_order, canvas, 999);
        invoke_static(rt, m.go_dont_destroy, &mut [root_go.raw()]);
    }
    let canvas_rt = unsafe { get_comp(rt, root_go, m, m.ty_rect_tf) };
    unsafe {
        set_v2(rt, m.rt_anc_min, canvas_rt, 0.0, 0.0);
        set_v2(rt, m.rt_anc_max, canvas_rt, 1.0, 1.0);
        set_v2(rt, m.rt_anc_pos, canvas_rt, 0.0, 0.0);
        set_v2(rt, m.rt_size_delta, canvas_rt, 0.0, 0.0);
    }

    // Window (raised gray dialog).
    let (_, win_img, window_rt) = unsafe { make_panel(rt, domain, m, "eftMenu_window", canvas_rt) };
    unsafe {
        set_color(rt, m.gr_color, win_img, C_FACE, 1.0);
        set_v2(rt, m.rt_anc_min, window_rt, MENU_ANCHOR_X, MENU_ANCHOR_Y);
        set_v2(rt, m.rt_anc_max, window_rt, MENU_ANCHOR_X, MENU_ANCHOR_Y);
        set_v2(rt, m.rt_pivot, window_rt, 0.5, 0.5);
        set_v2(rt, m.rt_anc_pos, window_rt, 0.0, 0.0);
        set_v2(rt, m.rt_size_delta, window_rt, WIN_W, WIN_H);
        bevel(rt, domain, m, window_rt, true);
    }

    // Title bar (navy) + title text + fps + close.
    let (_, tb_img, title_rt) = unsafe { make_panel(rt, domain, m, "eftMenu_title", window_rt) };
    unsafe {
        set_color(rt, m.gr_color, tb_img, C_NAVY, 1.0);
        place(rt, m, title_rt, lr(2.0, 2.0, WIN_W - 4.0, TITLE_H - 2.0));
    }
    let (_, _tt, ttrt) = unsafe {
        make_text_go(rt, domain, m, window_rt, "titletxt", "eftTrainer", 11, C_LIGHT,
            unity::ANCHOR_MIDDLE_LEFT, unity::WRAP_OVERFLOW, unity::WRAP_TRUNCATE)
    };
    unsafe { place(rt, m, ttrt, lr(7.0, 2.0, WIN_W - CLOSE_SZ - FPS_W - 18.0, TITLE_H - 2.0)) };
    let (_, fps_text, fpsrt) = unsafe {
        make_text_go(rt, domain, m, window_rt, "fps", "-- fps", 11, C_LIGHT,
            unity::ANCHOR_MIDDLE_RIGHT, unity::WRAP_OVERFLOW, unity::WRAP_TRUNCATE)
    };
    unsafe { place(rt, m, fpsrt, lr(WIN_W - CLOSE_SZ - FPS_W - 6.0, 2.0, FPS_W, TITLE_H - 2.0)) };
    let _ = unsafe { make_button(rt, domain, m, window_rt, "close", "x", CLOSE_R) };

    // Tabs.
    let mut tab_img = [Object::NULL; N_TABS];
    for i in 0..N_TABS {
        tab_img[i] = unsafe { make_tab(rt, domain, m, window_rt, i) };
    }

    // Content panels (one per tab, all filling the content rect; active shown).
    let mut tab_content = [Object::NULL; N_TABS];
    let mut content_rt = [Object::NULL; N_TABS];
    for i in 0..N_TABS {
        let (go, img, prt) = unsafe { make_panel(rt, domain, m, "content", window_rt) };
        unsafe {
            set_color(rt, m.gr_color, img, C_FACE, 1.0);
            place(rt, m, prt, lr(CONTENT_X, CONTENT_Y, CONTENT_W, CONTENT_H));
            bevel(rt, domain, m, prt, true);
        }
        tab_content[i] = go;
        content_rt[i] = prt;
    }

    // Weapon tab (blank placeholder).
    let (_, _w, wrt) = unsafe {
        make_text_go(rt, domain, m, content_rt[0], "weap", "No weapon features yet.", 12, C_SHADOW,
            unity::ANCHOR_MIDDLE_CENTER, unity::WRAP_OVERFLOW, unity::WRAP_TRUNCATE)
    };
    unsafe { fill(rt, m, wrt, 0.0) };

    // Visuals tab.
    let chams_check = unsafe { make_checkbox(rt, domain, m, content_rt[1], "chams", "Chams", V_CHAMS) };
    let style_txt = unsafe { make_button(rt, domain, m, content_rt[1], "style", "Style: flat", V_STYLE) };
    let scheme_txt = unsafe { make_button(rt, domain, m, content_rt[1], "scheme", "Scheme: plasma", V_SCHEME) };

    // Misc tab.
    let stam_check = unsafe { make_checkbox(rt, domain, m, content_rt[2], "stam", "Infinite Stamina", M_STAM) };
    let speed_check = unsafe { make_checkbox(rt, domain, m, content_rt[2], "speed", "Speedhack", M_SPEED) };
    let _ = unsafe { make_button(rt, domain, m, content_rt[2], "spdminus", "-", M_SPEED_MINUS) };
    let (_, speed_txt, sprt) = unsafe {
        make_text_go(rt, domain, m, content_rt[2], "spdval", "x1.00", 11, C_DARK,
            unity::ANCHOR_MIDDLE_CENTER, unity::WRAP_OVERFLOW, unity::WRAP_TRUNCATE)
    };
    unsafe { place(rt, m, sprt, M_SPEED_VAL) };
    let _ = unsafe { make_button(rt, domain, m, content_rt[2], "spdplus", "+", M_SPEED_PLUS) };
    let _ = unsafe { make_button(rt, domain, m, content_rt[2], "config", "Config Template", M_CONFIG) };

    // Console tab (sunken dark field + green text).
    let (_, cf_img, cf_rt) = unsafe { make_panel(rt, domain, m, "confield", content_rt[3]) };
    unsafe {
        set_color(rt, m.gr_color, cf_img, (0.09, 0.09, 0.09), 1.0);
        fill(rt, m, cf_rt, 6.0);
        bevel(rt, domain, m, cf_rt, false);
    }
    let (_, console_text, ct_rt) = unsafe {
        make_text_go(rt, domain, m, cf_rt, "contxt", "", 10, (0.0, 0.87, 0.0),
            unity::ANCHOR_UPPER_LEFT, unity::WRAP_OVERFLOW, unity::WRAP_OVERFLOW)
    };
    unsafe { fill(rt, m, ct_rt, 3.0) };

    // Status strip at the window bottom.
    let (_, status_text, strt) = unsafe {
        make_text_go(rt, domain, m, window_rt, "status", "", 11, (0.9, 0.9, 0.35),
            unity::ANCHOR_MIDDLE_LEFT, unity::WRAP_OVERFLOW, unity::WRAP_TRUNCATE)
    };
    unsafe { place(rt, m, strt, lr(CONTENT_X + 5.0, STATUS_Y, WIN_W - CONTENT_X * 2.0 - 10.0, STATUS_H)) };

    // Cursor gets its OWN canvas: moving it every frame must never dirty the
    // heavy menu canvas (that full-canvas rebuild was the performance hit). It
    // is positioned by anchoredPosition (a cheap transform move) against a
    // fixed bottom-left anchor, not by changing anchors (a layout rebuild).
    let cursor_canvas = unsafe { new_go(rt, domain, m, "eftMenu_cursorcv") }?;
    let cur_cv = unsafe { add_comp(rt, cursor_canvas, m, m.ty_canvas) };
    unsafe {
        set_int(rt, m.cv_render_mode, cur_cv, unity::RENDER_MODE_SCREEN_SPACE_OVERLAY);
        set_int(rt, m.cv_sort_order, cur_cv, 1000);
        invoke_static(rt, m.go_dont_destroy, &mut [cursor_canvas.raw()]);
    }
    let cur_cv_rt = unsafe { get_comp(rt, cursor_canvas, m, m.ty_rect_tf) };
    unsafe {
        set_v2(rt, m.rt_anc_min, cur_cv_rt, 0.0, 0.0);
        set_v2(rt, m.rt_anc_max, cur_cv_rt, 1.0, 1.0);
        set_v2(rt, m.rt_anc_pos, cur_cv_rt, 0.0, 0.0);
        set_v2(rt, m.rt_size_delta, cur_cv_rt, 0.0, 0.0);
    }
    let (_, cur_bg, cursor_rt) = unsafe { make_panel(rt, domain, m, "cursor", cur_cv_rt) };
    unsafe {
        set_color(rt, m.gr_color, cur_bg, (0.0, 0.0, 0.0), 0.0);
        set_v2(rt, m.rt_anc_min, cursor_rt, 0.0, 0.0);
        set_v2(rt, m.rt_anc_max, cursor_rt, 0.0, 0.0);
        set_v2(rt, m.rt_pivot, cursor_rt, 0.5, 0.5);
        set_v2(rt, m.rt_anc_pos, cursor_rt, 0.0, 0.0);
        set_v2(rt, m.rt_size_delta, cursor_rt, 0.0, 0.0);
    }
    for (nm, w, h) in [("cur_h", 18.0f32, 2.0f32), ("cur_v", 2.0, 18.0)] {
        let (_, bar, brt) = unsafe { make_panel(rt, domain, m, nm, cursor_rt) };
        unsafe {
            set_color(rt, m.gr_color, bar, (0.10, 1.0, 0.90), 1.0);
            set_v2(rt, m.rt_anc_min, brt, 0.5, 0.5);
            set_v2(rt, m.rt_anc_max, brt, 0.5, 0.5);
            set_v2(rt, m.rt_pivot, brt, 0.5, 0.5);
            set_v2(rt, m.rt_anc_pos, brt, 0.0, 0.0);
            set_v2(rt, m.rt_size_delta, brt, w, h);
        }
    }

    // Initial tab visibility.
    let active = ACTIVE_TAB.load(Ordering::Relaxed).min(N_TABS - 1);
    for i in 0..N_TABS {
        unsafe {
            set_bool(rt, m.go_set_active, tab_content[i], i == active);
            let c = if i == active { C_FACE } else { C_TAB_OFF };
            set_color(rt, m.gr_color, tab_img[i], c, 1.0);
        }
    }

    crate::elog!("[menu] UI tree constructed");
    Some(Handles {
        root_go, window_rt, title_rt, tab_img, tab_content,
        console_text, fps_text, status_text,
        chams_check, style_txt, scheme_txt,
        stam_check, speed_check, speed_txt,
        cursor_canvas, cursor_rt,
    })
}

unsafe fn set_text(rt: &dyn ScriptRuntime, domain: Domain, m: &M, obj: Object, s: &str) {
    if let Some(str_obj) = unsafe { rt.new_string(domain, s) } {
        let mut args = [str_obj.raw()];
        unsafe { invoke(rt, m.tx_text, obj, &mut args) };
    }
}

/// Show/hide a checkbox's fill square by toggling its Image alpha.
unsafe fn set_check(rt: &dyn ScriptRuntime, m: &M, fill_img: Object, on: bool) {
    unsafe { set_color(rt, m.gr_color, fill_img, C_CHECK, if on { 1.0 } else { 0.0 }) };
}

/// Destroys the overlay. Main thread only, just before on_frame returns false.
unsafe fn teardown(rt: &dyn ScriptRuntime) {
    crate::input::uninstall();
    crate::crash::phase(crate::crash::CHAMS_RESTORE);
    unsafe { crate::chams::on_unload() };
    crate::crash::phase(crate::crash::MENU_TEARDOWN);
    let Some(ms) = (unsafe { (*(&raw const MENU_STATE)).as_ref() }) else { return };
    if ms.m.obj_destroy.is_null() {
        return;
    }
    if !ms.h.cursor_canvas.is_null() {
        let mut a = [ms.h.cursor_canvas.raw()];
        unsafe { invoke_static(rt, ms.m.obj_destroy, &mut a) };
    }
    if !ms.h.root_go.is_null() {
        let mut args = [ms.h.root_go.raw()];
        unsafe { invoke_static(rt, ms.m.obj_destroy, &mut args) };
    }
    crate::elog!("[menu] overlay destroyed");
}

// ── per-frame driver entry ────────────────────────────────────────────────────
pub unsafe fn on_frame() -> bool {
    static IN_FRAME: AtomicBool = AtomicBool::new(false);
    if IN_FRAME.swap(true, Ordering::Acquire) {
        return true;
    }
    struct FrameGuard {
        start: Instant,
    }
    impl Drop for FrameGuard {
        fn drop(&mut self) {
            let us = self.start.elapsed().as_micros() as u64;
            ONFRAME_LAST_US.store(us, Ordering::Relaxed);
            ONFRAME_MAX_US.fetch_max(us, Ordering::Relaxed);
            ONFRAME_TOTAL_US.fetch_add(us, Ordering::Relaxed);
            IN_FRAME.store(false, Ordering::Release);
        }
    }
    let _guard = FrameGuard { start: Instant::now() };

    tick_fps();
    FRAME_COUNT.fetch_add(1, Ordering::Relaxed);

    let Some(rt) = runtime::get() else { return true };
    let Some(&domain) = DOMAIN.get() else { return true };

    // INSERT toggles the menu; seed the pointer to centre on open.
    static INSERT_PREV: AtomicBool = AtomicBool::new(false);
    let insert_now = unsafe { (GetAsyncKeyState(VK_INSERT.0 as i32) as u16 & 0x8000) != 0 };
    // Swap UNCONDITIONALLY. Folding it into `insert_now && !PREV.swap(..)`
    // short-circuits on key release, so PREV latches true and the toggle fires
    // exactly once for the whole session. This was the reopen regression.
    let insert_prev = INSERT_PREV.swap(insert_now, Ordering::Relaxed);
    let insert_edge = insert_now && !insert_prev;
    if insert_edge {
        let now_visible = !VISIBLE.fetch_xor(true, Ordering::Relaxed);
        crate::elog!("[menu] INSERT -> visible={}", now_visible);
        // SetActive + cursor seed are applied by the reconcile below, so the
        // overlay always follows VISIBLE however it changed (INSERT, HTTP), and
        // a missed key edge self-heals on the next frame.
    }

    // One click read per frame (shared with the close test below).
    let clicked = lmb_clicked();
    let visible = VISIBLE.load(Ordering::Relaxed);
    let (sw, sh) = screen_wh();
    let (cx, cy) = crate::input::virtual_cursor();
    let close_clicked = clicked && visible && hit(CLOSE_R, sw, sh, cx, cy);

    if clicked && visible {
        crate::elog!(
            "[menu] click ({},{}) close={} tab={} origin=({:.0},{:.0})",
            cx, cy, close_clicked, ACTIVE_TAB.load(Ordering::Relaxed),
            win_origin(sw, sh).0, win_origin(sw, sh).1
        );
    }

    if clicked && visible && !close_clicked {
        // Tabs first.
        let mut handled = false;
        for i in 0..N_TABS {
            if hit(tab_rect(i), sw, sh, cx, cy) {
                ACTIVE_TAB.store(i, Ordering::Relaxed);
                handled = true;
                break;
            }
        }
        // Then the active tab's controls.
        if !handled {
            match ACTIVE_TAB.load(Ordering::Relaxed) {
                1 => {
                    if hit_content(V_CHAMS, sw, sh, cx, cy) {
                        crate::chams::toggle();
                    } else if hit_content(V_STYLE, sw, sh, cx, cy) {
                        crate::chams::cycle_style();
                    } else if hit_content(V_SCHEME, sw, sh, cx, cy) {
                        crate::chams::cycle_scheme();
                    }
                }
                2 => {
                    if hit_content(M_STAM, sw, sh, cx, cy) {
                        crate::features::toggle_inf_stamina();
                    } else if hit_content(M_SPEED, sw, sh, cx, cy) {
                        crate::features::toggle_speed();
                    } else if hit_content(M_SPEED_MINUS, sw, sh, cx, cy) {
                        crate::features::bump_speed(false);
                    } else if hit_content(M_SPEED_PLUS, sw, sh, cx, cy) {
                        crate::features::bump_speed(true);
                    } else if hit_content(M_CONFIG, sw, sh, cx, cy) {
                        crate::elog!("[menu] config template: not built yet");
                    }
                }
                _ => {}
            }
        }
    }

    crate::input::ensure_installed();

    // Main-thread features share the driver.
    unsafe { crate::chams::tick(rt, domain) };
    unsafe { crate::features::tick(rt, domain) };
    unsafe { crate::world::tick(rt, domain) };

    // END (worker) unloads the trainer for real. The X button only HIDES the
    // menu -- tearing the trainer down on X is why the menu could never be
    // reopened once "closed". Checked every frame so both stay responsive.
    if UNLOAD_PENDING.load(Ordering::Relaxed) {
        crate::elog!("[menu] unload (END key)");
        unsafe { teardown(rt) };
        return false;
    }
    if close_clicked {
        VISIBLE.store(false, Ordering::Relaxed);
        if let Some(ms) = unsafe { (*(&raw const MENU_STATE)).as_ref() } {
            unsafe {
                set_bool(rt, ms.m.go_set_active, ms.h.root_go, false);
                set_bool(rt, ms.m.go_set_active, ms.h.cursor_canvas, false);
            }
        }
        crate::elog!("[menu] hidden (X button) -- INSERT to reopen");
        return true;
    }

    // Lazy init.
    if !INITIALIZED.load(Ordering::Acquire) {
        crate::elog!("[menu] building UI ({} backend)...", rt.backend().name());
        let Some(m) = (unsafe { resolve_methods(rt, domain) }) else {
            crate::elog!("[menu] resolve failed -- menu disabled");
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
                m, h,
                last_text: String::new(),
                last_fps: u32::MAX,
                last_status: String::new(),
                last_tab: usize::MAX,
                last_chams_on: None,
                last_style: String::new(),
                last_scheme: String::new(),
                last_stam: None,
                last_speed_on: None,
                last_speed_mult: -1.0,
                last_cur: (i32::MIN, i32::MIN),
            })
        };
        INITIALIZED.store(true, Ordering::Release);
        let (cx, cy) = window_center();
        crate::input::seed_cursor(cx, cy);
        crate::elog!("[menu] ready");
    }

    let Some(ms) = (unsafe { (*(&raw mut MENU_STATE)).as_mut() }) else { return true };

    // Reconcile the overlay's active state to VISIBLE. Declarative, not tied to
    // the INSERT edge, so the menu always matches the flag however it changed
    // (INSERT key, HTTP /menu) and a dropped edge self-heals. Only invokes on an
    // actual change, so steady state is free.
    {
        static LAST_APPLIED: AtomicI32 = AtomicI32::new(-1);
        let want = if visible { 1 } else { 0 };
        if LAST_APPLIED.swap(want, Ordering::Relaxed) != want {
            unsafe {
                set_bool(rt, ms.m.go_set_active, ms.h.root_go, visible);
                set_bool(rt, ms.m.go_set_active, ms.h.cursor_canvas, visible);
            }
            if visible {
                let (scx, scy) = window_center();
                crate::input::seed_cursor(scx, scy);
            }
            crate::elog!("[menu] applied visible={}", visible);
        }
    }

    // Menu hidden: none of the work below (cursor, canvas text, labels) is
    // visible, so bail before spending a single invoke or GC allocation on it.
    // The gameplay ticks (chams/features/world) and unload already ran above,
    // and stay live with the menu closed -- this only skips UI upkeep, which is
    // the bulk of what on_frame was doing during normal play.
    if !visible {
        return true;
    }

    // Pointer draw (cheap: own canvas, anchoredPosition). Skip the invoke when
    // the pointer has not moved since last frame -- the overlay canvas y is up,
    // so flip the Win32 y.
    if (cx, cy) != ms.last_cur {
        unsafe { set_v2(rt, ms.m.rt_anc_pos, ms.h.cursor_rt, cx as f32, sh - cy as f32) };
        ms.last_cur = (cx, cy);
    }
    if !ms.m.cur_set_visible.is_null() {
        // Re-assert every frame: the game re-shows the OS cursor on its own, and
        // this is one invoke only while the menu is actually open.
        let mut b: i32 = 0;
        let mut args = [&mut b as *mut i32 as *mut c_void];
        unsafe { invoke_static(rt, ms.m.cur_set_visible, &mut args) };
    }

    // Everything below writes menu-canvas text/state, which rebuilds that
    // canvas, so throttle it. Input, ticks and unload above run every frame.
    if !throttle_ok() {
        return true;
    }

    // Tab switch.
    let active = ACTIVE_TAB.load(Ordering::Relaxed).min(N_TABS - 1);
    if active != ms.last_tab {
        for i in 0..N_TABS {
            unsafe {
                set_bool(rt, ms.m.go_set_active, ms.h.tab_content[i], i == active);
                let c = if i == active { C_FACE } else { C_TAB_OFF };
                set_color(rt, ms.m.gr_color, ms.h.tab_img[i], c, 1.0);
            }
        }
        ms.last_tab = active;
    }

    // FPS + status.
    let fps = FPS_VALUE.load(Ordering::Relaxed);
    if fps != ms.last_fps {
        unsafe { set_text(rt, domain, &ms.m, ms.h.fps_text, &format!("{} fps", fps)) };
        ms.last_fps = fps;
    }
    if status_dirty() {
        let status = status_snapshot();
        if status != ms.last_status {
            unsafe { set_text(rt, domain, &ms.m, ms.h.status_text, &status) };
            ms.last_status = status;
        }
    }

    // Visuals labels.
    let chams_on = crate::chams::is_on();
    if ms.last_chams_on != Some(chams_on) {
        unsafe { set_check(rt, &ms.m, ms.h.chams_check, chams_on) };
        ms.last_chams_on = Some(chams_on);
    }
    let style = crate::chams::style_name();
    if ms.last_style != style {
        unsafe { set_text(rt, domain, &ms.m, ms.h.style_txt, &format!("Style: {}", style)) };
        ms.last_style = style.to_string();
    }
    let scheme = crate::chams::scheme_name();
    if ms.last_scheme != scheme {
        unsafe { set_text(rt, domain, &ms.m, ms.h.scheme_txt, &format!("Scheme: {}", scheme)) };
        ms.last_scheme = scheme.to_string();
    }

    // Misc labels.
    let stam = crate::features::inf_stamina();
    if ms.last_stam != Some(stam) {
        unsafe { set_check(rt, &ms.m, ms.h.stam_check, stam) };
        ms.last_stam = Some(stam);
    }
    let speed_on = crate::features::speed_on();
    if ms.last_speed_on != Some(speed_on) {
        unsafe { set_check(rt, &ms.m, ms.h.speed_check, speed_on) };
        ms.last_speed_on = Some(speed_on);
    }
    let sm = crate::features::speed_mult();
    if ms.last_speed_mult != sm {
        unsafe { set_text(rt, domain, &ms.m, ms.h.speed_txt, &format!("x{:.2}", sm)) };
        ms.last_speed_mult = sm;
    }

    // Console mirror -- only rebuild the 22-string snapshot when a line landed.
    if crate::console::take_dirty() {
        if let Some(snap) = crate::console::snapshot_tail(CONSOLE_LINES) {
            if snap != ms.last_text {
                unsafe { set_text(rt, domain, &ms.m, ms.h.console_text, &snap) };
                ms.last_text = snap;
            }
        }
    }

    true
}
