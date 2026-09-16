//! ScriptRuntime over IL2CPP, built from enumeration + calibrated metadata
//! walks. None of the decoyed exports are used:
//!
//!   class lookup   image_get_class_count/at + name compare, not il2cpp_class_from_name
//!   field lookup   walk Il2CppClass.fields,   not il2cpp_class_get_field_from_name
//!   method lookup  walk Il2CppClass.methods,  not il2cpp_class_get_method_from_name
//!   native_ptr     MethodInfo.methodPointer,  AOT so there is no JIT step
//!   type_object    Type.GetTypeFromHandle through runtime_invoke
//!
//! Struct offsets come from il2cpp_abi at load. Nothing here is a constant.

#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::il2cpp::{Api, cstr, read, read_ptr, readable};
use crate::il2cpp_abi::{self, ATTR_STATIC, Layout};
use crate::runtime::{Backend, Class, Domain, Field, Image, Method, Object, ScriptRuntime};

pub struct Il2CppRuntime {
    api: Api,
    l: Layout,
    /// assembly logical name -> Il2CppImage*
    images: Mutex<HashMap<String, usize>>,
    /// image -> (namespace, name) -> Il2CppClass*
    classes: Mutex<HashMap<usize, HashMap<(String, String), usize>>>,
    /// System.Type::GetTypeFromHandle, resolved once.
    type_from_handle: Mutex<Option<usize>>,
    /// Il2CppString layout, probed with a known string at startup.
    s_len: usize,
    s_chars: usize,
}

unsafe impl Send for Il2CppRuntime {}
unsafe impl Sync for Il2CppRuntime {}

impl Il2CppRuntime {
    unsafe fn str_at(&self, p: *mut u8, off: usize) -> Option<String> {
        let s = unsafe { read_ptr(p.add(off)) }?;
        unsafe { cstr(s, 512) }
    }

    unsafe fn class_name_raw(&self, c: *mut u8) -> String {
        unsafe { self.str_at(c, self.l.c_name) }.unwrap_or_default()
    }
    unsafe fn class_ns_raw(&self, c: *mut u8) -> String {
        unsafe { self.str_at(c, self.l.c_namespace) }.unwrap_or_default()
    }

    unsafe fn field_at(&self, c: *mut u8, i: usize) -> Option<*mut u8> {
        let base = unsafe { read_ptr(c.add(self.l.c_fields)) }?;
        let p = unsafe { base.add(i * self.l.f_stride) };
        unsafe { readable(p, self.l.f_stride) }.then_some(p)
    }
    unsafe fn field_count(&self, c: *mut u8) -> usize {
        unsafe { read::<u16>(c.add(self.l.c_field_count)) }.unwrap_or(0) as usize
    }

    unsafe fn method_at(&self, c: *mut u8, i: usize) -> Option<*mut u8> {
        let arr = unsafe { read_ptr(c.add(self.l.c_methods)) }?;
        let m = unsafe { read_ptr(arr.add(i * 8)) }?;
        unsafe { readable(m, 0x60) }.then_some(m)
    }
    unsafe fn method_count(&self, c: *mut u8) -> usize {
        unsafe { read::<u16>(c.add(self.l.c_method_count)) }.unwrap_or(0) as usize
    }
    unsafe fn method_name(&self, m: *mut u8) -> String {
        unsafe { self.str_at(m, self.l.m_name) }.unwrap_or_default()
    }
    unsafe fn method_argc(&self, m: *mut u8) -> u32 {
        unsafe { read::<u8>(m.add(self.l.m_param_count)) }.unwrap_or(0) as u32
    }
    unsafe fn method_is_generic(&self, m: *mut u8) -> bool {
        unsafe { read::<u8>(m.add(self.l.m_bits)) }.unwrap_or(0) & 1 != 0
    }
    unsafe fn method_is_static(&self, m: *mut u8) -> bool {
        unsafe { read::<u16>(m.add(self.l.m_flags)) }.unwrap_or(0) & ATTR_STATIC != 0
    }

    unsafe fn parent(&self, c: *mut u8) -> Option<*mut u8> {
        unsafe { read_ptr(c.add(self.l.c_parent)) }
    }

