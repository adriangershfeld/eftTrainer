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
//! Scope: this file is the FFI and nothing else. Resolution policy lives in
//! symbols.rs, the backend-neutral interface in runtime.rs, and the adapter
//! between them in mono_runtime.rs, which is the only module that should ever
//! import this one.

#![allow(dead_code)]

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
type FnObjectNew =
    unsafe extern "C" fn(domain: *mut MonoDomain, klass: *mut MonoClass) -> *mut MonoObject;
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
    object_new: FnObjectNew,
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
            object_new: sym!("mono_object_new"),
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

    /// Allocates a new managed object of type `klass` WITHOUT calling any
    /// constructor. You must invoke `.ctor` yourself immediately after.
    pub unsafe fn object_new(&self, domain: *mut MonoDomain, klass: *mut MonoClass) -> Option<*mut MonoObject> {
        let obj = unsafe { (self.object_new)(domain, klass) };
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

    /// Calls an instance method. Returns Ok(ptr) — ptr may be null for void
    /// returns. Returns Err(()) on a managed exception.
    pub unsafe fn invoke(
        &self,
        method: *mut MonoMethod,
        obj: *mut MonoObject,
        args: &mut [*mut c_void],
    ) -> Result<*mut MonoObject, ()> {
        let mut exc: *mut MonoObject = std::ptr::null_mut();
        let params_ptr = if args.is_empty() {
            std::ptr::null_mut()
        } else {
            args.as_mut_ptr()
        };
        let result = unsafe {
            (self.runtime_invoke)(method, obj as *mut c_void, params_ptr, &mut exc)
        };
        if !exc.is_null() {
            println!("[mono] invoke: managed exception");
            return Err(());
        }
        Ok(result)
    }
}

unsafe fn cstr_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}

// Everything past this point -- the name-keyed ResolverTable, FieldSpec/
// MethodSpec, STAMINA_CHAIN_FIELDS, resolve(), the FindObjectOfType helpers,
// read_current_stamina() and smoke_test() -- was superseded by symbols.rs and
// runtime.rs and has been removed. Two parallel resolvers is how a future
// session ends up editing the one that isn't wired up. Git history has them.
//
// This file is now ONLY the raw Mono FFI. MonoRuntime in mono_runtime.rs is
// the only thing that should ever touch it.
