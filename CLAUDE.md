# nier_coop

Co-op mod DLL for NieR:Automata, injected into the game process.

## Target binary

- `NieRAutomata.exe`: x86_64 Windows, MSVC ABI, PlatinumGames proprietary C++ engine, DirectX 11.
- Calling convention: x64 MSVC (`__fastcall`). In Rust use `extern "system"` (identical to `"C"` on x64) for hooked functions; `this` is the first argument (RCX).
- ASLR is enabled. **Never hardcode absolute addresses.** Resolve every address by AoB/pattern scan, or as `module_base + offset` with the offset documented alongside the game version it was taken from.

## Engine terminology

- Entities inherit from `BehaviorAppBase` / `Behavior`.
- Playable characters (2B, 9S, A2) are class `Pl0000`.
- Companion/partner AI runs through the buddy/NPC behavior routines.
- World/entity management: `SceneEntitySystem` and `BehaviorSystem`.

## Build & deploy

```
cargo build --release --target x86_64-pc-windows-msvc
```

Requires nightly Rust (pinned in `rust-toolchain.toml`) because retour's `static-detour` uses unstable features. If the project path is deep, `link.exe` fails with LNK1104 (paths over 260 chars): set `CARGO_TARGET_DIR` to a short folder.

Output: `target/x86_64-pc-windows-msvc/release/nier_coop.dll`. Load it with an injector or a proxy DLL. On load, a debug console opens and `tracing` logs go to stdout.

## Folder structure

- `src/hooks/`: detours (`retour` static detours).
- `src/sdk/`: reversed C++ classes (`Pl0000`, `cVec4`, `Behavior`, ...).
- `src/net/`: UDP input sync, decoupled from the game thread (hooks only read/write shared state; networking never runs inside a hook). Every game frame's controller input is sent with the previous 7 frames (loss-proof); the receiver applies one frame per tick in order. The sender's real position/health ride along only for drift correction (>1 m pull, >10 m snap, >0.5 rad yaw).
- `src/hooks/lockstep.rs` (`lockstep=true`, one player `host=true`): steps 1–3 of `notes/lockstep-world-sync.md`. The host starts an epoch when both players are ready (a `Pl0000` exists), the client joins it; frames count from 1 per epoch. Each frame waits up to 100 ms for the peer's input of frame N-3, and compares a world checksum (FNV of handle + cm position of every object except the two players). `nier_coop.log` gets the first differing frame and a summary every 600 frames. Known gap: the local player's own input is not delayed by 3 frames, so the two games never apply identical inputs on identical frames yet; that, fixed dt and a shared RNG seed come next.
- `src/hooks/world.rs` (`world_sync=true`, one player `host=true`; the no-wait alternative to lockstep): the host sends, every frame, the 32 enemies (entity name `em*`, not `Pl0000`) nearest the client: handle, name hash, health (BehaviorAppBase 0x858), yaw, position. The client matches by handle + name hash, pulls position (50%/frame, snap >10 m), sets yaw and writes min(host hp, own hp); the damage its own hits do is sent back as per-enemy totals, which the host subtracts. Packets `NCWS`/`NCWD`. Untested in-game: assumes equal handles in both games, the client's enemy AI and animations still run locally, and kills by writing health 0 may not trigger death.
- `notes/`: Ghidra/IDA/ReClass dumps.
- `src/scan.rs`: AoB scanning and RIP-relative resolution. `src/state.rs`: lock-free seqlock slots shared by hooks and net.

## Status

Offsets and patterns in `src/sdk/mod.rs` come from praydog/AutomataMP (MIT, Steam build, Jan 2024) plus this project's own reversing, checked against the Steam exe built 2026-10-09 (see the ignored tests in `src/hooks/mod.rs`, run with `NIER_EXE=<exe path>`). `hooks::install` resolves the entity list, possessed-entity handle and per-frame update function by pattern; if any lookup fails it logs the reason and hooks nothing. The update hook is a naked entry thunk (signature unknown, so it preserves the argument registers). Runtime config: `nier_coop.cfg` (see `nier_coop.cfg.example`).

Verified in-game (mirror test, bind = peer = 127.0.0.1): with `spawn_2b=true` the remote is a spawned 2B made the local player's buddy (AutomataMP sequence, `changePlayer` twice). She is driven by controller fields (stick, held/pressed buttons) plus replayed animations, never by the local pad; walk/run/attacks/idle match. (Verified before the switch to per-frame input sync on branch feat/input-sync, which drops animation replay unless `anim_replay=true`; not yet re-verified in-game.) Her pod fires via `ai_pod_fire` and aims at the entity nearest the peer's pod aim point (`pod_target`), or at the aim point itself through a mid-function hook in pod_shot when nothing is near. Pod skills (incl. charged) work: the engine runs the pod-program update and cooldown tick only for the player-controlled character, so `run_pod_program` calls the update for her and copies the peer's cooldown. She is kept invincible (`muteki_timer`) and shows the peer's health, since her death would fail the mission, and is respawned ~5 s after the game removes her (restart, area load).

Not done: flight-unit sections (the local player is not a `Pl0000` there, so nothing is sent or applied; invincibility and respawn should carry her through, untested there). Needs a save inside a flight section to reverse live.

## Strict rules

1. **Never overwrite files in `notes/`.** Add new files only; do not edit or delete existing ones.
2. Every reversed struct is `#[repr(C)]`, with an explicit byte-offset comment on every field, plus padding fields for unknown gaps. Example:
   ```rust
   #[repr(C)]
   pub struct cVec4 {
       pub x: f32, // 0x00
       pub y: f32, // 0x04
       pub z: f32, // 0x08
       pub w: f32, // 0x0C
   }
   const _: () = assert!(core::mem::size_of::<cVec4>() == 0x10);
   ```
3. No raw pointer arithmetic or dereference without a null check and a bounds check against the known struct size or module range. Prefer `ptr.as_ref()` / `Option<&T>` over raw `*ptr`.
4. Keep `DllMain` trivial: everything runs on the worker thread it spawns (the loader lock is held inside `DllMain`).
