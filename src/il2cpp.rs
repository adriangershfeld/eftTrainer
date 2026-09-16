//! Raw IL2CPP FFI. Deliberately binds a SHORT list of exports.
//!
//! BSG ships decoyed exports: the name resolves, GetProcAddress succeeds, and
//! the body you get is not the real function. Verified on 1.1.0.1.46911 by
//! diffing on-disk bytes against a live process -- e.g. il2cpp_class_from_name
//! exports at rva 0x321D70 while Class::FromName actually lives at 0x399CE0.
//! A "sig-scan if the export is missing" fallback never fires against that,
//! because the export is present.
//!
//! So the rule here: bind only exports confirmed to be genuine thunks into the
//! real implementation, and rebuild everything else from metadata walks in
//! il2cpp_abi / il2cpp_runtime. The genuine set happens to be exactly the
//! enumeration primitives, which is all we need.
//!
//! Nothing in this file knows a struct offset. That is il2cpp_abi's job.

#![allow(dead_code)]

use std::ffi::{CStr, CString, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
};
use windows::core::{PCSTR, s};

pub const MODULE: &str = "GameAssembly.dll";

// ── memory safety helpers ─────────────────────────────────────────────────────
// Calibration probes addresses that may not be mapped, so every read during
// discovery goes through here. VirtualQuery rather than IsBadReadPtr: the
// latter can trip guard pages and is documented as unusable in new code.
//
// A VirtualQuery per read is far too slow in a process this size (thousands
// of regions, tens of microseconds each, millions of reads). So the committed
// readable regions are snapshotted once into a sorted table and membership
// is a binary search. While calibration runs the table is authoritative, so
// the constant stream of deliberate misses costs nothing. Afterwards a miss
// falls back to one VirtualQuery, which keeps later allocations visible.

static REGIONS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
static CACHE_ONLY: AtomicBool = AtomicBool::new(false);

/// Walk the address space and rebuild the readable-region table.
pub unsafe fn refresh_regions() -> usize {
    let mut v: Vec<(usize, usize)> = Vec::with_capacity(8192);
    let mut mbi = MEMORY_BASIC_INFORMATION::default();
    let sz = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    let mut addr: usize = 0;
    while addr < 0x7FFF_FFFF_0000 {
        let n = unsafe { VirtualQuery(Some(addr as *const c_void), &mut mbi, sz) };
        if n == 0 {
            break;
        }
        let base = mbi.BaseAddress as usize;
        let end = base.saturating_add(mbi.RegionSize);
        let ok = mbi.State == MEM_COMMIT
            && mbi.Protect.0 != 0
            && mbi.Protect.0 & (PAGE_NOACCESS.0 | PAGE_GUARD.0) == 0;
        if ok {
            match v.last_mut() {
                Some(last) if last.1 == base => last.1 = end,
                _ => v.push((base, end)),
            }
        }
        if end <= addr {
            break;
        }
        addr = end;
    }
    let n = v.len();
    if let Ok(mut g) = REGIONS.lock() {
        *g = v;
    }
    n
}

/// Calibration flips this on so misses are answered from the table alone.
pub struct CacheOnly;
impl CacheOnly {
    pub fn enter() -> Self {
        CACHE_ONLY.store(true, Ordering::SeqCst);
        CacheOnly
    }
}
impl Drop for CacheOnly {
    fn drop(&mut self) {
        CACHE_ONLY.store(false, Ordering::SeqCst);
    }
}

fn in_table(a: usize, len: usize) -> bool {
    let Ok(g) = REGIONS.lock() else { return false };
    let i = g.partition_point(|r| r.0 <= a);
    if i == 0 {
        return false;
    }
    let (s, e) = g[i - 1];
    a >= s && a.saturating_add(len) <= e
}

