//! Runtime discovery of the IL2CPP metadata struct layout.
//!
//! No offset in this crate is a constant. Everything below is derived at load
//! time by probing real metadata and checking invariants that hold by
//! definition rather than by build:
//!
//!   * corlib always contains System.Object and System.String
//!   * String's parent is Object
//!   * Il2CppClass.byval_arg.data.klass points back at its own class
//!   * FieldInfo.parent points back at the class that owns the array
//!   * MethodInfo.klass likewise
//!   * MethodInfo.methodPointer lands inside GameAssembly's own image
//!   * String::get_Length is instance/0-arg, String::IsNullOrEmpty is static/1-arg
//!
//! Those survive game patches, Unity bumps and re-obfuscation, which a table
//! of RVAs does not. `EXPECTED_46911` at the bottom is a self-test, not an
//! input: if a future build moves something, calibration still wins and the
//! mismatch is logged loudly instead of silently reading garbage.

#![allow(dead_code)]

use std::collections::HashMap;

use crate::il2cpp::{
    Api, CacheOnly, cstr, is_identifier, is_namespace, read, read_ptr, readable, refresh_regions,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    // Il2CppClass
    pub c_name: usize,
    pub c_namespace: usize,
    pub c_byval: usize,
    pub c_parent: usize,
    pub c_fields: usize,
    pub c_field_count: usize,
    pub c_methods: usize,
    pub c_method_count: usize,
    // FieldInfo
    pub f_name: usize,
    pub f_parent: usize,
    pub f_offset: usize,
    pub f_stride: usize,
    // MethodInfo
    pub m_ptr: usize,
    pub m_name: usize,
    pub m_klass: usize,
    pub m_flags: usize,
    pub m_param_count: usize,
    pub m_bits: usize,
    // Il2CppImage
    pub i_name: usize,
}

/// METHOD_ATTRIBUTE_STATIC
pub const ATTR_STATIC: u16 = 0x0010;

// ── probing helpers ───────────────────────────────────────────────────────────

unsafe fn u16_at(p: *const u8, off: usize) -> Option<u16> {
    unsafe { read::<u16>(p.add(off)) }
}
unsafe fn u8_at(p: *const u8, off: usize) -> Option<u8> {
    unsafe { read::<u8>(p.add(off)) }
}
unsafe fn str_at(p: *const u8, off: usize) -> Option<String> {
    let s = unsafe { read_ptr(p.add(off)) }?;
    unsafe { cstr(s, 512) }
}
unsafe fn ptr_at(p: *const u8, off: usize) -> Option<*mut u8> {
    unsafe { read_ptr(p.add(off)) }
}

/// SizeOfImage straight out of the mapped PE header, so "is this a code
/// pointer into GameAssembly" is answerable without a hardcoded size.
unsafe fn image_span(base: usize) -> (usize, usize) {
    let b = base as *const u8;
    let lfanew: u32 = unsafe { read(b.add(0x3C)) }.unwrap_or(0);
    if lfanew == 0 || lfanew > 0x1000 {
        return (base, base);
    }
    let size: u32 = unsafe { read(b.add(lfanew as usize + 24 + 56)) }.unwrap_or(0);
    (base, base + size as usize)
}

const CLASS_WINDOW: usize = 0x180;
/// Anchor types can sit anywhere in metadata order, so the cheap passes look
/// at every class in corlib.
const SAMPLE_MAX: usize = 4096;
/// The field/method passes are O(offsets x samples x inner), so they run over
/// an evenly spread subset instead.
const PROBE_MAX: usize = 192;

// ── calibration ───────────────────────────────────────────────────────────────

