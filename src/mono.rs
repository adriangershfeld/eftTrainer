//! Manual bindings to the Mono embedding API.
//!
//! No LLVM/libclang is installed on this machine, so these are hand-written
//! `extern "C"` declarations instead of bindgen output. Signatures come from
//! the public Mono embedding API (mono/metadata/*.h, mono/jit/jit.h), which
//! has been stable for years — the same API BepInEx, MelonLoader, and
//! basically every Unity-Mono internal tool binds against.
//!
//! We deliberately don't link against mono.lib. The game process already
//! has the Mono runtime loaded (mono-2.0-bdwgc.dll), so at inject time we
//! grab that module's handle and resolve every symbol with GetProcAddress.
//! That way we're calling into the *same* runtime instance the game uses,
//! not a second disconnected copy.
//!
//! NOTE: written without a compiler in the loop (no local build was run
//! while writing this) — build it and report back whatever errors show up.

#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use windows::core::PCSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

// ---------------------------------------------------------------------
// Opaque Mono handle types. We never look inside these — every Mono API
// treats them as opaque pointers, so a zero-sized marker struct plus a
// raw pointer is the right shape (the same trick the `windows` crate uses
// for HWND/HMODULE/etc.).
// ---------------------------------------------------------------------
macro_rules! opaque_handle {
    ($name:ident) => {
        #[repr(C)]
        pub struct $name {
            _opaque: [u8; 0],
        }
    };
}

opaque_handle!(MonoDomain);
opaque_handle!(MonoThread);
opaque_handle!(MonoAssembly);
opaque_handle!(MonoImage);
opaque_handle!(MonoClass);
opaque_handle!(MonoClassField);
opaque_handle!(MonoMethod);
opaque_handle!(MonoObject);
opaque_handle!(MonoVTable);
opaque_handle!(MonoType);
opaque_handle!(MonoMethodSignature);

// ---------------------------------------------------------------------
// Raw function pointer types, one per symbol pulled out of
// mono-2.0-bdwgc.dll. Calling convention: Mono is a portable C library
// built with the platform default convention, which is cdecl
// (`extern "C"` in Rust) — none of these are declared __stdcall, and on
// x64 the distinction is moot anyway (there's only one native ABI).
// ---------------------------------------------------------------------
type FnGetRootDomain = unsafe extern "C" fn() -> *mut MonoDomain;
type FnThreadAttach = unsafe extern "C" fn(domain: *mut MonoDomain) -> *mut MonoThread;
type FnDomainGet = unsafe extern "C" fn() -> *mut MonoDomain;
type FnDomainAssemblyOpen =
    unsafe extern "C" fn(domain: *mut MonoDomain, name: *const c_char) -> *mut MonoAssembly;
type FnAssemblyGetImage = unsafe extern "C" fn(assembly: *mut MonoAssembly) -> *mut MonoImage;
type FnClassFromName = unsafe extern "C" fn(
    image: *mut MonoImage,
    name_space: *const c_char,
    name: *const c_char,
) -> *mut MonoClass;
type FnClassGetParent = unsafe extern "C" fn(klass: *mut MonoClass) -> *mut MonoClass;
type FnClassGetName = unsafe extern "C" fn(klass: *mut MonoClass) -> *const c_char;
type FnClassGetNamespace = unsafe extern "C" fn(klass: *mut MonoClass) -> *const c_char;
type FnClassGetFieldFromName =
    unsafe extern "C" fn(klass: *mut MonoClass, name: *const c_char) -> *mut MonoClassField;
type FnFieldGetOffset = unsafe extern "C" fn(field: *mut MonoClassField) -> i32;
type FnFieldGetName = unsafe extern "C" fn(field: *mut MonoClassField) -> *const c_char;
type FnClassGetMethodFromName = unsafe extern "C" fn(
    klass: *mut MonoClass,
    name: *const c_char,
    param_count: i32,
) -> *mut MonoMethod;
type FnMethodGetName = unsafe extern "C" fn(method: *mut MonoMethod) -> *const c_char;
type FnCompileMethod = unsafe extern "C" fn(method: *mut MonoMethod) -> *mut c_void;
type FnClassVTable =
    unsafe extern "C" fn(domain: *mut MonoDomain, klass: *mut MonoClass) -> *mut MonoVTable;