pub unsafe fn readable(p: *const u8, len: usize) -> bool {
    if p.is_null() || len == 0 {
        return false;
    }
    if in_table(p as usize, len) {
        return true;
    }
    if CACHE_ONLY.load(Ordering::Relaxed) {
        return false;
    }
    let mut mbi = MEMORY_BASIC_INFORMATION::default();
    let n = unsafe {
        VirtualQuery(
            Some(p as *const c_void),
            &mut mbi,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if n == 0 || mbi.State != MEM_COMMIT {
        return false;
    }
    if mbi.Protect.0 & (PAGE_NOACCESS.0 | PAGE_GUARD.0) != 0 {
        return false;
    }
    // Must not run off the end of the region.
    let region_end = mbi.BaseAddress as usize + mbi.RegionSize;
    (p as usize).saturating_add(len) <= region_end
}

#[inline]
pub unsafe fn read<T: Copy>(p: *const u8) -> Option<T> {
    if unsafe { readable(p, std::mem::size_of::<T>()) } {
        Some(unsafe { (p as *const T).read_unaligned() })
    } else {
        None
    }
}

#[inline]
pub unsafe fn read_ptr(p: *const u8) -> Option<*mut u8> {
    let v: usize = unsafe { read(p) }?;
    if v == 0 { None } else { Some(v as *mut u8) }
}

/// A NUL-terminated ASCII string, bounded. None if it is not plausibly text.
pub unsafe fn cstr(p: *const u8, max: usize) -> Option<String> {
    if !unsafe { readable(p, 1) } {
        return None;
    }
    // Probe a window at a time instead of a VirtualQuery per byte: calibration
    // reads millions of these and the syscall dominates otherwise. Shrink the
    // window at region boundaries so a short string near the end still reads.
    let mut out = Vec::with_capacity(32);
    let mut n = 0usize;
    while n < max {
        let mut w = 64.min(max - n);
        while w > 0 && !unsafe { readable(p.add(n), w) } {
            w /= 2;
        }
        if w == 0 {
            return None;
        }
        for i in 0..w {
            let b = unsafe { *p.add(n + i) };
            if b == 0 {
                return String::from_utf8(out).ok();
            }
            if !(0x20..0x7f).contains(&b) {
                return None;
            }
            out.push(b);
        }
        n += w;
    }
    None
}

/// C# identifiers, including the compiler's decorations: `<>c__DisplayClass`,
/// backtick arity, nested-type dots.
pub fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() < 512
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'`' | b'<' | b'>' | b'/' | b'[' | b']' | b',' | b'|' | b'-' | b'+' | b'=' | b' ')
        })
}

/// Namespaces are dotted or empty.
pub fn is_namespace(s: &str) -> bool {
    s.is_empty() || is_identifier(s)
}

// ── export bindings ───────────────────────────────────────────────────────────

type FnPtr = unsafe extern "system" fn() -> isize;

type FnDomainGet = unsafe extern "C" fn() -> *mut c_void;
type FnThreadAttach = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnThreadCurrent = unsafe extern "C" fn() -> *mut c_void;
type FnDomainGetAssemblies = unsafe extern "C" fn(*mut c_void, *mut usize) -> *mut *mut c_void;
type FnAssemblyGetImage = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnImageGetClassCount = unsafe extern "C" fn(*mut c_void) -> usize;
type FnImageGetClass = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;
type FnGetCorlib = unsafe extern "C" fn() -> *mut c_void;
type FnRuntimeInvoke =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut *mut c_void, *mut *mut c_void) -> *mut c_void;
type FnObjectNew = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type FnStringNew = unsafe extern "C" fn(*const i8) -> *mut c_void;
// il2cpp_array_new(Il2CppClass* element_type, il2cpp_array_size_t length) -> Il2CppArray*
type FnArrayNew = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;

/// Every entry here was confirmed genuine on 46911: on-disk bytes match the
/// live image, and GooseAbi (which exists purely to repair the decoyed ones)
/// leaves all of them alone.
pub struct Api {
    pub base: usize,
    domain_get: FnDomainGet,
    thread_attach: FnThreadAttach,
    thread_current: FnThreadCurrent,
    domain_get_assemblies: FnDomainGetAssemblies,
    assembly_get_image: FnAssemblyGetImage,
    image_get_class_count: FnImageGetClassCount,
    image_get_class: FnImageGetClass,
    get_corlib: FnGetCorlib,
    runtime_invoke: FnRuntimeInvoke,
    object_new: FnObjectNew,
    string_new: FnStringNew,
    /// Optional: allocation primitive for managed arrays. Not in the verified
    /// genuine set, so it is loaded best-effort and validated by a runtime
    /// self-test before use (see il2cpp_runtime). None means "not available".
    array_new: Option<FnArrayNew>,
}

unsafe impl Send for Api {}
unsafe impl Sync for Api {}

unsafe fn module(name: PCSTR) -> Option<HMODULE> {
    unsafe { GetModuleHandleA(name) }.ok().filter(|h| !h.is_invalid())
}

macro_rules! sym {
    ($h:expr, $lit:expr) => {{
        match unsafe { GetProcAddress($h, $lit) } {
            Some(p) => unsafe { std::mem::transmute::<FnPtr, _>(p) },
            None => {
                crate::elog!("[il2cpp] missing export: {}", stringify!($lit));
                return None;
            }
        }
    }};
}

