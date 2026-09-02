//! Every game- and engine-specific name, in one file. Nothing outside this
//! module should hold a string literal naming a class, field, method,
//! namespace or assembly. A patch breaks this file and nowhere else.
//!
//! `unity` = Unity's names: stable across versions and backends, free to port.
//! `eft` = BSG's names: these move.
//!
//! Rule for the eft tier: never bind to an obfuscated name. GClassNNNN and
//! friends renumber every patch. Resolve structurally instead.

#![allow(dead_code)]

/// Logical assembly names. No path, no `.dll` -- the runtime layer maps these
/// to a file under Managed\ (Mono) or an assembly name (IL2CPP).
pub mod asm {
    pub const GAME: &str = "Assembly-CSharp";
    pub const UNITY_CORE: &str = "UnityEngine.CoreModule";
    pub const UNITY_UI: &str = "UnityEngine.UI";
    pub const UNITY_UI_MODULE: &str = "UnityEngine.UIModule";
    /// Where UnityEngine.Font lives.
    pub const UNITY_TEXT_RENDERING: &str = "UnityEngine.TextRenderingModule";
}

pub mod ns {
    pub const UNITY: &str = "UnityEngine";
    pub const UNITY_UI: &str = "UnityEngine.UI";
    pub const EFT: &str = "EFT";
    /// PhysicalBase and Stamina are declared with no namespace block, i.e.
    /// the global namespace. Not an oversight, verified in the dnSpy export.
    pub const GLOBAL: &str = "";
}

/// A class addressed as (assembly, namespace, name).
#[derive(Copy, Clone, Debug)]
pub struct ClassRef {
    pub assembly: &'static str,
    pub namespace: &'static str,
    pub name: &'static str,
}

const fn cls(assembly: &'static str, namespace: &'static str, name: &'static str) -> ClassRef {
    ClassRef { assembly, namespace, name }
}

// ── ENGINE tier: Unity's own names ────────────────────────────────────────────

pub mod unity {
    use super::{asm, cls, ns, ClassRef};

    pub const GAME_OBJECT: ClassRef = cls(asm::UNITY_CORE, ns::UNITY, "GameObject");
    pub const OBJECT: ClassRef = cls(asm::UNITY_CORE, ns::UNITY, "Object");
    pub const TRANSFORM: ClassRef = cls(asm::UNITY_CORE, ns::UNITY, "Transform");
    pub const RECT_TRANSFORM: ClassRef = cls(asm::UNITY_CORE, ns::UNITY, "RectTransform");
    pub const CANVAS: ClassRef = cls(asm::UNITY_UI_MODULE, ns::UNITY, "Canvas");
    pub const GRAPHIC: ClassRef = cls(asm::UNITY_UI, ns::UNITY_UI, "Graphic");
    pub const TEXT: ClassRef = cls(asm::UNITY_UI, ns::UNITY_UI, "Text");
    pub const IMAGE: ClassRef = cls(asm::UNITY_UI, ns::UNITY_UI, "Image");
    pub const FONT: ClassRef = cls(asm::UNITY_TEXT_RENDERING, ns::UNITY, "Font");
    pub const RESOURCES: ClassRef = cls(asm::UNITY_CORE, ns::UNITY, "Resources");

    /// Static (Type, string). Chosen over Font.CreateDynamicFontFromOSFont,
    /// whose (string,int) and (string[],int) overloads both take 2 args.
    pub const GET_BUILTIN_RESOURCE: &str = "GetBuiltinResource";
    pub const SET_FONT: &str = "set_font";

    /// 2022 ships LegacyRuntime.ttf, older builds Arial.ttf.
    pub const BUILTIN_FONTS: &[&str] = &["LegacyRuntime.ttf", "Arial.ttf"];

