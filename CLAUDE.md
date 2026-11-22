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

Output: `target/x86_64-pc-windows-msvc/release/nier_coop.dll`. It exports `DirectInput8Create` (forwarded to the
System32 dinput8.dll by full path), so renamed to `dinput8.dll` in the game folder it loads at startup; an injector
also works, and a named mutex keeps a second copy from running. `package/build.ps1` makes `dist/nier_coop.zip`
(the DLL as dinput8.dll, `Install.bat`/`install.ps1` that finds the Steam folder and writes the cfg, `Uninstall.bat`).
The net thread trusts the peer's IP and learns its port from what arrives, so port-changing routers still connect. Both sides send `NCVR` + the crate version every second; a different version, or an `NCO*` input packet with another layout tag, logs VERSION MISMATCH. On load, a debug console opens and `tracing` logs go to stdout.

## Folder structure

- `src/hooks/`: detours (`retour` static detours).
- `src/sdk/`: reversed C++ classes (`Pl0000`, `cVec4`, `Behavior`, ...).
- `src/net/`: UDP input sync, decoupled from the game thread (hooks only read/write shared state; networking never runs inside a hook). Every game frame's controller input is sent with the previous 7 frames (loss-proof); the receiver applies one frame per tick in order. Each frame carries the sender's position on that frame, and drift correction (>1 m pull, >10 m snap, >0.5 rad yaw) measures against the frame being applied; the sender's newest position/health ride along separately (`Correction`, cleared while it has no character, except a dead player stays reported dead through the reload). After 30 ticks with no input the partner lets go of the stick. Send errors are logged, never fatal. Story events (`NCEV`) and scene signals (`NCSG`) go over `src/net/reliable.rs` (`NCRL`/`NCRA`: resent until acked, delivered once and in order).
- `src/hooks/lockstep.rs` (`lockstep=true`, one player `host=true`): steps 1–3 of `notes/lockstep-world-sync.md`. The host starts an epoch when both players are ready (a `Pl0000` exists), the client joins it; frames count from 1 per epoch. Each frame waits up to 100 ms for the peer's input of frame N-3, and compares a world checksum (FNV of the sorted (type id, cm position) of every object except the two players; not handles, which each game assigns itself). `nier_coop.log` gets the first differing frame and a summary every 600 frames. Known gap: the local player's own input is not delayed by 3 frames, so the two games never apply identical inputs on identical frames yet; that, fixed dt and a shared RNG seed come next.
- `src/hooks/world.rs` (`world_sync=true`, one player `host=true`; the no-wait alternative to lockstep): the host sends, every frame, the 24 enemies (type id 0x2xxxx, not `Pl0000`) nearest the client, live ones first (a corpse is sent for 2 s, then dropped): handle, type id, health (BehaviorAppBase 0x858), yaw, position, and the client damage it has applied. Each game numbers its objects itself, so the client pairs each host enemy with its nearest unpaired enemy of the same type within 150 m (a one-of-a-kind enemy at any distance), pulls position (50%/frame, snap >10 m), sets yaw and writes host hp minus its own not-yet-acknowledged damage; the damage its hits do goes back as per-enemy totals, which the host subtracts. Totals belong to a generation on each side (`state::WorldGen`, new on every reset: death, respawn, hacking, timeout), so none is applied twice or ignored after a reset. Packets `NCW2`/`NCD2`. The client logs host enemies it has no copy for every 10 s. Known gaps: the client's enemy AI still runs locally on unpaired enemies; each partner avatar's hits also land in the other game, so damage counts roughly double; kills by writing health 0 may not trigger death.
- `src/hooks/events.rs`: host scene events (cutscene play, SendCommand, objective state, enemy-set spawn) are applied on the client; an event whose action is not loaded there yet is retried every 30 frames for up to 180 s (60 s for a cutscene). Each game loads action groups by itself: a session log showed the client load a boss-fight group 57 s after the host had spawned its 26 enemy sets, and dropping those events left the client without the host's enemies. Sender contexts for forwarded signals are only reused while the same action object is loaded.
- `src/hooks/quest.rs`: host scene states are set on the client at once, but reset only after the host lacked them in 3 lists in a row, and never from a host list under half the client's size.
- `src/hooks/enemy_anim.rs` (`enemy_anim=true`, with world_sync): patches vtable slot 18 (`start_animation`) of every enemy class seen. The host records each enemy's last call and count in the snapshot (`EnemyState` 0x38 B, 24 per packet to stay in one UDP payload); the client drops its own AI's calls on paired enemies and replays the host's when the count changes. Untested in-game: assumes enemy classes use the same slot as `Pl0000`. After a death the client also warps to the host if it reloaded >30 m away (different checkpoint), and a one-of-a-kind enemy (boss) pairs at any distance.
- `src/net/save.rs` (`save_sync=true`): the host checks its save folder (Documents via the shell, OneDrive-safe) every second and sends the newest `SlotData_N.dat` in 1 KB chunks (`NCSV`, rounds until the client acks the CRC with `NCSA`); the client checks slot/size/offsets/CRC, keeps its own file once as `.nier_coop.bak`, and writes via a temp file. Re-sent after a peer timeout. The game lists slots only once (seen in-game: GameData.dat and SlotData_0..3 opened with CreateFileA ~20 s after launch, after the logos), so `src/hooks/savefile.rs` detours KernelBase CreateFileA/W on the client and holds that first SlotData open (up to 20 s) until the save is on disk. Verified in-game with the ignored `fake_host_for_running_game` test.
- `notes/`: Ghidra/IDA/ReClass dumps.
- `src/scan.rs`: AoB scanning and RIP-relative resolution. `src/state.rs`: lock-free seqlock slots shared by hooks and net.