type FnVTableGetStaticFieldData = unsafe extern "C" fn(vtable: *mut MonoVTable) -> *mut c_void;
type FnClassGetFields =
    unsafe extern "C" fn(klass: *mut MonoClass, iter: *mut *mut c_void) -> *mut MonoClassField;
type FnClassGetMethods =
    unsafe extern "C" fn(klass: *mut MonoClass, iter: *mut *mut c_void) -> *mut MonoMethod;
type FnObjectGetClass = unsafe extern "C" fn(obj: *mut MonoObject) -> *mut MonoClass;
type FnClassGetType = unsafe extern "C" fn(klass: *mut MonoClass) -> *mut MonoType;
type FnTypeGetObject =
    unsafe extern "C" fn(domain: *mut MonoDomain, type_: *mut MonoType) -> *mut MonoObject;
type FnRuntimeInvoke = unsafe extern "C" fn(
    method: *mut MonoMethod,
    obj: *mut c_void,
    params: *mut *mut c_void,
    exc: *mut *mut MonoObject,
) -> *mut MonoObject;
type FnMethodGetGenericContainer = unsafe extern "C" fn(method: *mut MonoMethod) -> *mut c_void;
type FnMethodSignature = unsafe extern "C" fn(method: *mut MonoMethod) -> *mut MonoMethodSignature;
type FnSignatureGetParamCount = unsafe extern "C" fn(sig: *mut MonoMethodSignature) -> u32;
type FnStringNew =
    unsafe extern "C" fn(domain: *mut MonoDomain, text: *const c_char) -> *mut MonoObject;
type FnStringToUtf8 = unsafe extern "C" fn(string_obj: *mut MonoObject) -> *mut c_char;
type FnMonoFree = unsafe extern "C" fn(ptr: *mut c_void);

/// Every Mono embedding function this trainer touches, resolved once at
/// inject time via GetProcAddress against the game's already-loaded
/// mono-2.0-bdwgc.dll. If a symbol is missing, `load()` fails instead of
/// leaving a null pointer around to segfault on later.
pub struct MonoApi {
    get_root_domain: FnGetRootDomain,
    thread_attach: FnThreadAttach,
    domain_get: FnDomainGet,
    domain_assembly_open: FnDomainAssemblyOpen,
    assembly_get_image: FnAssemblyGetImage,
    class_from_name: FnClassFromName,
    class_get_parent: FnClassGetParent,
    class_get_name: FnClassGetName,
    class_get_namespace: FnClassGetNamespace,
    class_get_field_from_name: FnClassGetFieldFromName,
    field_get_offset: FnFieldGetOffset,
    field_get_name: FnFieldGetName,
    class_get_method_from_name: FnClassGetMethodFromName,
    method_get_name: FnMethodGetName,
    compile_method: FnCompileMethod,
    class_vtable: FnClassVTable,
    vtable_get_static_field_data: FnVTableGetStaticFieldData,
    class_get_fields: FnClassGetFields,
    class_get_methods: FnClassGetMethods,
    object_get_class: FnObjectGetClass,
    class_get_type: FnClassGetType,
    type_get_object: FnTypeGetObject,
    runtime_invoke: FnRuntimeInvoke,
    method_get_generic_container: FnMethodGetGenericContainer,
    method_signature: FnMethodSignature,
    signature_get_param_count: FnSignatureGetParamCount,
    string_new: FnStringNew,
    string_to_utf8: FnStringToUtf8,
    mono_free: FnMonoFree,
}

/// Pulls one symbol out of `module` and reinterprets it as `T`. `T` must be
/// exactly the function pointer type the symbol actually has — GetProcAddress
/// gives us no way to check that, which is the whole reason these are
/// hand-written instead of bindgen-checked. transmute_copy (rather than
/// transmute) sidesteps the fact that FARPROC and our Fn* aliases aren't
/// technically the same Rust type even though they're both one pointer wide.
unsafe fn load_symbol<T: Copy>(module: HMODULE, name: &str) -> Option<T> {
    let cname = CString::new(name).ok()?;
    let addr = unsafe { GetProcAddress(module, PCSTR(cname.as_ptr() as *const u8)) }?;
    Some(unsafe { std::mem::transmute_copy::<_, T>(&addr) })
}