/// `s_len` / `s_chars` are Il2CppString's length and first-char offsets,
/// probed by the caller from a string it allocated. They anchor
/// FieldInfo.offset below.
pub unsafe fn calibrate(api: &Api, s_len: usize, s_chars: usize) -> Option<Layout> {
    let t0 = std::time::Instant::now();
    let nreg = unsafe { refresh_regions() };
    crate::elog!("[abi] snapshotted {} readable regions in {:?}", nreg, t0.elapsed());
    // Every probe below is answered from the table. Dropped on any return.
    let _fast = CacheOnly::enter();

    let corlib = unsafe { api.corlib() };
    if corlib.is_null() {
        crate::elog!("[abi] il2cpp_get_corlib returned null");
        return None;
    }
    let n = unsafe { api.class_count(corlib) };
    crate::elog!("[abi] corlib has {} classes", n);
    if n < 32 {
        crate::elog!("[abi] too few classes to calibrate");
        return None;
    }

    let mut samples: Vec<*mut u8> = Vec::with_capacity(SAMPLE_MAX);
    for i in 0..n.min(SAMPLE_MAX) {
        let c = unsafe { api.class_at(corlib, i) } as *mut u8;
        if unsafe { readable(c, CLASS_WINDOW) } {
            samples.push(c);
        }
    }
    if samples.len() < 32 {
        crate::elog!("[abi] only {} readable classes", samples.len());
        return None;
    }

    // Evenly spread subset for the expensive field/method passes.
    let step = (samples.len() / PROBE_MAX).max(1);
    let probe: Vec<*mut u8> = samples.iter().copied().step_by(step).take(PROBE_MAX).collect();
    crate::elog!("[abi] {} readable classes, {} used as probes", samples.len(), probe.len());

    // -- name: the offset where nearly every class yields an identifier, and
    //    where the resulting set contains the two types corlib must have.
    let mut c_name = usize::MAX;
    let mut best = 0usize;
    let mut diag: Vec<(usize, usize, bool, String)> = Vec::new();
    for off in (0..0x80).step_by(8) {
        let names: Vec<String> = samples
            .iter()
            .filter_map(|&c| unsafe { str_at(c, off) })
            .filter(|s| is_identifier(s))
            .collect();
        let has_anchor = names.iter().any(|s| s == "Object") && names.iter().any(|s| s == "String");
        let sample_txt = names.iter().take(4).cloned().collect::<Vec<_>>().join(",");
        diag.push((off, names.len(), has_anchor, sample_txt));
        if has_anchor && names.len() > best {
            best = names.len();
            c_name = off;
        }
    }
    if c_name == usize::MAX {
        crate::elog!("[abi] could not locate Il2CppClass.name -- candidates:");
        diag.sort_by_key(|d| std::cmp::Reverse(d.1));
        for (off, cnt, anchor, ex) in diag.iter().take(5) {
            crate::elog!("[abi]   {:#x}: {} strings, anchors={} e.g. {}", off, cnt, anchor, ex);
        }
        return None;
    }
    crate::elog!("[abi] class.name {:#x} ({}/{} resolved)", c_name, best, samples.len());

    let name_of = |c: *mut u8| -> String { unsafe { str_at(c, c_name) }.unwrap_or_default() };

    // -- namespace: the neighbouring string pointer that reads "System" most.
    let mut c_namespace = usize::MAX;
    let mut best_sys = 0usize;
    for off in (0..0x60).step_by(8) {
        if off == c_name {
            continue;
        }
        let mut ok = 0usize;
        let mut sys = 0usize;
        for &c in &samples {
            if let Some(s) = unsafe { str_at(c, off) } {
                if is_namespace(&s) {
                    ok += 1;
                    if s == "System" {
                        sys += 1;
                    }
                }
            }
        }
        if sys > best_sys && ok > samples.len() / 2 {
            best_sys = sys;
            c_namespace = off;
        }
    }
    if c_namespace == usize::MAX {
        crate::elog!("[abi] could not locate Il2CppClass.namespaze");
        return None;
    }
    crate::elog!("[abi] class.namespace {:#x} ({} in System)", c_namespace, best_sys);

    // Anchor classes, by System.<name> so a same-named type elsewhere in
    // corlib cannot be picked up.
    let find_sys = |n: &str| -> Option<*mut u8> {
        samples.iter().copied().find(|&c| {
            name_of(c) == n && unsafe { str_at(c, c_namespace) }.as_deref() == Some("System")
        })
    };
    let (a_str, a_obj, a_i32) = match (find_sys("String"), find_sys("Object"), find_sys("Int32")) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => {
            crate::elog!("[abi] System.String/Object/Int32 not all found in corlib");
            return None;
        }
    };

    // -- byval_arg: an inline Il2CppType. Its second qword is a bitfield with
    //    the ECMA element type in bits 16..23, fixed by the CLI spec:
    //    STRING=0x0e, OBJECT=0x1c, I4=0x08. this_arg is the same struct 0x10
    //    later with the same element type, so the lowest fit is byval_arg.
    //    (Its data union is NOT a back-pointer to the class; element_class
    //    and castClass are, which is how a self-pointer search picks 0x40.)
    let elem = |c: *mut u8, x: usize| -> Option<u8> {
        let v: u32 = unsafe { read(c.add(x + 8)) }?;
        Some(((v >> 16) & 0xFF) as u8)
    };
    let mut c_byval = usize::MAX;
    for x in (0x18..0x80).step_by(8) {
        if elem(a_str, x) != Some(0x0e) || elem(a_obj, x) != Some(0x1c) || elem(a_i32, x) != Some(0x08) {
            continue;
        }
        let sane = samples
            .iter()
            .filter(|&&c| matches!((elem(c, x), elem(c, x + 0x10)),
                (Some(t), Some(t2)) if (1..=0x20).contains(&t) && t2 == t))
            .count();
        if sane > samples.len() / 2 {
            c_byval = x;
            break;
        }
    }
    if c_byval == usize::MAX {
        crate::elog!("[abi] could not locate Il2CppClass.byval_arg -- element bytes per offset:");
        for x in (0x18..0x80).step_by(8) {
            crate::elog!("[abi]   {:#x}: String={:?} Object={:?} Int32={:?}",
                x, elem(a_str, x), elem(a_obj, x), elem(a_i32, x));
        }
        return None;
    }
    crate::elog!("[abi] class.byval_arg {:#x} (String=0e Object=1c Int32=08 confirmed)", c_byval);

    // -- parent: points at another class, and String's must be Object.
    let str_cls = Some(a_str);
    let mut c_parent = usize::MAX;
    let mut best_p = 0usize;
    for off in (0..0xA0).step_by(8) {
        if off == c_byval {
            continue;
        }
        let mut hits = 0usize;
        for &c in &samples {
            let Some(p) = (unsafe { ptr_at(c, off) }) else { continue };
            if p == c || !unsafe { readable(p, CLASS_WINDOW) } {
                continue;
            }
            if unsafe { str_at(p, c_name) }.is_some_and(|s| is_identifier(&s)) {
                hits += 1;
            }
        }
        let anchor_ok = match str_cls {
            Some(sc) => unsafe { ptr_at(sc, off) }
                .map(|p| name_of(p) == "Object")
                .unwrap_or(false),
            None => false,
        };
        if anchor_ok && hits > best_p {
            best_p = hits;
            c_parent = off;
        }
    }
    if c_parent == usize::MAX {
        crate::elog!("[abi] could not locate Il2CppClass.parent (String->Object failed)");
        return None;
    }
    crate::elog!("[abi] class.parent {:#x} (String->Object confirmed)", c_parent);

    // -- FieldInfo: find the array pointer and the intra-struct offsets
    //    together, anchored on FieldInfo.parent == owning class.
    let mut c_fields = usize::MAX;
    let mut f_name = usize::MAX;
    let mut f_parent = usize::MAX;
    let mut best_f = 0usize;
    for off in (0..CLASS_WINDOW).step_by(8) {
        if crate::control::aborted() {
            crate::elog!("[abi] aborted during field scan");
            return None;
        }
        let mut votes: HashMap<(usize, usize), usize> = HashMap::new();
        for &c in &probe {
            let Some(p) = (unsafe { ptr_at(c, off) }) else { continue };
            if !unsafe { readable(p, 0x20) } {
                continue;
            }
            for fp in (0..0x20).step_by(8) {
                if unsafe { ptr_at(p, fp) } != Some(c) {
                    continue;
                }
                for fnm in (0..0x20).step_by(8) {
                    if fnm == fp {
                        continue;
                    }
                    if unsafe { str_at(p, fnm) }.is_some_and(|s| is_identifier(&s)) {
                        *votes.entry((fnm, fp)).or_default() += 1;
                    }
                }
            }
        }
        if let Some((&(fnm, fp), &v)) = votes.iter().max_by_key(|&(_, &v)| v) {
            if v > best_f {
                best_f = v;
                c_fields = off;
                f_name = fnm;
                f_parent = fp;
            }
        }
    }
    if c_fields == usize::MAX {
        crate::elog!("[abi] could not locate Il2CppClass.fields");
        return None;
    }
    crate::elog!(
        "[abi] class.fields {:#x}  FieldInfo.name {:#x} .parent {:#x} ({} classes agree)",
        c_fields, f_name, f_parent, best_f
    );

    // stride: the next entry must still belong to the same class.
    let mut f_stride = usize::MAX;
    for cand in [0x20usize, 0x18, 0x28, 0x30, 0x10] {
        let hits = probe
            .iter()
            .filter(|&&c| {
                let Some(p) = (unsafe { ptr_at(c, c_fields) }) else { return false };
                (unsafe { readable(p, cand * 2) })
                    && (unsafe { ptr_at(p.add(cand), f_parent) }) == Some(c)
            })
            .count();
        if hits >= 8 {
            f_stride = cand;
            break;
        }
    }
    if f_stride == usize::MAX {
        f_stride = 0x20;
        crate::elog!("[abi] FieldInfo stride not confirmed, assuming {:#x}", f_stride);
    } else {
        crate::elog!("[abi] FieldInfo stride {:#x}", f_stride);
    }

    let field_ok = |c: *mut u8, i: usize| -> bool {
        let Some(p) = (unsafe { ptr_at(c, c_fields) }) else { return false };
        let e = unsafe { p.add(i * f_stride) };
        (unsafe { readable(e, f_stride) })
            && (unsafe { ptr_at(e, f_parent) }) == Some(c)
            && (unsafe { str_at(e, f_name) }).is_some_and(|s| is_identifier(&s))
    };

    // field_count: entries [0, v) valid, entry v not. u16 in the tail.
    let mut c_field_count = usize::MAX;
    let mut best_fc = 0usize;
    for off in (0x80..CLASS_WINDOW).step_by(2) {
        if crate::control::aborted() {
            crate::elog!("[abi] aborted during field_count scan");
            return None;
        }
        let mut good = 0usize;
        for &c in &probe {
            let Some(v) = (unsafe { u16_at(c, off) }) else { continue };
            if v == 0 || v > 400 {
                continue;
            }
            let inside = (0..v as usize).take(6).all(|i| field_ok(c, i));
            let past = field_ok(c, v as usize);
            if inside && !past {
                good += 1;
            }
        }
        if good > best_fc {
            best_fc = good;
            c_field_count = off;
        }
    }
    if c_field_count == usize::MAX || best_fc < 8 {
        crate::elog!("[abi] could not locate Il2CppClass.field_count");
        return None;
    }
    crate::elog!("[abi] class.field_count {:#x} ({} classes agree)", c_field_count, best_fc);

    // -- FieldInfo.offset: pinned by System.String's own layout, which was
    //    probed at runtime from a string we built ourselves. Among String's
    //    fields, one must sit at the length slot and another at the first
    //    char. Pointer-bearing slots are excluded so the high half of `name`
    //    (which reads as a small int on a low heap) can never qualify.
    let n_str = unsafe { u16_at(a_str, c_field_count) }.unwrap_or(0) as usize;
    let str_fields: Vec<*mut u8> = (0..n_str.min(16))
        .filter_map(|i| {
            let p = unsafe { ptr_at(a_str, c_fields) }?;
            let e = unsafe { p.add(i * f_stride) };
            ((unsafe { readable(e, f_stride) }) && (unsafe { ptr_at(e, f_parent) }) == Some(a_str))
                .then_some(e)
        })
        .collect();
    let is_ptr_slot = |slot: usize| -> bool {
        let hits = str_fields
            .iter()
            .filter(|&&e| unsafe { ptr_at(e, slot) }.is_some_and(|p| unsafe { readable(p, 1) }))
            .count();
        hits * 2 > str_fields.len()
    };
    let mut f_offset = usize::MAX;
    for cand in (0..f_stride).step_by(4) {
        if is_ptr_slot(cand & !7) {
            continue;
        }
        let vals: Vec<i32> = str_fields
            .iter()
            .filter_map(|&e| unsafe { read::<i32>(e.add(cand)) })
            .collect();
        if vals.contains(&(s_len as i32)) && vals.contains(&(s_chars as i32)) {
            f_offset = cand;
            break;
        }
    }
    if f_offset == usize::MAX {
        crate::elog!("[abi] could not locate FieldInfo.offset ({} String fields, want {:#x}/{:#x})",
            str_fields.len(), s_len, s_chars);
        for &e in str_fields.iter().take(6) {
            crate::elog!("[abi]   field {:?}: {}", unsafe { str_at(e, f_name) },
                (0..f_stride).step_by(4)
                    .map(|c| format!("+{:#x}={:#x}", c, unsafe { read::<i32>(e.add(c)) }.unwrap_or(-1)))
                    .collect::<Vec<_>>().join(" "));
        }
        return None;
    }
    crate::elog!("[abi] FieldInfo.offset {:#x} (String length@{:#x} chars@{:#x} confirmed)",
        f_offset, s_len, s_chars);

    unsafe { calibrate_methods(api, &samples, &probe, c_name, c_namespace, c_byval, c_parent,
                               c_fields, c_field_count, f_name, f_parent, f_offset, f_stride) }
}

