# Quest / story state (for quest sync)

Steam exe as installed 2026-11-19 (same build as the `src/sdk/mod.rs` offsets). RVAs are exe-relative.

## In the save file

`SlotData_N.dat` (235980 bytes) holds a plain-text tree at file 0x7c..~0x38de (~14 KB), indent = depth:

```
version v0.0.2
Scene
 Action
  <hash>            per scripted event: done 1 / disabled 1 / state DONE / layout / user0 / user1 / ch
   script           size N, value { first <name>, second <state> } (e.g. this_q770_clear / done)
 Behavior
 ...                flat list of story flags: q120/DONE, q770_FIN, q410/REQ, Route/A, Buddy/Auto, ...
Flags
 ft_RC
Quest               active quest ids: q006, q160, q410
```

One quest step (q120 + q770 done, q410 taken) changed ~31 lines: new Scene/Action entries, flag lines added
(q120/DONE, q770_FIN, q770/FIN) and removed (q120/RUN_1, q770/RUN_1), and the Quest list.

## In memory

- The whole save image (file 0x70..end) sits in the exe image at exe+0x1422494 (file offset 0 -> exe+0x1422424),
  and a second copy at exe+0x145c334. Both match the last *saved* file: it is the save buffer, not live state.
  Found by the ignored test `map_save_in_running_game` (`src/net/save.rs`).
- Text tree serializer: generic node tree with a refcounted, interned string pool. Globals: pool exe+0xf8c4e0
  (+0x08 ptr, +0x28 CRITICAL_SECTION exe+0xf8c508), string hash fn exe+0x28ee30(str, len), intern/find
  exe+0x150e70(pool, hash, str, len), child find exe+0x3277e0(node, &key, 0), child add exe+0x33cdf0(node, &key, 0).
- Top level (version, "Scene/Action"): exe+0x3e73f0 (called from exe+0x3f0797).
- "Flags" save: exe+0x351120 (called from exe+0x351109). It gets the flag objects from registry exe+0xfc9998
  via exe+0x3d7740(registry, &out_vec, "Flags"), and each flag's on/off byte from the hash map at exe+0xf23540
  (lookup exe+0x326640, entry +8 = value; table end = [exe+0xf23548] + [exe+0xf23550]*16).
- "Quest" key: refs at exe+0x35b117 / exe+0x35b129 (function exe+0x35ae50).
- Scene/Action keys "layout"/"user0": exe+0x398a10 (one direction) and exe+0x398b10 (the other).

## Next

Live flags = hash map exe+0xf23540 (flag object -> byte). Find the setter (writes entry +8) and the load-side
twin of exe+0x351120: calling the game's own setter on the client with the host's changed flags is the safest sync.
