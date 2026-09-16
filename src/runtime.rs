//! Backend-neutral scripting runtime. Mono today (EFU), IL2CPP for 1.0+.
//! Nothing above this module names a backend.
//!
//! Two differences absorbed here: assemblies are addressed by logical name,
//! not path; native_ptr means "address to hook" (Mono JITs, IL2CPP is AOT).

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::OnceLock;

// Newtypes so a Class can't be passed where an Image belongs, and so no call
// site re-acquires a *mut MonoClass and pins itself to one backend.

macro_rules! handle {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Copy, Clone, PartialEq, Eq, Debug)]
        #[repr(transparent)]
        pub struct $name(pub *mut c_void);

        impl $name {
            pub const NULL: Self = Self(std::ptr::null_mut());
            #[inline]
            pub fn is_null(self) -> bool { self.0.is_null() }
            #[inline]
            pub fn raw(self) -> *mut c_void { self.0 }
        }
    };
}

handle!(/// Application domain.
    Domain);
handle!(/// One loaded assembly's metadata image.
    Image);
handle!(/// A managed type.
    Class);
handle!(/// A managed method (MonoMethod / MethodInfo).
    Method);
handle!(/// A field descriptor, used to get an offset.
    Field);
handle!(/// A live managed object instance.
    Object);

// Metadata is built once at load and never moves. Object is deliberately not
// Send/Sync: managed object pointers are thread-bound and GC-owned.
unsafe impl Send for Domain {}
unsafe impl Sync for Domain {}
unsafe impl Send for Image {}
unsafe impl Sync for Image {}
unsafe impl Send for Class {}
unsafe impl Sync for Class {}
unsafe impl Send for Method {}
unsafe impl Sync for Method {}
unsafe impl Send for Field {}
unsafe impl Sync for Field {}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Backend {
    Mono,
    Il2Cpp,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Mono => "Mono",
            Backend::Il2Cpp => "IL2CPP",
        }
    }
}

// ── The abstraction ───────────────────────────────────────────────────────────

/// Every method here has a counterpart in both embedding APIs. If something
/// only expresses against one of them it does not belong in this trait.
pub trait ScriptRuntime: Send + Sync {
    fn backend(&self) -> Backend;

    unsafe fn root_domain(&self) -> Domain;

    /// Required before any other call from a thread the runtime did not
    /// create. Idempotent.
    unsafe fn attach_thread(&self, domain: Domain);

    /// Resolve an assembly by LOGICAL NAME, no extension and no path:
    /// "Assembly-CSharp", "UnityEngine.CoreModule", "UnityEngine.UI".
    unsafe fn image(&self, domain: Domain, assembly: &str) -> Option<Image>;

    unsafe fn class(&self, image: Image, namespace: &str, name: &str) -> Option<Class>;
    unsafe fn class_name(&self, class: Class) -> String;

    /// Walks the base-class chain, since plenty of what we want lives on a
    /// parent type.
    unsafe fn field(&self, class: Class, name: &str) -> Option<Field>;
    unsafe fn field_offset(&self, field: Field) -> i32;

    /// Name + argument count, walking base classes.
    unsafe fn method(&self, class: Class, name: &str, argc: i32) -> Option<Method>;

    /// Enumerate fields as (name, offset), walking base classes. For the HTTP
    /// introspection server. Bounded by the backend.
    unsafe fn dump_fields(&self, class: Class) -> Vec<(String, i32)>;

    /// Enumerate methods as (name, argc, is_static, method_ptr), walking base
    /// classes. method_ptr is the native address (0 if none).
    unsafe fn dump_methods(&self, class: Class) -> Vec<(String, u32, bool, usize)>;

    /// Own class only, skips generic definitions, checks the real param count
    /// from the signature. Disambiguates overloads that collide on arity.
    unsafe fn method_exact(&self, class: Class, name: &str, argc: u32) -> Option<Method>;

    /// The address to hook.
    unsafe fn native_ptr(&self, method: Method) -> Option<*mut c_void>;

    unsafe fn invoke(&self, method: Method, obj: Object, args: &mut [*mut c_void])
        -> Result<Object, ()>;
    unsafe fn invoke_static(&self, method: Method, args: &mut [*mut c_void]) -> Option<Object>;

    /// Allocates without running a constructor. Invoke `.ctor` yourself.
    unsafe fn new_object(&self, domain: Domain, class: Class) -> Option<Object>;
    unsafe fn new_string(&self, domain: Domain, text: &str) -> Option<Object>;
    unsafe fn string_to_rust(&self, obj: Object) -> Option<String>;

    /// A fresh managed `element_class[len]`, zero-initialised. None if the
    /// backend cannot allocate one (e.g. IL2CPP without a trusted array_new),
    /// so callers must have a no-array fallback.
    unsafe fn new_array(&self, element_class: Class, len: usize) -> Option<Object>;

    /// `typeof(class)` as a real managed System.Type, for APIs that take one.
    unsafe fn type_object(&self, domain: Domain, class: Class) -> Option<Object>;

    unsafe fn static_field_ptr(&self, domain: Domain, class: Class, field: Field) -> *mut c_void;
}

static RUNTIME: OnceLock<Box<dyn ScriptRuntime>> = OnceLock::new();

pub fn install(rt: Box<dyn ScriptRuntime>) -> bool {
    RUNTIME.set(rt).is_ok()
}