impl MonoApi {
    /// Finds the Mono runtime already loaded in this process and resolves
    /// every symbol we need. Tries both DLL names Unity has shipped Mono
    /// under.
    pub unsafe fn load() -> Option<Self> {
        let module = unsafe { GetModuleHandleA(PCSTR(b"mono-2.0-bdwgc.dll\0".as_ptr())) }
            .or_else(|_| unsafe { GetModuleHandleA(PCSTR(b"mono.dll\0".as_ptr())) })
            .ok()?;

        macro_rules! sym {
            ($name:literal) => {
                unsafe { load_symbol(module, $name) }?
            };
        }

        Some(Self {
            get_root_domain: sym!("mono_get_root_domain"),
            thread_attach: sym!("mono_thread_attach"),
            domain_get: sym!("mono_domain_get"),
            domain_assembly_open: sym!("mono_domain_assembly_open"),
            assembly_get_image: sym!("mono_assembly_get_image"),
            class_from_name: sym!("mono_class_from_name"),
            class_get_parent: sym!("mono_class_get_parent"),
            class_get_name: sym!("mono_class_get_name"),
            class_get_namespace: sym!("mono_class_get_namespace"),
            class_get_field_from_name: sym!("mono_class_get_field_from_name"),
            field_get_offset: sym!("mono_field_get_offset"),
            field_get_name: sym!("mono_field_get_name"),
            class_get_method_from_name: sym!("mono_class_get_method_from_name"),
            method_get_name: sym!("mono_method_get_name"),
            compile_method: sym!("mono_compile_method"),
            class_vtable: sym!("mono_class_vtable"),
            vtable_get_static_field_data: sym!("mono_vtable_get_static_field_data"),
            class_get_fields: sym!("mono_class_get_fields"),
            class_get_methods: sym!("mono_class_get_methods"),
            object_get_class: sym!("mono_object_get_class"),
            class_get_type: sym!("mono_class_get_type"),
            type_get_object: sym!("mono_type_get_object"),
            runtime_invoke: sym!("mono_runtime_invoke"),
            method_get_generic_container: sym!("mono_method_get_generic_container"),
            method_signature: sym!("mono_method_signature"),
            signature_get_param_count: sym!("mono_signature_get_param_count"),
            string_new: sym!("mono_string_new"),
            string_to_utf8: sym!("mono_string_to_utf8"),
            mono_free: sym!("mono_free"),
        })
    }