## Status

Offsets and patterns in `src/sdk/mod.rs` come from praydog/AutomataMP (MIT, Steam build, Jan 2024) plus this project's own reversing, checked against the Steam exe built 2026-10-09 (see the ignored tests in `src/hooks/mod.rs`, run with `NIER_EXE=<exe path>`). `hooks::install` resolves the entity list, possessed-entity handle and per-frame update function by pattern; if any lookup fails it logs the reason and hooks nothing. The update hook is a naked entry thunk (signature unknown, so it preserves the argument registers). Runtime config: `nier_coop.cfg` (see `nier_coop.cfg.example`).

Verified in-game (mirror test, bind = peer = 127.0.0.1): with `spawn_2b=true` the remote is a spawned 2B made the local player's buddy (AutomataMP sequence, `changePlayer` twice). She is driven by controller fields (stick, held/pressed buttons) plus replayed animations, never by the local pad; walk/run/attacks/idle match. (Verified before the switch to per-frame input sync on branch feat/input-sync, which drops animation replay unless `anim_replay=true`; not yet re-verified in-game.) Her pod fires via `ai_pod_fire` and aims at the entity nearest the peer's pod aim point (`pod_target`), or at the aim point itself through a mid-function hook in pod_shot when nothing is near. Pod skills (incl. charged) work: the engine runs the pod-program update and cooldown tick only for the player-controlled character, so `run_pod_program` calls the update for her and copies the peer's cooldown. She is kept invincible (`muteki_timer`) and shows the peer's health, since her death would fail the mission, and is respawned ~5 s after the game removes her (restart, area load). When the peer's health stays 0 for ~2 s, the local player is killed the way a kill area does it (`life` 0x10668 and `health` to 0, then state 0x35; writing `health` alone never kills, see `notes/death-and-warp.md`), so both games reload their last checkpoint. If the peer dies while the local player has no character (hacking, flight unit, loading), the death is followed once the character is back, or the player warps to the peer if it has already respawned. Snaps (>10 m, stuck) use the engine's `Pl0000::warp`, since writing `position` alone does not stick over long distances (untested in-game).

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
