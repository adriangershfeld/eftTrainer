//! Input capture. While the menu is open, the game window's input messages are
//! swallowed so a click on a menu button (or a keypress) does not also reach
//! the game. The menu reads key/cursor state by polling (GetAsyncKeyState /
//! GetCursorPos), which is unaffected, so buttons still work; the game, which
//! processes window messages, goes deaf until the menu closes.
//!
//! The hook is a WndProc subclass on the game's top-level window, installed on
//! the main thread (same thread Unity pumps messages on) and restored on
//! unload. When the menu is closed every message passes straight through, so
//! the footprint is nil unless the menu is up.

#![allow(dead_code)]

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, TRUE, WPARAM};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIDEV_INPUTSINK,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, EnumWindows, GetSystemMetrics, GetWindowRect,
    GetWindowThreadProcessId, IsWindowVisible, SetWindowLongPtrW, GWLP_WNDPROC, SM_CXSCREEN,
    SM_CYSCREEN, WNDPROC,
};

// Win32 message ids, spelled out to avoid any import-path drift across crate
// versions. Only input messages are swallowed.
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_CHAR: u32 = 0x0102;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_LBUTTONDBLCLK: u32 = 0x0203;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_RBUTTONDBLCLK: u32 = 0x0206;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MBUTTONDBLCLK: u32 = 0x0209;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_XBUTTONUP: u32 = 0x020C;
const WM_MOUSEHWHEEL: u32 = 0x020E;
const WM_INPUT: u32 = 0x00FF;

static INSTALLED: AtomicBool = AtomicBool::new(false);
static HOOKED_HWND: AtomicIsize = AtomicIsize::new(0);
static ORIGINAL_WNDPROC: AtomicIsize = AtomicIsize::new(0);

// Virtual cursor in Win32 screen coords (y down from top), driven from raw
// mouse deltas while the menu is open. The game locks/centres the hardware
// cursor for mouselook, so GetCursorPos is unreliable there; this is not. The
// menu draws its own pointer here and hit-tests against it.
static VCUR_X: AtomicI32 = AtomicI32::new(0);
static VCUR_Y: AtomicI32 = AtomicI32::new(0);

/// Seed the virtual cursor, e.g. to screen centre when the menu opens.
pub fn seed_cursor(x: i32, y: i32) {
    VCUR_X.store(x, Ordering::Relaxed);
    VCUR_Y.store(y, Ordering::Relaxed);
}

/// Current virtual cursor, Win32 screen coords. What the menu draws and hits.
pub fn virtual_cursor() -> (i32, i32) {
    (VCUR_X.load(Ordering::Relaxed), VCUR_Y.load(Ordering::Relaxed))
}

// Screen size, cached: it does not change within a session, and this is read
// per raw mouse event, so we do not want a GetSystemMetrics pair each time.
static SCR_W: AtomicI32 = AtomicI32::new(0);
static SCR_H: AtomicI32 = AtomicI32::new(0);
fn screen_cached() -> (i32, i32) {
    let w = SCR_W.load(Ordering::Relaxed);
    if w != 0 {
        return (w, SCR_H.load(Ordering::Relaxed));
    }
    let w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1);
    let h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1);
    SCR_W.store(w, Ordering::Relaxed);
    SCR_H.store(h, Ordering::Relaxed);
    (w, h)
}

/// Accumulate a raw mouse move into the virtual cursor, clamped to the screen.
/// Called from the WndProc on WM_INPUT while capturing.
/// Returns true if this WM_INPUT was a MOUSE event (accumulated into the
/// virtual cursor, and the caller should swallow it). Returns false for a
/// keyboard/HID raw event or an unreadable record, so the caller passes it to
/// the game -- keeping the game's key state in sync (a swallowed key release is
/// what left sprint/jump stuck).
unsafe fn accumulate_raw(lparam: LPARAM) -> bool {
    let hraw = HRAWINPUT(lparam.0 as *mut c_void);
    let hdr = core::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut buf = [0u8; core::mem::size_of::<RAWINPUT>()];
    let mut size = buf.len() as u32;
    let got = unsafe {
        GetRawInputData(hraw, RID_INPUT, Some(buf.as_mut_ptr() as *mut c_void), &mut size, hdr)
    };
    if got == 0 || got == u32::MAX {
        return false; // couldn't read it -- let the game keep the event
    }
    let raw = unsafe { &*(buf.as_ptr() as *const RAWINPUT) };
    if raw.header.dwType != 0 {
        return false; // keyboard / HID (RIM_TYPEMOUSE == 0) -- pass to the game
    }
    static RAW_SEEN: AtomicBool = AtomicBool::new(false);
    if !RAW_SEEN.swap(true, Ordering::Relaxed) {
        crate::elog!("[input] first raw mouse delta received -- cursor is live");
    }
    let m = unsafe { &raw.data.mouse };
    // MOUSE_MOVE_ABSOLUTE == 1: skip absolute reports (tablets/RDP); we want
    // relative deltas, which is what a mouse sends. Still a mouse event: swallow.
    if (m.usFlags.0 & 1) != 0 {
        return true;
    }
    let (dx, dy) = (m.lLastX, m.lLastY);
    if dx == 0 && dy == 0 {
        return true; // mouse event, no delta -- nothing to move, but swallow it
    }
    let (sw, sh) = screen_cached();
    let nx = (VCUR_X.load(Ordering::Relaxed) + dx).clamp(0, sw - 1);
    let ny = (VCUR_Y.load(Ordering::Relaxed) + dy).clamp(0, sh - 1);
    VCUR_X.store(nx, Ordering::Relaxed);
    VCUR_Y.store(ny, Ordering::Relaxed);
    true
}