    /// Build (or reuse) the namespace+name index for one image.
    unsafe fn index_of(&self, image: *mut c_void) -> Option<()> {
        let key = image as usize;
        {
            let g = self.classes.lock().ok()?;
            if g.contains_key(&key) {
                return Some(());
            }
        }
        let n = unsafe { self.api.class_count(image) };
        let mut map: HashMap<(String, String), usize> = HashMap::with_capacity(n);
        for i in 0..n {
            if i % 512 == 0 && crate::control::aborted() {
                crate::elog!("[il2cpp] aborted while indexing image");
                return None;
            }
            let c = unsafe { self.api.class_at(image, i) } as *mut u8;
            if !unsafe { readable(c, 0x40) } {
                continue;
            }
            let name = unsafe { self.class_name_raw(c) };
            if name.is_empty() {
                continue;
            }
            let ns = unsafe { self.class_ns_raw(c) };
            map.entry((ns, name)).or_insert(c as usize);
        }
        crate::elog!("[il2cpp] indexed {} classes for image {:p}", map.len(), image);
        self.classes.lock().ok()?.insert(key, map);
        Some(())
    }

    unsafe fn type_from_handle_method(&self) -> Option<*mut u8> {
        if let Some(m) = *self.type_from_handle.lock().ok()? {
            return Some(m as *mut u8);
        }
        let corlib = unsafe { self.api.corlib() };
        unsafe { self.index_of(corlib) }?;
        let cls = {
            let g = self.classes.lock().ok()?;
            let m = g.get(&(corlib as usize))?;
            *m.get(&("System".to_string(), "Type".to_string()))? as *mut u8
        };
        let n = unsafe { self.method_count(cls) };
        for i in 0..n {
            let Some(m) = (unsafe { self.method_at(cls, i) }) else { continue };
            if unsafe { self.method_name(m) } == "GetTypeFromHandle"
                && unsafe { self.method_argc(m) } == 1
                && unsafe { self.method_is_static(m) }
            {
                *self.type_from_handle.lock().ok()? = Some(m as usize);
                crate::elog!("[il2cpp] Type.GetTypeFromHandle @ {:p}", m);
                return Some(m);
            }
        }
        crate::elog!("[il2cpp] Type.GetTypeFromHandle not found");
        None
    }
}

impl ScriptRuntime for Il2CppRuntime {
    fn backend(&self) -> Backend {
        Backend::Il2Cpp
    }

    unsafe fn root_domain(&self) -> Domain {
        Domain(unsafe { self.api.domain() })
    }

    unsafe fn attach_thread(&self, domain: Domain) {
        unsafe { self.api.attach(domain.raw()) };
    }

    unsafe fn image(&self, domain: Domain, assembly: &str) -> Option<Image> {
        if let Some(&p) = self.images.lock().ok()?.get(assembly) {
            return Some(Image(p as *mut c_void));
        }
        for asm in unsafe { self.api.assemblies(domain.raw()) } {
            let img = unsafe { self.api.image_of(asm) };
            if img.is_null() {
                continue;
            }
            let Some(mut nm) = (unsafe { self.str_at(img as *mut u8, self.l.i_name) }) else {
                continue;
            };
            if let Some(stem) = nm.strip_suffix(".dll") {
                nm = stem.to_string();
            }
            if nm.eq_ignore_ascii_case(assembly) {
                self.images.lock().ok()?.insert(assembly.to_string(), img as usize);
                return Some(Image(img));
            }
        }
        None
    }

    unsafe fn class(&self, image: Image, namespace: &str, name: &str) -> Option<Class> {
        unsafe { self.index_of(image.raw()) }?;
        let g = self.classes.lock().ok()?;
        let m = g.get(&(image.raw() as usize))?;
        let p = m.get(&(namespace.to_string(), name.to_string()))?;
        Some(Class(*p as *mut c_void))
    }

    unsafe fn class_name(&self, class: Class) -> String {
        unsafe { self.class_name_raw(class.raw() as *mut u8) }
    }

    unsafe fn field(&self, class: Class, name: &str) -> Option<Field> {
        let mut c = class.raw() as *mut u8;
        loop {
            let n = unsafe { self.field_count(c) };
            for i in 0..n {
                let Some(f) = (unsafe { self.field_at(c, i) }) else { continue };
                if unsafe { self.str_at(f, self.l.f_name) }.as_deref() == Some(name) {
                    return Some(Field(f as *mut c_void));
                }
            }
            match unsafe { self.parent(c) } {
                Some(p) if p != c => c = p,
                _ => return None,
            }
        }
    }

    unsafe fn field_offset(&self, field: Field) -> i32 {
        let v = unsafe { read::<i32>((field.raw() as *mut u8).add(self.l.f_offset)) }.unwrap_or(0);
        // Instance offsets are header-inclusive, 4-aligned, and small. Anything
        // else means the FieldInfo layout is wrong and every read downstream
        // would be garbage, so say so where it can be seen.
        if v < 0 || v > 0x10000 || v & 3 != 0 {
            crate::elog!("[il2cpp] suspicious field offset {:#x} for {:?}", v,
                unsafe { self.str_at(field.raw() as *mut u8, self.l.f_name) });
        }
        v
    }

