//! Spawning a second 2B for the remote player (method from praydog/AutomataMP `NierClient::on_create_player`).
//! Runs on the game thread only.

use std::ffi::c_void;
use std::ptr::addr_of_mut;
use std::sync::OnceLock;

use super::{as_pl0000, in_module};
use crate::scan;
use crate::sdk::{self, Entity, Pl0000, ScriptFunction, SpawnMatrix, SpawnParams, cVec4, patterns};

/// Image offsets found by pattern; see `sdk::patterns`.
#[derive(Debug)]
pub struct Offsets {
    pub spawn: usize,      // fn(spawner, &SpawnParams) -> *mut Entity
    pub spawner: usize,    // its `this` (a global object)
    pub routine: usize,    // fn(name, *mut Entity): assigns an AI routine
    pub scripts: usize,    // global `ScriptFunction*` (list head)
    pub held_store: usize, // instruction to mid-hook in the controller's button processing
}

/// End (+4: the next instruction) of every rel32 operand pointing at the string `s`.
fn string_refs(m: &[u8], name: &str, s: &str) -> Result<Vec<usize>, String> {
    let bytes: Vec<_> = s.bytes().chain([0]).map(Some).collect();
    let at = scan::find_unique(m, &bytes).ok_or(format!("{name}: string not found or not unique"))?;
    let v: Vec<_> = (0..m.len().saturating_sub(4)).filter(|&i| scan::rip_target(m, i, 0, 4) == Some(at)).map(|i| i + 4).collect();
    if v.is_empty() { Err(format!("{name}: no code reference")) } else { Ok(v) }
}

fn string_ref(m: &[u8], name: &str, s: &str) -> Result<usize, String> {
    string_refs(m, name, s).map(|v| v[0])
}

/// First match of `pat` starting in `[start, start + len)` (AutomataMP scans N instructions; ~4 bytes each).
fn in_window(m: &[u8], name: &str, start: usize, len: usize, pat: &str) -> Result<usize, String> {
    let p = scan::parse(pat).ok_or(format!("{name}: invalid pattern"))?;
    let w = m.get(start..start + len + p.len()).ok_or(format!("{name}: past module end"))?;
    let hit = w.windows(p.len()).position(|x| x.iter().zip(&p).all(|(b, q)| q.is_none_or(|q| q == *b)));
    Ok(start + hit.ok_or(format!("{name}: not found"))?)
}

pub fn resolve(m: &[u8]) -> Result<Offsets, String> {
    // The first reference is a string compare; the spawn site builds SpawnParams{name = "MapInstance"}.
    let lea = string_refs(m, "SPAWN_STRING", patterns::SPAWN_STRING)?
        .into_iter()
        .find_map(|r| in_window(m, "SPAWN_LEA", r, 0x40, patterns::SPAWN_LEA).ok())
        .ok_or("SPAWN_LEA: not found after any string reference")?;
    let spawner = scan::rip_target(m, lea, 3, 7).ok_or("SPAWN_LEA: bad rip target")?;
    let spawn = scan::rip_target(m, lea + 7, 1, 5).ok_or("SPAWN call: bad rip target")?;

    let r = string_ref(m, "ROUTINE_STRING", patterns::ROUTINE_STRING)?;
    let w = m.get(r..r + 10).ok_or("ROUTINE: past module end")?;
    let jmp = r + w.iter().position(|&b| b == 0xE9).ok_or("ROUTINE: no jmp after string reference")?;
    let routine = scan::rip_target(m, jmp, 1, 5).ok_or("ROUTINE: bad rip target")?;

    let r = string_ref(m, "SCRIPT_STRING", patterns::SCRIPT_STRING)?;
    let mov = in_window(m, "SCRIPT_MOV", r, 10 * 4, patterns::SCRIPT_MOV)?;
    let scripts = scan::rip_target(m, mov, 3, 7).ok_or("SCRIPT_MOV: bad rip target")?;

    let end = scan::find_unique(m, &scan::parse(patterns::BUTTONS_END).ok_or("BUTTONS_END: invalid pattern")?)
        .ok_or("BUTTONS_END: not found or not unique")?;
    let func = scan::rfind(m, end, 0x1000, &scan::parse("CC CC CC").unwrap()).ok_or("BUTTONS: function start not found")? + 3;
    let held_store = in_window(m, "HELD_STORE", func, 0x100, patterns::HELD_STORE)?;
    Ok(Offsets { spawn, spawner, routine, scripts, held_store })
}

/// Absolute addresses, set once by `hooks::install`.
pub static ADDR: OnceLock<Offsets> = OnceLock::new();

/// CRC-32 (IEEE), as the engine hashes script function names.
pub fn crc32(s: &[u8]) -> u32 {
    !s.iter().fold(!0u32, |mut c, &b| {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg());
        }
        c
    })
}

