# World/entity sync: lockstep plan (2026-11-18)

Goal: enemies, world objects and story state identical on both machines, driven only by both players' inputs.

## Static findings (installed Steam exe, import table + call-site scan)

| Source of divergence | Evidence | Why it breaks lockstep |
|---|---|---|
| Wall-clock frame time | `QueryPerformanceCounter` used at 39 call sites, `timeGetTime` at 17, `GetTickCount` at 3 | Each frame's delta time is the machine's own frame time, so the same inputs move things by different amounts |
| Worker threads | `CreateThread` at 7 sites, each next to `SetThreadAffinityMask` | Job order between threads varies run to run |
| Random numbers | No CRT `rand`/`srand` import, so the engine has its own RNG (not found yet) | Enemy AI, damage and drops roll differently unless seeded identically and drawn in the same order |
| Floating point | x64 SSE | Same exe gives the same results for plain math, but approximate instructions (`rcpps`, `rsqrtps`) differ between Intel and AMD CPUs |
| Input | `DirectInput8Create` imported; XInput loaded at runtime | Not a problem: the mod already sends per-frame inputs |

Conclusion: pure lockstep (inputs only, nothing else) will diverge within seconds until timing and RNG are pinned.
Running blind is pointless, so the plan measures divergence first.

## Plan: lockstep with checksums and host repair

1. **Shared frame clock.** Both sides start frame 1 at the same event (area load finished) and tag inputs
   with it. The per-frame input packets in `src/net` already carry frame numbers and redundancy.
2. **Frame barrier.** In `on_tick`, run frame N only once the peer's input for frame N - D has arrived
   (D ≈ 3 frames of input delay). The game stalls instead of drifting when the network hiccups.
3. **Checksum per frame.** Hash every entity in the entity list (handle, position, health, state id at
   `Pl0000 + 0x670`), send it with the inputs, compare with the local hash for the same frame. Log the first
   frame and entity that differ. This is the tool that finds the remaining nondeterminism.
4. **Fixed delta time.** Find where the update loop turns QPC into dt and force 1/60 s on both sides.
5. **Shared RNG seed.** Find the engine's RNG (look for a multiply-add or xorshift state in `.data` called from
   AI code), seed it from the host at area load.
6. **Host repair.** Until checksums stay equal, when they differ the host sends a snapshot of the diverged
   entities (position, rotation, health, state). The game stays playable while determinism is incomplete,
   and repairs get rarer as steps 4–5 land.

Order: 1–3 first (no reversing needed, they show how fast and where the games diverge), then 4–5 guided by the
checksum log, 6 as the safety net.

## Open questions (need a live session)

- Which QPC call feeds the gameplay dt (vs. audio, video, profiling).
- Whether AI and physics run on the worker threads or the game thread.
- Whether both players must load the same save; story flags are probably simplest to send from the host once
  at area load rather than to lockstep.