    unsafe fn method(&self, class: Class, name: &str, argc: i32) -> Option<Method> {
        let mut c = class.raw() as *mut u8;
        loop {
            let n = unsafe { self.method_count(c) };
            for i in 0..n {
                let Some(m) = (unsafe { self.method_at(c, i) }) else { continue };
                if unsafe { self.method_name(m) } != name {
                    continue;
                }
                if argc < 0 || unsafe { self.method_argc(m) } == argc as u32 {
                    return Some(Method(m as *mut c_void));
                }
            }
            match unsafe { self.parent(c) } {
                Some(p) if p != c => c = p,
                _ => return None,
            }
        }
    }

    unsafe fn dump_fields(&self, class: Class) -> Vec<(String, i32)> {
        let mut out = Vec::new();
        let mut c = class.raw() as *mut u8;
        let mut guard = 0;
        loop {
            let n = unsafe { self.field_count(c) };
            for i in 0..n {
                let Some(f) = (unsafe { self.field_at(c, i) }) else { continue };
                let name = unsafe { self.str_at(f, self.l.f_name) }.unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                let off = unsafe { read::<i32>(f.add(self.l.f_offset)) }.unwrap_or(-1);
                out.push((name, off));
            }
            guard += 1;
            match unsafe { self.parent(c) } {
                Some(p) if p != c && guard < 32 => c = p,
                _ => break,
            }
        }
        out
    }

    unsafe fn dump_methods(&self, class: Class) -> Vec<(String, u32, bool, usize)> {
        let mut out = Vec::new();
        let mut c = class.raw() as *mut u8;
        let mut guard = 0;
        loop {
            let n = unsafe { self.method_count(c) };
            for i in 0..n {
                let Some(m) = (unsafe { self.method_at(c, i) }) else { continue };
                let name = unsafe { self.method_name(m) };
                if name.is_empty() {
                    continue;
                }
                let argc = unsafe { self.method_argc(m) };
                let is_static = unsafe { self.method_is_static(m) };
                let ptr = unsafe { read_ptr(m.add(self.l.m_ptr)) }.map(|p| p as usize).unwrap_or(0);
                out.push((name, argc, is_static, ptr));
            }
            guard += 1;
            match unsafe { self.parent(c) } {
                Some(p) if p != c && guard < 32 => c = p,
                _ => break,
            }
        }
        out
    }

    unsafe fn method_exact(&self, class: Class, name: &str, argc: u32) -> Option<Method> {
        let c = class.raw() as *mut u8;
        let n = unsafe { self.method_count(c) };
        for i in 0..n {
            let Some(m) = (unsafe { self.method_at(c, i) }) else { continue };
            if unsafe { self.method_is_generic(m) } {
                continue;
            }
            if unsafe { self.method_name(m) } == name && unsafe { self.method_argc(m) } == argc {
                return Some(Method(m as *mut c_void));
            }
        }
        None
    }

    unsafe fn native_ptr(&self, method: Method) -> Option<*mut c_void> {
        let p = unsafe { read_ptr((method.raw() as *mut u8).add(self.l.m_ptr)) }?;
        Some(p as *mut c_void)
    }

    unsafe fn invoke(
        &self,
        method: Method,
        obj: Object,
        args: &mut [*mut c_void],
    ) -> Result<Object, ()> {
        unsafe { self.api.invoke(method.raw(), obj.raw(), args) }.map(Object)
    }

    unsafe fn invoke_static(&self, method: Method, args: &mut [*mut c_void]) -> Option<Object> {
        unsafe { self.api.invoke(method.raw(), std::ptr::null_mut(), args) }
            .ok()
            .map(Object)
    }

    unsafe fn new_object(&self, _domain: Domain, class: Class) -> Option<Object> {
        let o = unsafe { self.api.object_new(class.raw()) };
        (!o.is_null()).then_some(Object(o))
    }

    unsafe fn new_string(&self, _domain: Domain, text: &str) -> Option<Object> {
        let s = unsafe { self.api.string_new(text) };
        (!s.is_null()).then_some(Object(s))
    }

    unsafe fn new_array(&self, element_class: Class, len: usize) -> Option<Object> {
        if !self.api.has_array_new() || len > 0x10000 {
            return None;
        }
        let a = unsafe { self.api.array_new(element_class.raw(), len) };
        if a.is_null() {
            return None;
        }
        let arr = Object(a);
        // Sanity: a real Il2CppArray reports exactly the length we asked for.
        // A decoyed export would return the wrong shape and get rejected here.
        if unsafe { crate::runtime::array_len(arr) } != len {
            crate::elog!("[il2cpp] new_array length mismatch -- rejecting");
            return None;
        }
        Some(arr)
    }