impl Api {
    /// None means GameAssembly.dll never showed up, i.e. not an IL2CPP build.
    pub unsafe fn load(wait: Duration) -> Option<Self> {
        let deadline = Instant::now() + wait;
        let h = loop {
            if let Some(h) = unsafe { module(s!("GameAssembly.dll")) } {
                break h;
            }
            if Instant::now() >= deadline || crate::control::aborted() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(250));
        };

        let api = Self {
            base: h.0 as usize,
            domain_get: sym!(h, s!("il2cpp_domain_get")),
            thread_attach: sym!(h, s!("il2cpp_thread_attach")),
            thread_current: sym!(h, s!("il2cpp_thread_current")),
            domain_get_assemblies: sym!(h, s!("il2cpp_domain_get_assemblies")),
            assembly_get_image: sym!(h, s!("il2cpp_assembly_get_image")),
            image_get_class_count: sym!(h, s!("il2cpp_image_get_class_count")),
            image_get_class: sym!(h, s!("il2cpp_image_get_class")),
            get_corlib: sym!(h, s!("il2cpp_get_corlib")),
            runtime_invoke: sym!(h, s!("il2cpp_runtime_invoke")),
            object_new: sym!(h, s!("il2cpp_object_new")),
            string_new: sym!(h, s!("il2cpp_string_new")),
            // Best-effort: absence just means dual-tone chams falls back to
            // single material. Never fail load over it.
            array_new: match unsafe { GetProcAddress(h, s!("il2cpp_array_new")) } {
                Some(p) => Some(unsafe { std::mem::transmute::<FnPtr, FnArrayNew>(p) }),
                None => {
                    crate::elog!("[il2cpp] il2cpp_array_new export absent");
                    None
                }
            },
        };
        crate::elog!("[il2cpp] GameAssembly.dll @ {:#x}", api.base);
        Some(api)
    }

    pub unsafe fn domain(&self) -> *mut c_void {
        unsafe { (self.domain_get)() }
    }
    pub unsafe fn attach(&self, domain: *mut c_void) -> *mut c_void {
        unsafe { (self.thread_attach)(domain) }
    }
    pub unsafe fn thread_current(&self) -> *mut c_void {
        unsafe { (self.thread_current)() }
    }
    pub unsafe fn corlib(&self) -> *mut c_void {
        unsafe { (self.get_corlib)() }
    }

    pub unsafe fn assemblies(&self, domain: *mut c_void) -> Vec<*mut c_void> {
        let mut n: usize = 0;
        let p = unsafe { (self.domain_get_assemblies)(domain, &mut n) };
        if p.is_null() || n == 0 || n > 4096 {
            return Vec::new();
        }
        (0..n).filter_map(|i| unsafe { read_ptr(p.add(i) as *const u8) }.map(|q| q as *mut c_void)).collect()
    }

    pub unsafe fn image_of(&self, assembly: *mut c_void) -> *mut c_void {
        unsafe { (self.assembly_get_image)(assembly) }
    }
    pub unsafe fn class_count(&self, image: *mut c_void) -> usize {
        unsafe { (self.image_get_class_count)(image) }
    }
    pub unsafe fn class_at(&self, image: *mut c_void, i: usize) -> *mut c_void {
        unsafe { (self.image_get_class)(image, i) }
    }

    pub unsafe fn invoke(
        &self,
        method: *mut c_void,
        obj: *mut c_void,
        args: &mut [*mut c_void],
    ) -> Result<*mut c_void, ()> {
        let mut exc: *mut c_void = std::ptr::null_mut();
        let argp = if args.is_empty() { std::ptr::null_mut() } else { args.as_mut_ptr() };
        let r = unsafe { (self.runtime_invoke)(method, obj, argp, &mut exc) };
        if exc.is_null() { Ok(r) } else { Err(()) }
    }

    pub unsafe fn object_new(&self, class: *mut c_void) -> *mut c_void {
        unsafe { (self.object_new)(class) }
    }

    pub unsafe fn string_new(&self, text: &str) -> *mut c_void {
        let Ok(c) = CString::new(text) else { return std::ptr::null_mut() };
        unsafe { (self.string_new)(c.as_ptr()) }
    }

    pub fn has_array_new(&self) -> bool {
        self.array_new.is_some()
    }

    /// Null if the export was absent. The caller is responsible for validating
    /// the result before trusting it (the export is not in the genuine set).
    pub unsafe fn array_new(&self, element_class: *mut c_void, len: usize) -> *mut c_void {
        match self.array_new {
            Some(f) => unsafe { f(element_class, len) },
            None => std::ptr::null_mut(),
        }
    }
}