    /// Same as `load`, but retries for up to `max_wait` if
    /// mono-2.0-bdwgc.dll isn't loaded yet. Covers early injection --
    /// ExtremeInjector (or anything else) firing before the game has
    /// gotten around to loading the Mono runtime, which would otherwise
    /// make `load()` fail permanently instead of just needing a moment.
    pub unsafe fn load_with_retry(max_wait: std::time::Duration) -> Option<Self> {
        let start = std::time::Instant::now();
        loop {
            if let Some(api) = unsafe { Self::load() } {
                return Some(api);
            }
            if start.elapsed() >= max_wait {
                println!("[mono] gave up waiting for mono-2.0-bdwgc.dll after {:?}", max_wait);
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }

    // -- thin wrappers ----------------------------------------------------

    pub unsafe fn root_domain(&self) -> *mut MonoDomain {
        unsafe { (self.get_root_domain)() }
    }

    /// Every Mono API call from a thread Mono didn't create needs the
    /// thread attached first (this one, from DllMain's worker thread).
    /// Safe to call more than once.
    pub unsafe fn attach_thread(&self, domain: *mut MonoDomain) {
        unsafe { (self.thread_attach)(domain) };
    }

    pub unsafe fn open_assembly(
        &self,
        domain: *mut MonoDomain,
        path: &str,
    ) -> Option<*mut MonoAssembly> {
        let cpath = CString::new(path).ok()?;
        let asm = unsafe { (self.domain_assembly_open)(domain, cpath.as_ptr()) };
        (!asm.is_null()).then_some(asm)
    }

    pub unsafe fn image(&self, assembly: *mut MonoAssembly) -> *mut MonoImage {
        unsafe { (self.assembly_get_image)(assembly) }
    }

    pub unsafe fn class(
        &self,
        image: *mut MonoImage,
        namespace: &str,
        name: &str,
    ) -> Option<*mut MonoClass> {
        let cns = CString::new(namespace).ok()?;
        let cname = CString::new(name).ok()?;
        let klass = unsafe { (self.class_from_name)(image, cns.as_ptr(), cname.as_ptr()) };
        (!klass.is_null()).then_some(klass)
    }

    /// Walks up the base-class chain looking for `field`, since
    /// mono_class_get_field_from_name only checks `klass` itself on some
    /// Mono versions and a lot of what we want (health, movement state)
    /// lives on a base class.
    pub unsafe fn field(&self, klass: *mut MonoClass, name: &str) -> Option<*mut MonoClassField> {
        let cname = CString::new(name).ok()?;
        let mut current = klass;
        loop {
            if current.is_null() {
                return None;
            }
            let field = unsafe { (self.class_get_field_from_name)(current, cname.as_ptr()) };
            if !field.is_null() {
                return Some(field);
            }
            current = unsafe { (self.class_get_parent)(current) };
        }
    }

    pub unsafe fn field_offset(&self, field: *mut MonoClassField) -> i32 {
        unsafe { (self.field_get_offset)(field) }
    }

    /// Same base-class walk as `field`, for methods.
    pub unsafe fn method(
        &self,
        klass: *mut MonoClass,
        name: &str,
        param_count: i32,
    ) -> Option<*mut MonoMethod> {
        let cname = CString::new(name).ok()?;
        let mut current = klass;
        loop {
            if current.is_null() {
                return None;
            }
            let m = unsafe { (self.class_get_method_from_name)(current, cname.as_ptr(), param_count) };
            if !m.is_null() {
                return Some(m);
            }
            current = unsafe { (self.class_get_parent)(current) };
        }
    }

    /// Like `method`, but for disambiguating overloads that share a name
    /// *and* param count -- Unity's `Object.FindObjectOfType` is exactly
    /// this: a non-generic `(Type)` overload and a generic `<T>(bool)`
    /// overload both take one parameter. Walks every method actually
    /// named `name` via the `mono_class_get_methods` iterator, skips
    /// generic method definitions, and checks the real parameter count
    /// from the method's signature instead of trusting
    /// `mono_class_get_method_from_name`'s param-count filter to land on
    /// the right one out of a collision like that (it doesn't reliably --
    /// this is what crashed the game the first time around).
    pub unsafe fn find_method_exact(
        &self,
        klass: *mut MonoClass,
        name: &str,
        param_count: u32,
    ) -> Option<*mut MonoMethod> {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let method = unsafe { (self.class_get_methods)(klass, &mut iter) };
            if method.is_null() {
                return None;
            }
            if unsafe { cstr_to_string((self.method_get_name)(method)) } != name {
                continue;
            }
            let is_generic = !unsafe { (self.method_get_generic_container)(method) }.is_null();
            if is_generic {
                continue;
            }
            let sig = unsafe { (self.method_signature)(method) };
            if sig.is_null() {
                continue;
            }
            if unsafe { (self.signature_get_param_count)(sig) } == param_count {
                return Some(method);
            }
        }
    }

    /// Force-JITs `method` if it hasn't run yet and returns the real native
    /// code pointer — this is what a trampoline hook actually hooks, not
    /// some Mono-internal thunk.
    pub unsafe fn compile(&self, method: *mut MonoMethod) -> Option<*mut c_void> {
        let ptr = unsafe { (self.compile_method)(method) };
        (!ptr.is_null()).then_some(ptr)
    }

    /// Address of a *static* field's storage. Static fields don't live at
    /// `instance_ptr + offset` like instance fields do — they live in the
    /// class's vtable static-data block, so this applies the field's
    /// offset to vtable_get_static_field_data(vtable), not to any object
    /// pointer.
    pub unsafe fn static_field_ptr(
        &self,
        domain: *mut MonoDomain,
        klass: *mut MonoClass,
        field: *mut MonoClassField,
    ) -> *mut c_void {
        let vtable = unsafe { (self.class_vtable)(domain, klass) };
        let base = unsafe { (self.vtable_get_static_field_data)(vtable) };
        let offset = unsafe { self.field_offset(field) };
        unsafe { base.byte_add(offset as usize) }
    }

    pub unsafe fn class_name(&self, klass: *mut MonoClass) -> String {
        unsafe { cstr_to_string((self.class_get_name)(klass)) }
    }

    pub unsafe fn class_namespace(&self, klass: *mut MonoClass) -> String {
        unsafe { cstr_to_string((self.class_get_namespace)(klass)) }
    }

    /// Builds a real managed System.String from Rust text -- e.g. to pass
    /// as an argument to an invoked method.
    pub unsafe fn new_string(&self, domain: *mut MonoDomain, text: &str) -> Option<*mut MonoObject> {
        let ctext = CString::new(text).ok()?;
        let s = unsafe { (self.string_new)(domain, ctext.as_ptr()) };
        (!s.is_null()).then_some(s)
    }

    /// Reads a managed System.String object back into a Rust String.
    /// `mono_string_to_utf8` allocates with Mono's own allocator, so the
    /// buffer is freed via `mono_free` before returning -- otherwise this
    /// would leak on every call.
    pub unsafe fn string_to_rust(&self, string_obj: *mut MonoObject) -> Option<String> {
        let raw = unsafe { (self.string_to_utf8)(string_obj) };
        if raw.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned();
        unsafe { (self.mono_free)(raw as *mut c_void) };
        Some(s)
    }

    // -- reflection / invoke, for the FindObjectOfType route below --------

    pub unsafe fn class_type(&self, klass: *mut MonoClass) -> *mut MonoType {
        unsafe { (self.class_get_type)(klass) }
    }

    /// `typeof(klass)` as a real managed System.Type object, suitable as an
    /// argument to a method that takes `Type`.
    pub unsafe fn type_object(&self, domain: *mut MonoDomain, ty: *mut MonoType) -> Option<*mut MonoObject> {
        let obj = unsafe { (self.type_get_object)(domain, ty) };
        (!obj.is_null()).then_some(obj)
    }

    /// Calls a static method. `args` holds one raw pointer per parameter —
    /// a reference-type argument (like a Type object) goes in as-is; a
    /// value-type argument would need boxing first, which nothing here
    /// currently needs. Returns None both on a null result *and* on a
    /// managed exception (logged, not propagated — an injected trainer
    /// crashing the game over a caught exception would be worse than a
    /// missing feature).
    pub unsafe fn invoke_static(&self, method: *mut MonoMethod, args: &mut [*mut c_void]) -> Option<*mut MonoObject> {
        let mut exc: *mut MonoObject = std::ptr::null_mut();
        let params_ptr = if args.is_empty() {
            std::ptr::null_mut()
        } else {
            args.as_mut_ptr()
        };
        let result = unsafe { (self.runtime_invoke)(method, std::ptr::null_mut(), params_ptr, &mut exc) };
        if !exc.is_null() {
            println!("[mono] invoke_static: managed exception raised, ignoring result");
            return None;
        }
        (!result.is_null()).then_some(result)
    }
}

unsafe fn cstr_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}

