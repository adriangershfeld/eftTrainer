//! ScriptRuntime over the Mono embedding API. Thin adapter over mono::MonoApi.
//!
//! The IL2CPP sibling goes next to this file. Differences to expect:
//!   load         GetProcAddress on GameAssembly.dll; sig-scan if stripped
//!   image        assembly NAME, so the managed_dir join goes away
//!   native_ptr   read methodPointer from MethodInfo, no JIT step
//!   method_exact generic sharing changes the generic filter
//! Verify the export list against the real DLL first.

#![allow(dead_code)]

use std::ffi::c_void;

use crate::mono::{MonoApi, MonoClass, MonoClassField, MonoDomain, MonoMethod, MonoObject};
use crate::runtime::{Backend, Class, Domain, Field, Image, Method, Object, ScriptRuntime};

pub struct MonoRuntime {
    api: MonoApi,
    /// Mono addresses assemblies by path, the trait by name. IL2CPP has no
    /// equivalent field.
    managed_dir: String,
}

impl MonoRuntime {
    /// None means no Mono in this process, i.e. an IL2CPP build.
    pub unsafe fn load(managed_dir: &str, wait: std::time::Duration) -> Option<Self> {
        let api = unsafe { MonoApi::load_with_retry(wait) }?;
        Some(Self { api, managed_dir: managed_dir.to_owned() })
    }
}

#[inline]
fn class_raw(c: Class) -> *mut MonoClass {
    c.0 as *mut MonoClass
}
#[inline]
fn method_raw(m: Method) -> *mut MonoMethod {
    m.0 as *mut MonoMethod
}
#[inline]
fn field_raw(f: Field) -> *mut MonoClassField {
    f.0 as *mut MonoClassField
}
#[inline]
fn domain_raw(d: Domain) -> *mut MonoDomain {
    d.0 as *mut MonoDomain
}
#[inline]
fn obj_raw(o: Object) -> *mut MonoObject {
    o.0 as *mut MonoObject
}

impl ScriptRuntime for MonoRuntime {
    fn backend(&self) -> Backend {
        Backend::Mono
    }

    unsafe fn root_domain(&self) -> Domain {
        Domain(unsafe { self.api.root_domain() } as *mut c_void)
    }

    unsafe fn attach_thread(&self, domain: Domain) {
        unsafe { self.api.attach_thread(domain_raw(domain)) }
    }

    unsafe fn image(&self, domain: Domain, assembly: &str) -> Option<Image> {
        let path = format!("{}\\{}.dll", self.managed_dir, assembly);
        let asm = unsafe { self.api.open_assembly(domain_raw(domain), &path) }?;
        let img = unsafe { self.api.image(asm) };
        (!img.is_null()).then(|| Image(img as *mut c_void))
    }

    unsafe fn class(&self, image: Image, namespace: &str, name: &str) -> Option<Class> {
        let k = unsafe { self.api.class(image.0 as *mut _, namespace, name) }?;
        Some(Class(k as *mut c_void))
    }

    unsafe fn class_name(&self, class: Class) -> String {
        unsafe { self.api.class_name(class_raw(class)) }
    }

    unsafe fn field(&self, class: Class, name: &str) -> Option<Field> {
        let f = unsafe { self.api.field(class_raw(class), name) }?;
        Some(Field(f as *mut c_void))
    }

    unsafe fn field_offset(&self, field: Field) -> i32 {
        unsafe { self.api.field_offset(field_raw(field)) }
    }

    unsafe fn method(&self, class: Class, name: &str, argc: i32) -> Option<Method> {
        let m = unsafe { self.api.method(class_raw(class), name, argc) }?;
        Some(Method(m as *mut c_void))
    }

    unsafe fn method_exact(&self, class: Class, name: &str, argc: u32) -> Option<Method> {
        let m = unsafe { self.api.find_method_exact(class_raw(class), name, argc) }?;
        Some(Method(m as *mut c_void))
    }

    unsafe fn native_ptr(&self, method: Method) -> Option<*mut c_void> {
        // Forces JIT if the method has never run.
        unsafe { self.api.compile(method_raw(method)) }
    }

    unsafe fn invoke(
        &self,
        method: Method,
        obj: Object,
        args: &mut [*mut c_void],
    ) -> Result<Object, ()> {
        unsafe { self.api.invoke(method_raw(method), obj_raw(obj), args) }
            .map(|p| Object(p as *mut c_void))
    }

    unsafe fn invoke_static(&self, method: Method, args: &mut [*mut c_void]) -> Option<Object> {
        unsafe { self.api.invoke_static(method_raw(method), args) }
            .map(|p| Object(p as *mut c_void))
    }

    unsafe fn new_object(&self, domain: Domain, class: Class) -> Option<Object> {
        let o = unsafe { self.api.object_new(domain_raw(domain), class_raw(class)) }?;
        Some(Object(o as *mut c_void))
    }

    unsafe fn new_string(&self, domain: Domain, text: &str) -> Option<Object> {
        let s = unsafe { self.api.new_string(domain_raw(domain), text) }?;
        Some(Object(s as *mut c_void))
    }

    unsafe fn string_to_rust(&self, obj: Object) -> Option<String> {
        unsafe { self.api.string_to_rust(obj_raw(obj)) }
    }

    unsafe fn type_object(&self, domain: Domain, class: Class) -> Option<Object> {
        let ty = unsafe { self.api.class_type(class_raw(class)) };
        let o = unsafe { self.api.type_object(domain_raw(domain), ty) }?;
        Some(Object(o as *mut c_void))
    }

    unsafe fn static_field_ptr(&self, domain: Domain, class: Class, field: Field) -> *mut c_void {
        unsafe { self.api.static_field_ptr(domain_raw(domain), class_raw(class), field_raw(field)) }
    }
}

/// Backend probe. Mono first because that is what EFU runs; the IL2CPP arm is
/// where a 1.0-or-later build will land once that implementation exists.
pub unsafe fn detect(managed_dir: &str) -> Option<Box<dyn ScriptRuntime>> {
    if let Some(rt) = unsafe { MonoRuntime::load(managed_dir, std::time::Duration::from_secs(15)) }
    {
        println!("[runtime] backend: Mono");
        return Some(Box::new(rt));
    }
    println!("[runtime] no Mono runtime in this process.");
    println!("[runtime] if GameAssembly.dll is present this is an IL2CPP build (EFT 1.0+),");
    println!("[runtime] which needs Il2CppRuntime -- not implemented yet.");
    None
}