    unsafe fn string_to_rust(&self, obj: Object) -> Option<String> {
        let p = obj.raw() as *mut u8;
        let len: i32 = unsafe { read(p.add(self.s_len)) }?;
        if !(0..0x10000).contains(&len) {
            return None;
        }
        let mut u = Vec::with_capacity(len as usize);
        for i in 0..len as usize {
            u.push(unsafe { read::<u16>(p.add(self.s_chars + i * 2)) }?);
        }
        String::from_utf16(&u).ok()
    }

    /// typeof(class) without il2cpp_type_get_object: hand byval_arg to
    /// Type.GetTypeFromHandle, which takes a RuntimeTypeHandle wrapping it.
    unsafe fn type_object(&self, _domain: Domain, class: Class) -> Option<Object> {
        let m = unsafe { self.type_from_handle_method() }?;
        let handle = unsafe { (class.raw() as *mut u8).add(self.l.c_byval) } as *mut c_void;
        // Refuse to hand the runtime anything that does not look like an
        // Il2CppType: the element-type byte must be a real ECMA code.
        let bits: u32 = unsafe { read((handle as *const u8).add(8)) }?;
        let elem = (bits >> 16) & 0xFF;
        if !(1..=0x20).contains(&elem) {
            crate::elog!("[il2cpp] type_object: byval_arg at {:#x} has element {:#x}, refusing",
                self.l.c_byval, elem);
            return None;
        }
        let mut slot = handle;
        let mut args = [&mut slot as *mut *mut c_void as *mut c_void];
        let o = unsafe { self.api.invoke(m as *mut c_void, std::ptr::null_mut(), &mut args) }.ok()?;
        (!o.is_null()).then_some(Object(o))
    }

    unsafe fn static_field_ptr(&self, _domain: Domain, _class: Class, _field: Field) -> *mut c_void {
        // Il2CppClass.static_fields is not calibrated: nothing in the trainer
        // reads a static field yet. Add it to il2cpp_abi when something does.
        crate::elog!("[il2cpp] static_field_ptr is not implemented on this backend");
        std::ptr::null_mut()
    }
}

/// Probe Il2CppString's layout with a string whose contents we chose.
unsafe fn probe_string_layout(api: &Api) -> Option<(usize, usize)> {
    const PROBE: &str = "il2cppProbe";
    let s = unsafe { api.string_new(PROBE) };
    if s.is_null() {
        return None;
    }
    let p = s as *mut u8;
    let want: Vec<u16> = PROBE.encode_utf16().collect();
    for off in (0x08..0x30).step_by(4) {
        if unsafe { read::<i32>(p.add(off)) } != Some(want.len() as i32) {
            continue;
        }
        let chars = off + 4;
        let ok = want.iter().enumerate().all(|(i, &w)| {
            (unsafe { read::<u16>(p.add(chars + i * 2)) }) == Some(w)
        });
        if ok {
            crate::elog!("[abi] Il2CppString.length {:#x} .chars {:#x}", off, chars);
            return Some((off, chars));
        }
    }
    crate::elog!("[abi] could not probe Il2CppString layout");
    None
}

/// Backend probe. IL2CPP only -- the Mono path is gone.
pub unsafe fn detect() -> Option<Box<dyn ScriptRuntime>> {
    let api = unsafe { Api::load(Duration::from_secs(20)) }?;

    // Injection can land before il2cpp_init has finished. Wait for a live
    // domain and corlib rather than calibrating against half-built metadata.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let d = unsafe { api.domain() };
        let c = unsafe { api.corlib() };
        if !d.is_null() && !c.is_null() && unsafe { api.class_count(c) } > 32 {
            break;
        }
        if crate::control::aborted() {
            crate::elog!("[il2cpp] aborted while waiting for the runtime");
            return None;
        }
        if Instant::now() >= deadline {
            crate::elog!("[il2cpp] runtime never became ready (domain/corlib still empty)");
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Metadata reads must happen on an attached thread.
    unsafe { api.attach(api.domain()) };

    // String layout first: it is the anchor for FieldInfo.offset, and it
    // has to run before the region snapshot so the fresh allocation is
    // still visible to the reads.
    let Some((s_len, s_chars)) = (unsafe { probe_string_layout(&api) }) else {
        crate::elog!("[il2cpp] cannot continue without the string layout");
        return None;
    };
    let l = unsafe { il2cpp_abi::calibrate(&api, s_len, s_chars) }?;

    crate::elog!("[runtime] backend: IL2CPP");
    Some(Box::new(Il2CppRuntime {
        api,
        l,
        images: Mutex::new(HashMap::new()),
        classes: Mutex::new(HashMap::new()),
        type_from_handle: Mutex::new(None),
        s_len,
        s_chars,
    }))
}