    // GameObject / Object
    pub const CTOR: &str = ".ctor";
    pub const ADD_COMPONENT: &str = "AddComponent";
    pub const GET_COMPONENT: &str = "GetComponent";
    pub const SET_ACTIVE: &str = "SetActive";
    pub const DONT_DESTROY_ON_LOAD: &str = "DontDestroyOnLoad";
    /// Static `Object.Destroy(Object)`. The `(Object, float)` overload differs
    /// in arity, so an exact-1 lookup lands on the right one.
    pub const DESTROY: &str = "Destroy";
    /// The non-generic `FindObjectOfType(Type)` overload. The generic
    /// `FindObjectOfType<T>(bool)` also takes one argument, which is exactly
    /// the arity collision that must be resolved with `method_exact`.
    pub const FIND_OBJECT_OF_TYPE: &str = "FindObjectOfType";

    // Transform / RectTransform
    pub const SET_PARENT: &str = "SetParent";
    pub const SET_ANCHOR_MIN: &str = "set_anchorMin";
    pub const SET_ANCHOR_MAX: &str = "set_anchorMax";
    pub const SET_ANCHORED_POSITION: &str = "set_anchoredPosition";
    pub const SET_SIZE_DELTA: &str = "set_sizeDelta";
    pub const SET_OFFSET_MIN: &str = "set_offsetMin";
    pub const SET_OFFSET_MAX: &str = "set_offsetMax";
    pub const SET_PIVOT: &str = "set_pivot";

    // Canvas
    pub const SET_RENDER_MODE: &str = "set_renderMode";
    pub const SET_SORTING_ORDER: &str = "set_sortingOrder";

    // Graphic (base of Image and Text)
    pub const SET_COLOR: &str = "set_color";
    pub const SET_RAYCAST_TARGET: &str = "set_raycastTarget";

    // Text
    pub const SET_TEXT: &str = "set_text";
    pub const SET_FONT_SIZE: &str = "set_fontSize";
    pub const SET_ALIGNMENT: &str = "set_alignment";
    pub const SET_HORIZONTAL_OVERFLOW: &str = "set_horizontalOverflow";
    pub const SET_VERTICAL_OVERFLOW: &str = "set_verticalOverflow";

    /// UnityEngine.RenderMode
    pub const RENDER_MODE_SCREEN_SPACE_OVERLAY: i32 = 0;
    /// UnityEngine.TextAnchor
    pub const ANCHOR_UPPER_LEFT: i32 = 0;
    pub const ANCHOR_MIDDLE_LEFT: i32 = 3;
    pub const ANCHOR_MIDDLE_CENTER: i32 = 4;
    pub const ANCHOR_MIDDLE_RIGHT: i32 = 5;
    /// HorizontalWrapMode / VerticalWrapMode
    pub const WRAP_TRUNCATE: i32 = 0;
    pub const WRAP_OVERFLOW: i32 = 1;
}

// ── GAME tier: BSG's names. These are the ones that move. ─────────────────────

pub mod eft {
    use super::{asm, cls, ns, ClassRef};

    pub const GAME_WORLD: ClassRef = cls(asm::GAME, ns::EFT, "GameWorld");
    pub const PLAYER: ClassRef = cls(asm::GAME, ns::EFT, "Player");
    pub const PHYSICAL_BASE: ClassRef = cls(asm::GAME, ns::GLOBAL, "PhysicalBase");
    pub const STAMINA: ClassRef = cls(asm::GAME, ns::GLOBAL, "Stamina");

    /// Per-frame main-thread driver, in preference order. Resolves to Update
    /// on EFU 0.16.9.5; LateUpdate is not a 0-arg method on GameWorld.
    ///
    /// Camera.get_main is not on this list on purpose: its only callers left
    /// in Assembly-CSharp are dead asset-store demo code, so it hooks cleanly
    /// and never fires.
    pub const DRIVER_METHODS: &[&str] = &["LateUpdate", "Update", "DoWorldTick"];
}

// ── Field chains ──────────────────────────────────────────────────────────────

/// One field, and the key it is looked up under everywhere downstream.
pub struct FieldSpec {
    pub key: &'static str,
    pub class: ClassRef,
    pub field: &'static str,
}