/// Best-effort name for logging.
pub unsafe fn cstr_lossy(p: *const u8) -> String {
    if !unsafe { readable(p, 1) } {
        return "<unreadable>".into();
    }
    unsafe { CStr::from_ptr(p as *const i8) }.to_string_lossy().into_owned()
}

/// Readability by a live VirtualQuery, ignoring the calibration snapshot. The
/// snapshot can be stale for arbitrary heap addresses (freed since startup), so
/// the HTTP reader uses this for user-supplied addresses.
pub unsafe fn readable_now(p: *const u8, len: usize) -> bool {
    if p.is_null() || len == 0 {
        return false;
    }
    let mut mbi = MEMORY_BASIC_INFORMATION::default();
    let n = unsafe {
        VirtualQuery(
            Some(p as *const c_void),
            &mut mbi,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if n == 0 || mbi.State != MEM_COMMIT {
        return false;
    }
    if mbi.Protect.0 & (PAGE_NOACCESS.0 | PAGE_GUARD.0) != 0 {
        return false;
    }
    let region_end = mbi.BaseAddress as usize + mbi.RegionSize;
    (p as usize).saturating_add(len) <= region_end
}

#[inline]
pub unsafe fn read_now<T: Copy>(p: *const u8) -> Option<T> {
    if unsafe { readable_now(p, std::mem::size_of::<T>()) } {
        Some(unsafe { (p as *const T).read_unaligned() })
    } else {
        None
    }
}

/// (base, end) of a loaded module by name, from its PE SizeOfImage. For the
/// HTTP scanner to bound a search to GameAssembly.dll etc.
pub unsafe fn module_bounds(name: &str) -> Option<(usize, usize)> {
    let cname = CString::new(name).ok()?;
    let h = unsafe { GetModuleHandleA(PCSTR(cname.as_ptr() as *const u8)) }.ok()?;
    if h.is_invalid() {
        return None;
    }
    let base = h.0 as usize;
    let e_lfanew: u32 = unsafe { read((base + 0x3C) as *const u8) }?;
    // PE sig (4) + IMAGE_FILE_HEADER (20) + OptionalHeader64.SizeOfImage (0x38)
    let size: u32 = unsafe { read((base + e_lfanew as usize + 0x50) as *const u8) }?;
    Some((base, base + size as usize))
}

/// Scan committed, readable memory in [start, end) for `needle`. Only touches
/// non-guard committed regions, so no access violation. Bounded by max_hits and
/// max_bytes, and it bails on abort.
pub unsafe fn scan(
    needle: &[u8],
    start: usize,
    end: usize,
    max_hits: usize,
    max_bytes: usize,
) -> Vec<usize> {
    let mut hits = Vec::new();
    if needle.is_empty() || start >= end {
        return hits;
    }
    let mut mbi = MEMORY_BASIC_INFORMATION::default();
    let sz = std::mem::size_of::<MEMORY_BASIC_INFORMATION>();
    let mut addr = start;
    let mut scanned = 0usize;
    while addr < end {
        if crate::control::aborted() {
            break;
        }
        let n = unsafe { VirtualQuery(Some(addr as *const c_void), &mut mbi, sz) };
        if n == 0 {
            break;
        }
        let base = mbi.BaseAddress as usize;
        let rend = base.saturating_add(mbi.RegionSize);
        let committed = mbi.State == MEM_COMMIT
            && mbi.Protect.0 != 0
            && mbi.Protect.0 & (PAGE_NOACCESS.0 | PAGE_GUARD.0) == 0;
        if committed {
            let s = base.max(start);
            let e = rend.min(end);
            if e > s {
                let len = e - s;
                let slice = unsafe { std::slice::from_raw_parts(s as *const u8, len) };
                let first = needle[0];
                let mut i = 0usize;
                while i + needle.len() <= slice.len() {
                    if slice[i] == first && &slice[i..i + needle.len()] == needle {
                        hits.push(s + i);
                        if hits.len() >= max_hits {
                            return hits;
                        }
                    }
                    i += 1;
                    if i & 0x1F_FFFF == 0 && crate::control::aborted() {
                        return hits;
                    }
                }
                scanned = scanned.saturating_add(len);
                if scanned >= max_bytes {
                    break;
                }
            }
        }
        if rend <= addr {
            break;
        }
        addr = rend;
    }
    hits
}