/// Reads a `T` out of a managed object at `object_ptr + offset`. Offsets
/// from `mono_field_get_offset` already account for the Mono object header,
/// so this is the correct, direct way to read an instance field once you
/// have the field's offset and the object's address — no further math.
/// `read_unaligned` because a field address computed this way isn't
/// guaranteed to satisfy Rust's normal alignment assumptions.
pub unsafe fn read_field<T: Copy>(object_ptr: *mut c_void, offset: i32) -> T {
    let ptr = unsafe { (object_ptr as *mut u8).offset(offset as isize) } as *mut T;
    unsafe { ptr.read_unaligned() }
}

// ---------------------------------------------------------------------
// The name-keyed resolution table. Everything downstream (reads, writes,
// hooks) looks a value up here by string key; nothing outside this file
// ever holds a literal offset or address.
// ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum ResolvedMember {
    FieldOffset(i32),
    MethodPtr(*mut c_void),
}

#[derive(Default)]
pub struct ResolverTable {
    entries: HashMap<&'static str, ResolvedMember>,
}

impl ResolverTable {
    pub fn field_offset(&self, key: &str) -> Option<i32> {
        match self.entries.get(key) {
            Some(ResolvedMember::FieldOffset(o)) => Some(*o),
            _ => None,
        }
    }

    pub fn method_ptr(&self, key: &str) -> Option<*mut c_void> {
        match self.entries.get(key) {
            Some(ResolvedMember::MethodPtr(p)) => Some(*p),
            _ => None,
        }
    }
}

/// One field this trainer wants, described by where it lives in the
/// managed assembly and the key it's looked up under everywhere else.
pub struct FieldSpec {
    pub key: &'static str,
    pub namespace: &'static str,
    pub class: &'static str,
    pub field: &'static str,
}