/// None until `install` has run.
pub fn get() -> Option<&'static dyn ScriptRuntime> {
    RUNTIME.get().map(|b| &**b)
}

/// Both backends report offsets including the object header, so this is the
/// whole of the arithmetic. Unaligned because the address carries no
/// alignment guarantee.
pub unsafe fn read_field<T: Copy>(object: Object, offset: i32) -> T {
    let ptr = unsafe { (object.0 as *mut u8).offset(offset as isize) } as *mut T;
    unsafe { ptr.read_unaligned() }
}

/// Write a value at a field offset. Same header-inclusive, unaligned rules as
/// read_field. Caller owns the safety: the object must be live and the offset
/// a real field of a compatible type.
pub unsafe fn write_field<T: Copy>(object: Object, offset: i32, val: T) {
    let ptr = unsafe { (object.0 as *mut u8).offset(offset as isize) } as *mut T;
    unsafe { ptr.write_unaligned(val) };
}

// ── Managed arrays ────────────────────────────────────────────────────────────
// IL2CPP System.Array on x64: klass+monitor (0x10), bounds (0x10), max_length
// (0x18), then the element vector (0x20). A reference-type array holds one
// pointer per element.

const ARRAY_COUNT_OFF: usize = 0x18;
const ARRAY_DATA_OFF: usize = 0x20;

/// Element count, or 0 for null. Caller should still sanity-cap the result:
/// a non-array pointer will read garbage here.
pub unsafe fn array_len(arr: Object) -> usize {
    if arr.is_null() {
        return 0;
    }
    unsafe { ((arr.0 as *const u8).add(ARRAY_COUNT_OFF) as *const usize).read_unaligned() }
}

pub unsafe fn array_get(arr: Object, i: usize) -> Object {
    let p = unsafe { (arr.0 as *const u8).add(ARRAY_DATA_OFF + i * 8) as *const usize };
    Object(unsafe { p.read_unaligned() } as *mut c_void)
}

/// Overwrites one reference slot. No GC write barrier: valid because the IL2CPP
/// GC is non-moving and the value written is kept rooted elsewhere.
pub unsafe fn array_set(arr: Object, i: usize, val: Object) {
    let p = unsafe { (arr.0 as *mut u8).add(ARRAY_DATA_OFF + i * 8) as *mut usize };
    unsafe { p.write_unaligned(val.0 as usize) };
}

// ── UnityEngine.Object liveness ───────────────────────────────────────────────
// Every UnityEngine.Object carries m_CachedPtr (the native C++ object) as its
// first field, at 0x10 on x64. Destroyed objects keep the managed shell but
// zero this pointer, so it is the one safe "is this still usable" test before
// invoking anything on a cached handle.
const UNITY_CACHED_PTR_OFF: usize = 0x10;

/// True only for a live UnityEngine.Object. Null, or a destroyed object whose
/// native side is gone, both read false. Pass only real managed pointers.
pub unsafe fn is_alive(obj: Object) -> bool {
    if obj.is_null() {
        return false;
    }
    let p = unsafe { (obj.0 as *const u8).add(UNITY_CACHED_PTR_OFF) as *const usize };
    (unsafe { p.read_unaligned() }) != 0
}

/// Unbox an `int` returned through runtime_invoke. Value-type returns come back
/// boxed: object header (0x10) then the value. None if null.
pub unsafe fn unbox_i32(boxed: Object) -> Option<i32> {
    if boxed.is_null() {
        return None;
    }
    let p = unsafe { (boxed.0 as *const u8).add(0x10) as *const i32 };
    Some(unsafe { p.read_unaligned() })
}

/// Unbox a single byte (a `bool`) at the boxed value slot. None if null.
pub unsafe fn unbox_bool(boxed: Object) -> Option<bool> {
    if boxed.is_null() {
        return None;
    }
    let p = unsafe { (boxed.0 as *const u8).add(0x10) };
    Some(unsafe { p.read_unaligned() } != 0)
}

/// Unbox a `UnityEngine.Vector3` (three f32) at the boxed value slot. This is
/// how a struct-returning method's result comes back through runtime_invoke.
/// None if null.
pub unsafe fn unbox_vec3(boxed: Object) -> Option<[f32; 3]> {
    if boxed.is_null() {
        return None;
    }
    let p = unsafe { (boxed.0 as *const u8).add(0x10) as *const [f32; 3] };
    Some(unsafe { p.read_unaligned() })
}

// ── System.Collections.Generic.List<T> ────────────────────────────────────────
// Layout on IL2CPP x64: _items (T[]) at 0x10, _size (int) at 0x18. Reading the
// backing array + used count avoids an enumerator invoke.
const LIST_ITEMS_OFF: usize = 0x10;
const LIST_SIZE_OFF: usize = 0x18;

/// The backing array of a List<T>. Null object if the list is null/empty.
pub unsafe fn list_items(list: Object) -> Object {
    if list.is_null() {
        return Object::NULL;
    }
    let p = unsafe { (list.0 as *const u8).add(LIST_ITEMS_OFF) as *const usize };
    Object(unsafe { p.read_unaligned() } as *mut c_void)
}

/// The used element count of a List<T> (its _size, not the backing capacity).
pub unsafe fn list_count(list: Object) -> usize {
    if list.is_null() {
        return 0;
    }
    let p = unsafe { (list.0 as *const u8).add(LIST_SIZE_OFF) as *const i32 };
    let n = unsafe { p.read_unaligned() };
    if n < 0 { 0 } else { n as usize }
}
