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

    // ── Rendering (chams) ─────────────────────────────────────────────────────
    pub const COMPONENT: ClassRef = cls(asm::UNITY_CORE, ns::UNITY, "Component");
    pub const RENDERER: ClassRef = cls(asm::UNITY_CORE, ns::UNITY, "Renderer");
    /// The character mesh type. Filtering the transform walk to this excludes
    /// transient effect renderers (particles, casings, decals) that otherwise
    /// flood the walk every frame.
    pub const SKINNED_MESH_RENDERER: ClassRef =
        cls(asm::UNITY_CORE, ns::UNITY, "SkinnedMeshRenderer");
    pub const MATERIAL: ClassRef = cls(asm::UNITY_CORE, ns::UNITY, "Material");
    pub const SHADER: ClassRef = cls(asm::UNITY_CORE, ns::UNITY, "Shader");
    pub const CAMERA: ClassRef = cls(asm::UNITY_CORE, ns::UNITY, "Camera");

    /// Camera.main (static) and WorldToScreenPoint(Vector3) -> Vector3 (boxed),
    /// the argc-1 overload. World-to-screen for the raid framework.
    pub const CAMERA_GET_MAIN: &str = "get_main";
    pub const CAMERA_WORLD_TO_SCREEN: &str = "WorldToScreenPoint";

    // Object.FindObjectsOfType(Type) -> Object[]. 1-arg overload; some Unity
    // versions add a (Type, bool) at arity 2, tried as a fallback.
    pub const FIND_OBJECTS_OF_TYPE: &str = "FindObjectsOfType";
    /// Object.Instantiate(Object) -> Object. The 1-arg overload; used to clone
    /// a live material instead of Material..ctor(Shader), whose overload set
    /// (Shader/Material/string) all collide at arity 1.
    pub const INSTANTIATE: &str = "Instantiate";

    // This build has no non-generic GetComponentsInChildren(Type, bool) -- only
    // the generic <T> overloads, which cannot be resolved by an arity walk. So
    // renderers are gathered by walking the transform tree with these, all
    // non-generic: Component.get_transform, Transform.get_childCount,
    // Transform.GetChild(int), and Component.GetComponent(Type).
    pub const GET_COMPONENTS_IN_CHILDREN: &str = "GetComponentsInChildren";
    pub const GET_TRANSFORM: &str = "get_transform";
    pub const GET_CHILD_COUNT: &str = "get_childCount";
    pub const GET_CHILD: &str = "GetChild";

    // Renderer
    pub const GET_MATERIALS: &str = "get_materials";
    pub const SET_MATERIALS: &str = "set_materials";
    pub const GET_SHARED_MATERIAL: &str = "get_sharedMaterial";
    /// Renderer.sharedMaterials: the shared originals, no per-instance clone.
    /// Captured on first touch so F5-off can restore exactly.
    pub const GET_SHARED_MATERIALS: &str = "get_sharedMaterials";
    pub const SET_SHARED_MATERIALS: &str = "set_sharedMaterials";
    pub const GET_ENABLED: &str = "get_enabled";

    /// UnityEngine.Object.name, for logging discovered shaders during tuning.
    pub const GET_NAME: &str = "get_name";
    /// Object.FindObjectsOfTypeAll(Type) -> Object[]: every loaded instance,
    /// assets included. Used once to enumerate loaded Shaders for rim tuning.
    pub const FIND_OBJECTS_OF_TYPE_ALL: &str = "FindObjectsOfTypeAll";

    // Material
    pub const MAT_SET_SHADER: &str = "set_shader";
    pub const MAT_GET_SHADER: &str = "get_shader";
    /// SetColor/SetInt/SetFloat: the (string, value) overloads. Verified on
    /// 46911 to precede the (int nameID, value) overloads in metadata, so an
    /// arity-2 exact lookup lands on the string form we pass a property name to.
    pub const MAT_SET_COLOR: &str = "SetColor";
    pub const MAT_SET_INT: &str = "SetInt";
    pub const MAT_SET_FLOAT: &str = "SetFloat";
    pub const MAT_ENABLE_KEYWORD: &str = "EnableKeyword";
    pub const MAT_SET_RENDER_QUEUE: &str = "set_renderQueue";
    /// Material.HasProperty(string). The (int) overload precedes it, so this
    /// is resolved by walking to the arity-1 string form via a name check.
    pub const MAT_HAS_PROPERTY: &str = "HasProperty";

    // Shader
    pub const SHADER_FIND: &str = "Find";

    /// UnityEngine.Rendering.CompareFunction, for _ZTest. Always = draw through
    /// everything; LessEqual / Greater split visible vs occluded for two-tone.
    pub const ZTEST_ALWAYS: i32 = 8;
    pub const ZTEST_LEQUAL: i32 = 4;
    pub const ZTEST_GREATER: i32 = 5;

    /// Shader-property names the cham material drives. Internal-Colored honours
    /// _Color/_ZTest/_ZWrite/_Cull; the rim set is only honoured by a rim-
    /// capable shader and is set best-effort (guarded by HasProperty).
    pub const PROP_COLOR: &str = "_Color";
    pub const PROP_ZTEST: &str = "_ZTest";
    pub const PROP_ZWRITE: &str = "_ZWrite";
    pub const PROP_CULL: &str = "_Cull";
    /// Blend factors. Internal-Colored declares _SrcBlend/_DstBlend as Float
    /// enums (UnityEngine.Rendering.BlendMode); driving them switches the
    /// material between opaque / additive / alpha styles without a new shader.
    pub const PROP_SRC_BLEND: &str = "_SrcBlend";
    pub const PROP_DST_BLEND: &str = "_DstBlend";

    /// UnityEngine.Rendering.BlendMode values we use.
    pub const BLEND_ZERO: i32 = 0;
    pub const BLEND_ONE: i32 = 1;
    pub const BLEND_SRC_ALPHA: i32 = 5;
    pub const BLEND_ONE_MINUS_SRC_ALPHA: i32 = 10;
    /// UnityEngine.Rendering.CullMode.Off -- draw both faces (see through the
    /// model to its back faces, which reads as depth on translucent styles).
    pub const CULL_OFF: i32 = 0;
    pub const CULL_BACK: i32 = 2;

    /// UnityEngine.Cursor: forced visible + unlocked while the menu is open, so
    /// the pointer can be seen and clicked (the game locks/hides it for
    /// mouselook otherwise). Static setters. CursorLockMode.None = 0.
    pub const CURSOR: ClassRef = cls(asm::UNITY_CORE, ns::UNITY, "Cursor");
    pub const CURSOR_SET_VISIBLE: &str = "set_visible";
    pub const CURSOR_SET_LOCK_STATE: &str = "set_lockState";
    pub const CURSOR_LOCK_NONE: i32 = 0;

    /// Rim/fresnel property names, in the spelling different shader families
    /// use. Applied only where HasProperty confirms the shader has them.
    pub const RIM_COLOR_PROPS: &[&str] = &["_RimColor", "_RimLightColor", "_FresnelColor", "_OutlineColor"];
    pub const RIM_POWER_PROPS: &[&str] = &["_RimPower", "_RimLightPower", "_FresnelPower", "_RimIntensity"];

    /// Rim-capable shaders to try before falling back to Internal-Colored.
    /// Whichever the game happens to ship wins; discovery is logged.
    pub const RIM_SHADER_CANDIDATES: &[&str] = &[
        "Custom/Rim",
        "Hidden/Rim",
        "Unlit/Rim",
    ];
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

    /// GameWorld collections of live players, tried in order. Real, readable
    /// field names (not GClass), so binding them is safe, and chams falls back
    /// to FindObjectsOfType(Player) if none resolve. Both are List<...Player>.
    pub const PLAYER_LIST_FIELDS: &[&str] = &["RegisteredPlayers", "AllAlivePlayersList"];

    /// Player._renderers: Renderer[], the game's own cached renderer array for a
    /// player. Enumerating it is stable and cheap; chams prefers it over walking
    /// the transform tree (which is expensive and hits a node cap).
    pub const PLAYER_RENDERERS_FIELD: &str = "_renderers";

    // ── Raid framework (world.rs) ─────────────────────────────────────────────
    // Auto-property backing fields, read directly (no invoke). Verified live on
    // 46911: IsYourPlayer @0xb89 (bool), AIData @0xa00 (ptr, non-null => bot).
    pub const PLAYER_IS_LOCAL_FIELD: &str = "<IsYourPlayer>k__BackingField";
    pub const PLAYER_AIDATA_FIELD: &str = "<AIData>k__BackingField";
    /// Player.get_Position() -> Vector3 (boxed through runtime_invoke). No plain
    /// position field exists; the getter computes it from the transform.
    pub const PLAYER_GET_POSITION: &str = "get_Position";
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
        crate::elog!("[sym] MISSING assembly: {}", c.assembly);
        return None;
    };
    match unsafe { rt.class(image, c.namespace, c.name) } {
        Some(k) => Some(k),
        None => {
            let ns = if c.namespace.is_empty() { "<global>" } else { c.namespace };
            crate::elog!("[sym] MISSING class: {}::{}.{}", c.assembly, ns, c.name);
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
            crate::elog!("[sym] MISSING Type object for {}", c.name);
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
            crate::elog!("[sym] MISSING method: {}/{}", name, argc);
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
            crate::elog!("[sym] MISSING field: {}.{}", spec.class.name, spec.field);
            continue;
        };
        let offset = unsafe { rt.field_offset(field) };
        crate::elog!("[sym] {} -> offset {:#x}", spec.key, offset);
        out.entries.insert(spec.key, offset);
    }
    out
}