/// True while input should be swallowed: the menu is open.
#[inline]
fn capturing() -> bool {
    crate::menu::VISIBLE.load(Ordering::Relaxed)
}

// Only MOUSE messages are swallowed while the menu is open: that stops a menu
// click from also firing in the game. KEYBOARD is deliberately NOT swallowed --
// the menu is mouse-driven, and eating a key's release while capturing left the
// game thinking sprint/jump was held. WM_INPUT is handled separately (mouse raw
// is swallowed, keyboard raw is passed through), so it is not listed here.
#[inline]
fn is_mouse_msg(msg: u32) -> bool {
    matches!(
        msg,
        WM_MOUSEMOVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_LBUTTONDBLCLK
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_RBUTTONDBLCLK
            | WM_MBUTTONDOWN
            | WM_MBUTTONUP
            | WM_MBUTTONDBLCLK
            | WM_XBUTTONDOWN
            | WM_XBUTTONUP
            | WM_MOUSEWHEEL
            | WM_MOUSEHWHEEL
    )
}

/// The subclass. While the menu is open, MOUSE input is consumed (so a menu
/// click does not also fire in the game) and the raw mouse delta drives the
/// cursor. KEYBOARD passes straight through to the game, so movement keys and
/// their releases stay in sync (swallowing a key release is what left sprint
/// and jump stuck). When the menu is closed, everything passes through.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if capturing() {
        if msg == WM_INPUT {
            // Mouse raw -> accumulate + swallow. Keyboard/HID raw -> pass to the
            // game (accumulate_raw returns false), so key state stays in sync.
            if unsafe { accumulate_raw(lparam) } {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
        } else if is_mouse_msg(msg) {
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        // keyboard (and anything else) falls through to the game below
    }
    let orig = ORIGINAL_WNDPROC.load(Ordering::Relaxed);
    if orig != 0 {
        let f: WNDPROC = Some(unsafe {
            core::mem::transmute::<
                isize,
                unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
            >(orig)
        });
        return unsafe { CallWindowProcW(f, hwnd, msg, wparam, lparam) };
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// Largest visible top-level window owned by this process: the game window.
#[repr(C)]
struct Finder {
    best: isize,
    area: i64,
    pid: u32,
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let f = unsafe { &mut *(lparam.0 as *mut Finder) };
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32)) };
    if pid == f.pid && unsafe { IsWindowVisible(hwnd) }.as_bool() {
        let mut r = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut r) }.is_ok() {
            let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
            if area > f.area {
                f.area = area;
                f.best = hwnd.0 as isize;
            }
        }
    }
    TRUE
}

fn find_game_window() -> Option<HWND> {
    let mut f = Finder {
        best: 0,
        area: 0,
        pid: unsafe { GetCurrentProcessId() },
    };
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut f as *mut Finder as isize));
    }
    if f.best == 0 {
        None
    } else {
        Some(HWND(f.best as *mut c_void))
    }
}

/// Install the WndProc hook once. Idempotent and cheap after the first success.
/// Main thread only (called from menu::on_frame), which is the window's thread,
/// so the subclass is legal and no cross-thread dispatch can race the store.
pub fn ensure_installed() {
    if INSTALLED.load(Ordering::Relaxed) {
        return;
    }
    let Some(hwnd) = find_game_window() else {
        return; // window not up yet; try again next frame
    };
    let prev = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc as *const () as isize) };
    if prev == 0 {
        return; // failed; leave uninstalled so it retries
    }
    ORIGINAL_WNDPROC.store(prev, Ordering::SeqCst);
    HOOKED_HWND.store(hwnd.0 as isize, Ordering::SeqCst);

    // Register raw mouse input targeting OUR window, so WM_INPUT (relative mouse
    // deltas) is actually delivered to the subclass. Without this the deltas
    // never arrive -- the game's own raw-input registration does not target our
    // WndProc -- so the virtual cursor stays frozen at its seed and every click
    // lands on the same dead spot. INPUTSINK = receive even when backgrounded.
    // When the menu is closed we pass WM_INPUT straight through to the game, so
    // its mouselook is unaffected.
    let rid = RAWINPUTDEVICE {
        usUsagePage: 0x01, // generic desktop
        usUsage: 0x02,     // mouse
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    match unsafe {
        RegisterRawInputDevices(&[rid], core::mem::size_of::<RAWINPUTDEVICE>() as u32)
    } {
        Ok(()) => crate::elog!("[input] raw mouse registered (INPUTSINK)"),
        Err(e) => crate::elog!("[input] raw mouse register FAILED: {e:?} -- cursor may not move"),
    }

    INSTALLED.store(true, Ordering::SeqCst);
    crate::elog!("[input] menu input-capture armed (hwnd={:#x})", hwnd.0 as usize);
}

/// Restore the game's original WndProc. MUST run before FreeLibrary or the game
/// will call freed code on the next message. Main thread only (unload teardown,
/// same thread as the pump, so we are never inside the subclass when we swap).
pub fn uninstall() {
    if !INSTALLED.swap(false, Ordering::SeqCst) {
        return;
    }
    let hwnd_raw = HOOKED_HWND.swap(0, Ordering::SeqCst);
    let orig = ORIGINAL_WNDPROC.swap(0, Ordering::SeqCst);
    if hwnd_raw != 0 && orig != 0 {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, orig) };
        crate::elog!("[input] input-capture disarmed");
    }
}
