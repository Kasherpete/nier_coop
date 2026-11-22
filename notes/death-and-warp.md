# Player death and warp (exe 2026-10-09, image offsets)

Found 2026-11-21 by static disassembly (iced-x86) of the installed Steam exe.

## Two health fields

- `Pl0000+0x858` (`health`, BehaviorAppBase) is a copy. `Pl0000+0x10668` (`life`) is the real value.
- Damage `0x4fe150(pl, amount, non_lethal)`: `life -= amount`, clamped at 0 (or at 1 when `non_lethal` or
  flags 0x16FD8 / 0x174AC say so); at `life <= 0` it also zeroes `health`. It does not start the death itself.
- Per-frame update `0x42a28b` (big function): at 0x42a506, `if [pl+0x10664] <= 0: health = life`.
  So writing `health = 0` alone is undone the next frame.

## Death

- `0x47af40` (called every frame from 0x42a529) handles hits. It returns at once when `health <= 0` or
  `life <= 0` (0x47afd5), so a player whose health was zeroed by hand never dies, it just stops reacting.
- At 0x47b160 it asks `0x389b70(&pl->position)` (walks the kill-area list at global 0xfc44e0, containment
  test 0x3dd1c0, same as `hap::KillArea`). Inside one:
  `life = 0; health = 0; vtable[18](pl, 0x35, 0, 0, 1)` (vtable 18 = `start_animation`/set state, state at 0x670).
- Hits that bring `life` to 0 also end in state 0x35 (0x47d597 and others); special modes use 0x7D, 0x9C, 0x8F.

## Warp

- `0x4f0fa0(pl, &pos, &rot, flag, f32)` teleports: copies `pos` to 0x50, 0x178A0, 0x17890, resets the collision
  body (vtable +0x298) and more. Skipped when state == 0xD3. Callers pass flag 0 and 0.0
  (0x7cce65: buddy to the player when more than 10 m apart, from an event hook; 0x4982a2).
- The mod's old snap wrote only 0x50. In a boss fight the client's log showed snaps of ~108 m every frame for
  minutes that never took, the partner left out of sight.
