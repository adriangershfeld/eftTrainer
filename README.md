# eftTrainer

Internal trainer for SPT/EFU (offline single-player Tarkov fork), injected via manual DLL injection (Extreme Injector, no mod-loader framework). Rust instead of the usual C++/MinHook/ImGui stack.

## Architecture

- `runtime.rs` — `ScriptRuntime` trait + neutral opaque handles (Domain/Image/Class/Method/Field/Object). Backend-agnostic; `il2cpp_runtime.rs` is the only implementation (Mono support removed, see `_removed_mono/`).
- `il2cpp.rs` — raw IL2CPP FFI, bound only to exports verified as genuine (not decoyed thunks).
- `il2cpp_abi.rs` — struct layout discovered at load time by probing metadata invariants, no hardcoded offsets.
- `symbols.rs` — every game/engine name in one place, split ENGINE (Unity, portable) / GAME (BSG, volatile).
- `hooks.rs` — retour trampoline hooks, driven off a per-frame GameWorld message.
- `menu.rs` / `console.rs` / `input.rs` — pure UGUI menu (no ImGui, no DX11 Present hook), Win98 themed, INSERT to toggle, own input capture.
- `chams.rs`, `world.rs`, `features.rs` — chams/ESP, player/world state, misc toggles (stamina, speedhack).
- `http.rs` — loopback-only control/introspection server for driving the trainer and querying live metadata without static analysis.
- `crash.rs` / `control.rs` — vectored exception logger, cooperative abort flag for long scans.

## Build

```
cargo build --release
```

Output: `target/release/eft_trainer.dll`.

## Status

Working DLL injection, menu, chams, HTTP control server, stamina/speedhack. IL2CPP backend, ported off the earlier Mono implementation.