#[allow(clippy::too_many_arguments)]
unsafe fn calibrate_methods(
    api: &Api,
    samples: &[*mut u8],
    probe: &[*mut u8],
    c_name: usize,
    c_namespace: usize,
    c_byval: usize,
    c_parent: usize,
    c_fields: usize,
    c_field_count: usize,
    f_name: usize,
    f_parent: usize,
    f_offset: usize,
    f_stride: usize,
) -> Option<Layout> {
    let name_of = |c: *mut u8| -> String { unsafe { str_at(c, c_name) }.unwrap_or_default() };

    // -- methods: array of MethodInfo*, anchored on MethodInfo.klass == class.
    let mut c_methods = usize::MAX;
    let mut m_name = usize::MAX;
    let mut m_klass = usize::MAX;
    let mut best_m = 0usize;
    for off in (0..CLASS_WINDOW).step_by(8) {
        if crate::control::aborted() {
            crate::elog!("[abi] aborted during method scan");
            return None;
        }
        if off == c_fields {
            continue;
        }
        let mut votes: HashMap<(usize, usize), usize> = HashMap::new();
        for &c in probe {
            let Some(arr) = (unsafe { ptr_at(c, off) }) else { continue };
            if !unsafe { readable(arr, 8) } {
                continue;
            }
            let Some(m) = (unsafe { read_ptr(arr) }) else { continue };
            if !unsafe { readable(m, 0x60) } {
                continue;
            }
            for mk in (0..0x40).step_by(8) {
                if unsafe { ptr_at(m, mk) } != Some(c) {
                    continue;
                }
                for mn in (0..0x40).step_by(8) {
                    if mn == mk {
                        continue;
                    }
                    if unsafe { str_at(m, mn) }.is_some_and(|s| is_identifier(&s)) {
                        *votes.entry((mn, mk)).or_default() += 1;
                    }
                }
            }
        }
        if let Some((&(mn, mk), &v)) = votes.iter().max_by_key(|&(_, &v)| v) {
            if v > best_m {
                best_m = v;
                c_methods = off;
                m_name = mn;
                m_klass = mk;
            }
        }
    }
    if c_methods == usize::MAX {
        crate::elog!("[abi] could not locate Il2CppClass.methods");
        return None;
    }
    crate::elog!(
        "[abi] class.methods {:#x}  MethodInfo.name {:#x} .klass {:#x} ({} classes agree)",
        c_methods, m_name, m_klass, best_m
    );

    let method_at = |c: *mut u8, i: usize| -> Option<*mut u8> {
        let arr = unsafe { ptr_at(c, c_methods) }?;
        let m = unsafe { read_ptr(arr.add(i * 8)) }?;
        unsafe { readable(m, 0x60) }.then_some(m)
    };
    let method_ok = |c: *mut u8, i: usize| -> bool {
        match method_at(c, i) {
            Some(m) => {
                (unsafe { ptr_at(m, m_klass) }) == Some(c)
                    && (unsafe { str_at(m, m_name) }).is_some_and(|s| is_identifier(&s))
            }
            None => false,
        }
    };

    let mut c_method_count = usize::MAX;
    let mut best_mc = 0usize;
    for off in (0x80..CLASS_WINDOW).step_by(2) {
        if crate::control::aborted() {
            crate::elog!("[abi] aborted during method_count scan");
            return None;
        }
        if off == c_field_count {
            continue;
        }
        let mut good = 0usize;
        for &c in probe {
            let Some(v) = (unsafe { u16_at(c, off) }) else { continue };
            if v == 0 || v > 2000 {
                continue;
            }
            if (0..v as usize).take(6).all(|i| method_ok(c, i)) && !method_ok(c, v as usize) {
                good += 1;
            }
        }
        if good > best_mc {
            best_mc = good;
            c_method_count = off;
        }
    }
    if c_method_count == usize::MAX || best_mc < 8 {
        crate::elog!("[abi] could not locate Il2CppClass.method_count");
        return None;
    }
    crate::elog!("[abi] class.method_count {:#x} ({} classes agree)", c_method_count, best_mc);

    // -- methodPointer: the only qword in the header that lands in our own image.
    let (lo, hi) = unsafe { image_span(api.base) };
    let mut m_ptr = usize::MAX;
    for off in (0..0x20).step_by(8) {
        let mut hits = 0usize;
        let mut total = 0usize;
        for &c in probe.iter().take(120) {
            for i in 0..4 {
                let Some(m) = method_at(c, i) else { break };
                if unsafe { ptr_at(m, m_klass) } != Some(c) {
                    break;
                }
                total += 1;
                if let Some(p) = unsafe { ptr_at(m, off) } {
                    if (lo..hi).contains(&(p as usize)) {
                        hits += 1;
                    }
                }
            }
        }
        if total > 32 && hits * 4 > total * 3 {
            m_ptr = off;
            break;
        }
    }
    if m_ptr == usize::MAX {
        crate::elog!("[abi] could not locate MethodInfo.methodPointer");
        return None;
    }
    crate::elog!("[abi] MethodInfo.methodPointer {:#x} (image {:#x}..{:#x})", m_ptr, lo, hi);

    // -- flags / param_count, discriminated by two String methods whose shape
    //    is fixed by the BCL: get_Length is instance/0, IsNullOrEmpty static/1.
    let str_cls = samples.iter().copied().find(|&c| name_of(c) == "String")?;
    let count = unsafe { u16_at(str_cls, c_method_count) }.unwrap_or(0) as usize;
    let mut get_length = None;
    let mut is_null_or_empty = None;
    for i in 0..count {
        let Some(m) = method_at(str_cls, i) else { continue };
        match unsafe { str_at(m, m_name) }.unwrap_or_default().as_str() {
            "get_Length" => get_length = Some(m),
            "IsNullOrEmpty" => is_null_or_empty = Some(m),
            _ => {}
        }
    }
    let (gl, inoe) = match (get_length, is_null_or_empty) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            crate::elog!("[abi] String::get_Length / IsNullOrEmpty not found ({} methods)", count);
            return None;
        }
    };

    let mut m_param_count = usize::MAX;
    for off in 0x30..0x60 {
        if unsafe { u8_at(gl, off) } == Some(0) && unsafe { u8_at(inoe, off) } == Some(1) {
            // must stay plausible across the rest of String's methods
            let sane = (0..count.min(40)).filter_map(|i| method_at(str_cls, i)).all(|m| {
                unsafe { u8_at(m, off) }.map(|v| v <= 32).unwrap_or(false)
            });
            if sane {
                m_param_count = off;
                break;
            }
        }
    }
    if m_param_count == usize::MAX {
        crate::elog!("[abi] could not locate MethodInfo.parameters_count");
        return None;
    }

    let mut m_flags = usize::MAX;
    for off in (0x30..0x60).step_by(2) {
        let a = unsafe { u16_at(gl, off) };
        let b = unsafe { u16_at(inoe, off) };
        if let (Some(a), Some(b)) = (a, b) {
            if a != 0 && b != 0 && a & ATTR_STATIC == 0 && b & ATTR_STATIC != 0 {
                m_flags = off;
                break;
            }
        }
    }
    if m_flags == usize::MAX {
        crate::elog!("[abi] could not locate MethodInfo.flags");
        return None;
    }

    // is_generic / is_inflated share the byte immediately after the count in
    // every IL2CPP revision; derive rather than assume, then sanity check.
    let m_bits = m_param_count + 1;
    let bits_sane = (0..count.min(40))
        .filter_map(|i| method_at(str_cls, i))
        .all(|m| unsafe { u8_at(m, m_bits) }.map(|v| v <= 0x0F).unwrap_or(false));
    if !bits_sane {
        crate::elog!("[abi] MethodInfo bitfield at {:#x} looks wrong", m_bits);
        return None;
    }
    crate::elog!(
        "[abi] MethodInfo.flags {:#x} .param_count {:#x} .bits {:#x}",
        m_flags, m_param_count, m_bits
    );

    // -- Il2CppImage.name, anchored on corlib being mscorlib.
    let corlib = unsafe { api.corlib() } as *mut u8;
    let mut i_name = usize::MAX;
    for off in (0..0x40).step_by(8) {
        if unsafe { str_at(corlib, off) }.is_some_and(|s| s.starts_with("mscorlib")) {
            i_name = off;
            break;
        }
    }
    if i_name == usize::MAX {
        crate::elog!("[abi] could not locate Il2CppImage.name");
        return None;
    }
    crate::elog!("[abi] image.name {:#x} -> {:?}", i_name, unsafe { str_at(corlib, i_name) });

    let l = Layout {
        c_name, c_namespace, c_byval, c_parent, c_fields, c_field_count,
        c_methods, c_method_count,
        f_name, f_parent, f_offset, f_stride,
        m_ptr, m_name, m_klass, m_flags, m_param_count, m_bits,
        i_name,
    };
    self_test(&l);
    crate::elog!("[abi] calibration complete");
    Some(l)
}