/// Same idea for a method we plan to hook or force-JIT.
pub struct MethodSpec {
    pub key: &'static str,
    pub namespace: &'static str,
    pub class: &'static str,
    pub method: &'static str,
    pub param_count: i32,
}

/// Pulled from dnSpy against Assembly-CSharp.dll (0.16.9.5.40743, EFU):
/// EFT.Player.Physical -> PhysicalBase.Stamina -> Stamina.Current, plus
/// EFT.GameWorld.MainPlayer to get from a GameWorld instance to the local
/// Player. PhysicalBase and Stamina both live in the *global* namespace
/// (no `namespace` block in their source), hence the empty-string
/// namespaces below.
pub const STAMINA_CHAIN_FIELDS: &[FieldSpec] = &[
    FieldSpec { key: "GameWorld.MainPlayer", namespace: "EFT", class: "GameWorld", field: "MainPlayer" },
    FieldSpec { key: "Player.Physical", namespace: "EFT", class: "Player", field: "Physical" },
    FieldSpec { key: "PhysicalBase.Stamina", namespace: "", class: "PhysicalBase", field: "Stamina" },
    FieldSpec { key: "Stamina.Current", namespace: "", class: "Stamina", field: "Current" },
];

/// Resolves every spec against `image`, logging each hit/miss to the
/// console. A miss doesn't abort the run — that key just stays absent from
/// the table and whatever feature needs it stays disabled, which is the
/// whole point of resolving by name instead of hardcoding offsets: a
/// renamed/removed member degrades one feature, not the DLL.
pub unsafe fn resolve(
    api: &MonoApi,
    domain: *mut MonoDomain,
    image: *mut MonoImage,
    fields: &[FieldSpec],
    methods: &[MethodSpec],
) -> ResolverTable {
    let _ = domain; // reserved for static-field specs once those're added
    let mut table = ResolverTable::default();

    for spec in fields {
        let Some(klass) = (unsafe { api.class(image, spec.namespace, spec.class) }) else {
            println!(
                "[mono] miss: class {}.{} not found (for field {})",
                spec.namespace, spec.class, spec.key
            );
            continue;
        };
        let Some(field) = (unsafe { api.field(klass, spec.field) }) else {
            println!(
                "[mono] miss: field {}.{}.{} not found",
                spec.namespace, spec.class, spec.field
            );
            continue;
        };
        let offset = unsafe { api.field_offset(field) };
        println!("[mono] resolved field {} -> offset {:#x}", spec.key, offset);
        table.entries.insert(spec.key, ResolvedMember::FieldOffset(offset));
    }

    for spec in methods {
        let Some(klass) = (unsafe { api.class(image, spec.namespace, spec.class) }) else {
            println!(
                "[mono] miss: class {}.{} not found (for method {})",
                spec.namespace, spec.class, spec.key
            );
            continue;
        };
        let Some(method) = (unsafe { api.method(klass, spec.method, spec.param_count) }) else {
            println!(
                "[mono] miss: method {}.{}.{} not found",
                spec.namespace, spec.class, spec.method
            );
            continue;
        };
        let Some(ptr) = (unsafe { api.compile(method) }) else {
            println!("[mono] miss: mono_compile_method returned null for {}", spec.key);
            continue;
        };
        println!("[mono] resolved method {} -> {:p}", spec.key, ptr);
        table.entries.insert(spec.key, ResolvedMember::MethodPtr(ptr));
    }

    table
}

/// Resolves `UnityEngine.Object.FindObjectOfType(Type)` specifically,
/// disambiguated from the generic `FindObjectOfType<T>(bool)` overload
/// (see `find_method_exact`). Done once at setup, not per-poll -- the
/// resolved method pointer doesn't change for the life of the process.
pub unsafe fn resolve_find_object_of_type(
    api: &MonoApi,
    unity_object_image: *mut MonoImage,
) -> Option<*mut MonoMethod> {
    let object_klass = unsafe { api.class(unity_object_image, "UnityEngine", "Object") }?;
    println!("[mono] found UnityEngine.Object class: {:p}", object_klass);
    let method = unsafe { api.find_method_exact(object_klass, "FindObjectOfType", 1) }?;
    println!("[mono] resolved non-generic FindObjectOfType(Type): {:p}", method);
    Some(method)
}