/// Verified live on EFU 0.16.9.5.40743. Real names the whole way down, no
/// obfuscated link, which is why this route is worth keeping over a shorter
/// one through a GClass.
pub const STAMINA_CHAIN: &[FieldSpec] = &[
    FieldSpec { key: "GameWorld.MainPlayer", class: eft::GAME_WORLD, field: "MainPlayer" },
    FieldSpec { key: "Player.Physical", class: eft::PLAYER, field: "Physical" },
    FieldSpec { key: "PhysicalBase.Stamina", class: eft::PHYSICAL_BASE, field: "Stamina" },
    FieldSpec { key: "Stamina.Current", class: eft::STAMINA, field: "Current" },
];

pub mod key {
    pub const MAIN_PLAYER: &str = "GameWorld.MainPlayer";
    pub const PHYSICAL: &str = "Player.Physical";
    pub const STAMINA: &str = "PhysicalBase.Stamina";
    pub const STAMINA_CURRENT: &str = "Stamina.Current";
}

// ── Resolution ────────────────────────────────────────────────────────────────

use crate::runtime::{Class, Domain, Method, Object, ScriptRuntime};
use std::collections::HashMap;

/// assembly -> image -> class. Both backends cache assembly opens, so no
/// image cache is needed here.
pub unsafe fn find_class(
    rt: &dyn ScriptRuntime,
    domain: Domain,
    c: ClassRef,
) -> Option<Class> {
    let Some(image) = (unsafe { rt.image(domain, c.assembly) }) else {
        println!("[sym] MISSING assembly: {}", c.assembly);
        return None;
    };
    match unsafe { rt.class(image, c.namespace, c.name) } {
        Some(k) => Some(k),
        None => {
            let ns = if c.namespace.is_empty() { "<global>" } else { c.namespace };
            println!("[sym] MISSING class: {}::{}.{}", c.assembly, ns, c.name);
            None
        }
    }
}

/// `typeof(C)` as a managed Type object, for APIs that take one.
pub unsafe fn find_type_object(
    rt: &dyn ScriptRuntime,
    domain: Domain,
    c: ClassRef,
) -> Option<Object> {
    let class = unsafe { find_class(rt, domain, c) }?;
    match unsafe { rt.type_object(domain, class) } {
        Some(o) => Some(o),
        None => {
            println!("[sym] MISSING Type object for {}", c.name);
            None
        }
    }
}

/// Own-class-only, arity-checked, generic-definition-skipping method lookup.
pub unsafe fn find_method(
    rt: &dyn ScriptRuntime,
    class: Class,
    name: &str,
    argc: u32,
) -> Option<Method> {
    match unsafe { rt.method_exact(class, name, argc) } {
        Some(m) => Some(m),
        None => {
            println!("[sym] MISSING method: {}/{}", name, argc);
            None
        }
    }
}

/// Name-keyed offsets. Nothing downstream holds a literal offset.
#[derive(Default)]
pub struct Offsets {
    entries: HashMap<&'static str, i32>,
}

impl Offsets {
    pub fn get(&self, key: &str) -> Option<i32> {
        self.entries.get(key).copied()
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// A miss disables one feature rather than aborting the run.
pub unsafe fn resolve_fields(
    rt: &dyn ScriptRuntime,
    domain: Domain,
    specs: &[FieldSpec],
) -> Offsets {
    let mut out = Offsets::default();
    for spec in specs {
        let Some(class) = (unsafe { find_class(rt, domain, spec.class) }) else { continue };
        let Some(field) = (unsafe { rt.field(class, spec.field) }) else {
            println!("[sym] MISSING field: {}.{}", spec.class.name, spec.field);
            continue;
        };
        let offset = unsafe { rt.field_offset(field) };
        println!("[sym] {} -> offset {:#x}", spec.key, offset);
        out.entries.insert(spec.key, offset);
    }
    out
}