/// Values measured on EFT 1.1.0.1.46911. NOT used for resolution -- purely a
/// tripwire so a layout change announces itself instead of corrupting reads.
const EXPECTED_46911: Layout = Layout {
    c_name: 0x10, c_namespace: 0x18, c_byval: 0x20, c_parent: 0x58,
    c_fields: 0x80, c_field_count: 0x124, c_methods: 0x98, c_method_count: 0x120,
    f_name: 0x00, f_parent: 0x10, f_offset: 0x18, f_stride: 0x20,
    m_ptr: 0x00, m_name: 0x18, m_klass: 0x20, m_flags: 0x4C,
    m_param_count: 0x52, m_bits: 0x53,
    i_name: 0x00,
};

fn self_test(l: &Layout) {
    let e = &EXPECTED_46911;
    let mut diffs: Vec<String> = Vec::new();
    macro_rules! cmp {
        ($($f:ident),* $(,)?) => {$(
            if l.$f != e.$f {
                diffs.push(format!("{}: got {:#x} expected {:#x}", stringify!($f), l.$f, e.$f));
            }
        )*};
    }
    cmp!(c_name, c_namespace, c_byval, c_parent, c_fields, c_field_count,
         c_methods, c_method_count, f_name, f_parent, f_offset, f_stride,
         m_ptr, m_name, m_klass, m_flags, m_param_count, m_bits, i_name);

    if diffs.is_empty() {
        crate::elog!("[abi] layout matches the 46911 reference exactly");
    } else {
        crate::elog!("[abi] LAYOUT DRIFT vs 46911 reference ({} fields):", diffs.len());
        for d in diffs {
            crate::elog!("[abi]   {}", d);
        }
        crate::elog!("[abi] calibration values are being used; update the reference if this build is correct");
    }
}