/// `typeof(class)` as a real managed Type object -- also resolved once at
/// setup and reused on every poll, since it's the same argument every
/// call.
pub unsafe fn resolve_type_object(
    api: &MonoApi,
    domain: *mut MonoDomain,
    image: *mut MonoImage,
    namespace: &str,
    class: &str,
) -> Option<*mut MonoObject> {
    let klass = unsafe { api.class(image, namespace, class) }?;
    let ty = unsafe { api.class_type(klass) };
    let obj = unsafe { api.type_object(domain, ty) };
    if obj.is_some() {
        println!("[mono] resolved Type object for {}.{}", namespace, class);
    }
    obj
}

/// Calls the already-resolved FindObjectOfType(Type) with an
/// already-resolved Type object. Returns None both on a real failure and
/// on the expected "not in a raid/hideout right now" case -- GameWorld
/// only exists once a raid or the hideout is loaded, so a null result at
/// the main menu is normal, not a bug.
pub unsafe fn find_object_of_type(
    api: &MonoApi,
    find_method: *mut MonoMethod,
    type_obj: *mut MonoObject,
) -> Option<*mut MonoObject> {
    let mut args = [type_obj as *mut c_void];
    unsafe { api.invoke_static(find_method, &mut args) }
}

/// Live readout: finds the current GameWorld (None outside a raid/hideout)
/// and walks MainPlayer -> Physical -> Stamina -> Current using offsets
/// already resolved into `table`. Cheap enough to call on a timer -- no
/// class/method resolution happens here, just one invoke against the
/// already-resolved `find_method`/`game_world_type_obj` and three
/// pointer-chases. Also works in the hideout: GameWorld is `abstract`, so
/// FindObjectOfType matches whatever concrete subclass is live (raids use
/// ClientLocalGameWorld; the hideout is the same player/movement/stamina
/// systems running under its own world instance), and stamina drains there
/// the same way it does in a raid even though the HUD doesn't show it.
pub unsafe fn read_current_stamina(
    api: &MonoApi,
    find_method: *mut MonoMethod,
    game_world_type_obj: *mut MonoObject,
    table: &ResolverTable,
) -> Option<f32> {
    let game_world = unsafe { find_object_of_type(api, find_method, game_world_type_obj) }?;

    let main_player_off = table.field_offset("GameWorld.MainPlayer")?;
    let physical_off = table.field_offset("Player.Physical")?;
    let stamina_off = table.field_offset("PhysicalBase.Stamina")?;
    let current_off = table.field_offset("Stamina.Current")?;

    let player_ptr: *mut c_void = unsafe { read_field(game_world as *mut c_void, main_player_off) };
    if player_ptr.is_null() {
        return None;
    }
    let physical_ptr: *mut c_void = unsafe { read_field(player_ptr, physical_off) };
    if physical_ptr.is_null() {
        return None;
    }
    let stamina_ptr: *mut c_void = unsafe { read_field(physical_ptr, stamina_off) };
    if stamina_ptr.is_null() {
        return None;
    }
    Some(unsafe { read_field(stamina_ptr, current_off) })
}

/// Sanity check for the next injection test: loads the API, gets the root
/// domain, attaches this thread, and opens Assembly-CSharp.dll. No
/// class/field names are guessed here — this only proves the pipeline
/// itself works against the live game. Returns (api, domain, image) on
/// success so real FieldSpec/MethodSpec resolution can follow once you've
/// pulled real names out of dnSpy.
pub unsafe fn smoke_test(assembly_csharp_path: &str) -> Option<(MonoApi, *mut MonoDomain, *mut MonoImage)> {
    let api = unsafe { MonoApi::load_with_retry(std::time::Duration::from_secs(15)) }?;
    println!("[mono] API loaded (26 symbols)");

    let domain = unsafe { api.root_domain() };
    if domain.is_null() {
        println!("[mono] root domain is null");
        return None;
    }
    println!("[mono] root domain: {:p}", domain);

    unsafe { api.attach_thread(domain) };
    println!("[mono] thread attached");

    let assembly = unsafe { api.open_assembly(domain, assembly_csharp_path) }?;
    println!("[mono] opened assembly: {}", assembly_csharp_path);

    let image = unsafe { api.image(assembly) };
    println!("[mono] got image: {:p}", image);

    Some((api, domain, image))
}
