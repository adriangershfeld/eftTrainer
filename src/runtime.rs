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