/// Native function behind a script function such as "Behavior.setSuspend".
fn script_fn(name: &str) -> Option<usize> {
    let head_ptr = ADDR.get()?.scripts as *const *const ScriptFunction;
    let want = crc32(name.as_bytes());
    let head = unsafe { head_ptr.read_volatile() };
    let mut node = head;
    // Nodes are static objects in the image; anything else means the list is not what we think.
    for _ in 0..100_000 {
        let n = node as usize;
        if node.is_null() || !in_module(n) || !in_module(n + size_of::<ScriptFunction>() - 1) {
            break;
        }
        let f = unsafe { &*node };
        if f.crc == want {
            return in_module(f.function).then_some(f.function);
        }
        node = f.next;
        if node == head {
            break;
        }
    }
    tracing::error!("script function {name} not found");
    None
}

/// Spawns a 2B next to `pl`, makes it `pl`'s buddy (so `on_tick` drives it) and gives it the player
/// AI routine, which moves from controller fields instead of buddy AI. Returns its behavior.
pub fn partner(pl: *mut Pl0000, pl_handle: u32) -> Option<*mut Pl0000> {
    let a = ADDR.get()?;
    let (suspend, from_npc) = (script_fn("Behavior.setSuspend")?, script_fn("Pl0000.setBuddyFromNpc")?);
    let change = script_fn("Pl0000.changePlayer")?;
    type Spawn = unsafe extern "system" fn(*mut c_void, *const SpawnParams) -> *mut Entity;
    type Routine = unsafe extern "system" fn(*const u8, *mut Entity);
    type Suspend = unsafe extern "system" fn(*mut Pl0000, i32);
    type FromNpc = unsafe extern "system" fn(*mut Pl0000);
    unsafe {
        let one = cVec4 { x: 1.0, y: 1.0, z: 1.0, w: 0.0 };
        let mut m = SpawnMatrix {
            forward: cVec4 { x: 1.0, ..Default::default() },
            up: cVec4 { y: 1.0, ..Default::default() },
            right: cVec4 { z: 1.0, ..Default::default() },
            w: cVec4 { w: 1.0, ..Default::default() },
            position: addr_of_mut!((*pl).position).read(),
            rotation: addr_of_mut!((*pl).rotation).read(),
            scale: one,
            unk: [0; 8],
        };
        m.position.w = 1.0;
        m.unk[6] = 1;
        let params = SpawnParams::new(b"partner\0", sdk::MODEL_2B, &m);
        let ent = std::mem::transmute::<usize, Spawn>(a.spawn)(a.spawner as *mut c_void, &params);
        let Some(e) = ent.as_mut() else {
            tracing::error!("spawn returned null");
            return None;
        };
        let Some(b) = as_pl0000(e) else {
            tracing::error!("spawned entity has no Pl0000 behavior yet");
            return None;
        };
        // Same order as AutomataMP: link buddies, unsuspend, player routine, then changePlayer twice
        // (control goes to the new buddy and back), which settles both characters' player/buddy state.
        addr_of_mut!((*b).buddy_handle).write(pl_handle);
        addr_of_mut!((*pl).buddy_handle).write(addr_of_mut!((*e).handle).read());
        std::mem::transmute::<usize, Suspend>(suspend)(b, 0);
        let routine = std::mem::transmute::<usize, Routine>(a.routine);
        routine(b"PLAYER\0".as_ptr(), e);
        routine(b"player\0".as_ptr(), e);
        let change = std::mem::transmute::<usize, FromNpc>(change);
        change(pl);
        change(pl);
        addr_of_mut!((*b).obj_flags).write(u32::MAX);
        std::mem::transmute::<usize, FromNpc>(from_npc)(b);
        addr_of_mut!((*b).obj_flags).write(0);
        tracing::info!("spawned remote 2B: entity {:#x} behavior {:#x}", e as *mut Entity as usize, b as usize);
        Some(b)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc32_known() {
        assert_eq!(super::crc32(b"123456789"), 0xCBF4_3926);
    }

    /// `NIER_EXE=<exe> cargo test spawn -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn resolves_against_installed_exe() {
        let img = crate::hooks::tests::map_image(&std::fs::read(std::env::var("NIER_EXE").unwrap()).unwrap());
        let o = super::resolve(&img).unwrap();
        println!("{o:x?}");
        let starts = crate::hooks::exe_checks::func_starts(&img);
        for (n, at) in [("spawn", o.spawn), ("routine", o.routine)] {
            println!("{n} is a function start: {}, first bytes {:02x?}", starts.binary_search(&at).is_ok(), &img[at..at + 12]);
        }
        println!("held_store bytes {:02x?}", &img[o.held_store..o.held_store + 16]);
    }
}
