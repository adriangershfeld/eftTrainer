//! Standalone feature toggles applied on the main thread from menu::on_frame.
//!
//!   infinite stamina  implemented: pins Stamina.Current to TotalCapacity each
//!                     tick, through the same by-name chain the poll uses.
//!   speedhack         toggle + multiplier only for now (1.0..=1.4x, the range
//!                     that reads as plausible in the real game). The movement
//!                     hook that consumes it is a later pass; kept realistic on
//!                     purpose, no teleport.

#![allow(dead_code)]

use crate::runtime::{self, Object, ScriptRuntime};
use crate::runtime::Domain;
use crate::symbols::{self, eft};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;

// ── toggles (atomics: safe to set from the menu, read on the main thread) ─────
static INF_STAMINA: AtomicBool = AtomicBool::new(false);
static SPEED_ON: AtomicBool = AtomicBool::new(false);
// Speedhack multiplier stored as f32 bits. 1.0 = off. Clamped to [1.0, 1.4].
static SPEED_MULT_BITS: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0
pub const SPEED_MIN: f32 = 1.0;
pub const SPEED_MAX: f32 = 1.4;
pub const SPEED_STEP: f32 = 0.05;

pub fn set_inf_stamina(on: bool) {
    INF_STAMINA.store(on, Ordering::Relaxed);
    crate::elog!("[features] infinite stamina {}", if on { "ON" } else { "OFF" });
}
pub fn inf_stamina() -> bool {
    INF_STAMINA.load(Ordering::Relaxed)
}
pub fn toggle_inf_stamina() -> bool {
    let on = !INF_STAMINA.fetch_xor(true, Ordering::Relaxed);
    crate::elog!("[features] infinite stamina {}", if on { "ON" } else { "OFF" });
    on
}

pub fn set_speed_on(on: bool) {
    SPEED_ON.store(on, Ordering::Relaxed);
    crate::elog!("[features] speedhack {}", if on { "ON" } else { "OFF" });
}
pub fn speed_on() -> bool {
    SPEED_ON.load(Ordering::Relaxed)
}
pub fn toggle_speed() -> bool {
    let on = !SPEED_ON.fetch_xor(true, Ordering::Relaxed);
    crate::elog!("[features] speedhack {}", if on { "ON" } else { "OFF" });
    on
}
pub fn speed_mult() -> f32 {
    f32::from_bits(SPEED_MULT_BITS.load(Ordering::Relaxed))
}
pub fn set_speed_mult(m: f32) {
    let m = m.clamp(SPEED_MIN, SPEED_MAX);
    SPEED_MULT_BITS.store(m.to_bits(), Ordering::Relaxed);
}
/// Nudge the multiplier by one step (used by the menu +/- buttons).
pub fn bump_speed(up: bool) {
    let step = if up { SPEED_STEP } else { -SPEED_STEP };
    set_speed_mult(speed_mult() + step);
    crate::elog!("[features] speed x{:.2}", speed_mult());
}

// ── resolved offsets for the stamina chain (once) ─────────────────────────────
struct Cache {
    off_main_player: i32,
    off_physical: i32,
    off_stamina: i32,
    off_current: i32,
    off_capacity: i32,
}
static CACHE: OnceLock<Option<Cache>> = OnceLock::new();

unsafe fn resolve(rt: &dyn ScriptRuntime, domain: Domain) -> Option<Cache> {
    let fo = |cref, name: &str| -> Option<i32> {
        let c = unsafe { symbols::find_class(rt, domain, cref) }?;
        let f = unsafe { rt.field(c, name) }?;
        let o = unsafe { rt.field_offset(f) };
        (o > 0 && o < 0x10000).then_some(o)
    };
    let cache = Cache {
        off_main_player: fo(eft::GAME_WORLD, "MainPlayer")?,
        off_physical: fo(eft::PLAYER, "Physical")?,
        off_stamina: fo(eft::PHYSICAL_BASE, "Stamina")?,
        off_current: fo(eft::STAMINA, "Current")?,
        off_capacity: fo(eft::STAMINA, "TotalCapacity")?,
    };
    crate::elog!("[features] stamina chain resolved (current @{:#x})", cache.off_current);
    Some(cache)
}

/// Main thread, every frame. No-op unless a feature is on.
pub unsafe fn tick(rt: &dyn ScriptRuntime, domain: Domain) {
    if !INF_STAMINA.load(Ordering::Relaxed) {
        return; // speedhack has no backend yet; nothing else to apply
    }
    let cache = CACHE.get_or_init(|| unsafe { resolve(rt, domain) });
    let Some(c) = cache.as_ref() else { return };
    unsafe { apply_stamina(c) };
}

/// Pin Stamina.Current to TotalCapacity. Reads the chain fresh each tick and
/// bails at any null/unmapped link, so a menu screen or a dead world is safe.
unsafe fn apply_stamina(c: &Cache) {
    let world = crate::hooks::game_world();
    if !unsafe { runtime::is_alive(world) } {
        return;
    }
    let player = Object(unsafe { runtime::read_field::<*mut core::ffi::c_void>(world, c.off_main_player) });
    if player.is_null() || !unsafe { runtime::is_alive(player) } {
        return;
    }
    let physical = Object(unsafe { runtime::read_field::<*mut core::ffi::c_void>(player, c.off_physical) });
    if physical.is_null() {
        return;
    }
    let stamina = Object(unsafe { runtime::read_field::<*mut core::ffi::c_void>(physical, c.off_stamina) });
    if stamina.is_null()
        || !unsafe { crate::il2cpp::readable_now(stamina.raw() as *const u8, 0x40) }
    {
        return;
    }
    let cap: f32 = unsafe { runtime::read_field(stamina, c.off_capacity) };
    if cap.is_finite() && cap > 0.0 {
        unsafe { runtime::write_field::<f32>(stamina, c.off_current, cap) };
    }
}
