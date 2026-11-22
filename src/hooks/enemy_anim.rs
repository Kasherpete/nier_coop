//! Enemy action sync (`enemy_anim=true`, with `world_sync=true`): the host's enemies decide what they do. Every
//! `start_animation` (vtable slot `START_ANIMATION_INDEX`, the slot the kill area calls on players) of an enemy
//! class is redirected here by patching that class's vtable slot. The host records the last call per enemy and
//! sends it in the world snapshot; the client drops its own AI's calls on paired enemies and replays the host's.
//! Untested in-game: assumes enemy classes use the same slot as `Pl0000` (BehaviorAppBase virtual).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use windows::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};

use super::in_module;
use crate::sdk::{self, Pl0000};

type StartAnim = unsafe extern "system" fn(*mut Pl0000, u32, u32, u32, u32);

#[derive(Default)]
struct Hooks {
    orig: HashMap<usize, usize>,           // enemy vtable -> its original start_animation
    last: HashMap<usize, ([u32; 4], u32)>, // host: object -> last call args, call count
    blocked: HashSet<usize>,               // client: paired objects whose own AI may not start animations
    known: HashSet<(usize, [u32; 4])>,     // (vtable, args) this game's own enemies called: the replay allowlist
}
/// Allowlist cap, so a long session cannot grow it without bound.
const MAX_KNOWN: usize = 8192;
static HOOKS: Mutex<Option<Hooks>> = Mutex::new(None);
/// Client: the object whose host call is being replayed (let through the block).
static APPLYING: AtomicUsize = AtomicUsize::new(0);

/// Patches `b`'s class once. Only call for objects that passed `world::enemy`'s type check.
pub fn hook(b: *mut Pl0000) {
    let Some(vt) = (unsafe { b.as_ref() }).map(|o| o.vtable as usize) else { return };
    let mut g = crate::state::lock(&HOOKS);
    let h = g.get_or_insert_with(Hooks::default);
    if h.orig.contains_key(&vt) {
        return;
    }
    let slot = (vt + sdk::START_ANIMATION_INDEX * 8) as *mut usize;
    if !in_module(slot as usize) {
        return;
    }
    let f = unsafe { slot.read() };
    if !in_module(f) {
        return;
    }
    let mut old = PAGE_PROTECTION_FLAGS(0);
    if unsafe { VirtualProtect(slot.cast(), 8, PAGE_READWRITE, &mut old) }.is_err() {
        tracing::error!("enemy anim: VirtualProtect failed on vtable {vt:#x}");
        return;
    }
    h.orig.insert(vt, f); // before the patch: another thread may call through the slot at once
    unsafe { slot.write_volatile(on_start as StartAnim as usize) };
    let _ = unsafe { VirtualProtect(slot.cast(), 8, old, &mut old) };
    tracing::info!("enemy anim: hooked vtable {vt:#x} slot fn {f:#x}");
}

unsafe extern "system" fn on_start(this: *mut Pl0000, a: u32, v: u32, a3: u32, a4: u32) {
    let p = this as usize;
    let Some(vt) = (unsafe { this.as_ref() }).map(|o| o.vtable as usize) else { return };
    let orig = {
        let mut g = crate::state::lock(&HOOKS);
        let Some(h) = g.as_mut() else { return };
        let Some(&f) = h.orig.get(&vt) else { return }; // only reached through a vtable patched above
        if APPLYING.load(Relaxed) != p && h.known.len() < MAX_KNOWN {
            h.known.insert((vt, [a, v, a3, a4])); // also the calls blocked below: our own AI asked for them
        }
        if h.blocked.contains(&p) && APPLYING.load(Relaxed) != p {
            return; // client: the host's enemy decides
        }
        let e = h.last.entry(p).or_default();
        *e = ([a, v, a3, a4], e.1.wrapping_add(1));
        f
    }; // unlocked before the call: the original may start another animation
    unsafe { std::mem::transmute::<usize, StartAnim>(orig)(this, a, v, a3, a4) }
}

/// Host: last call args and call count for object `b` (count 0: none seen).
pub fn last(b: *mut Pl0000) -> ([u32; 4], u32) {
    crate::state::lock(&HOOKS).as_ref().and_then(|h| h.last.get(&(b as usize)).copied()).unwrap_or_default()
}

/// Client: replays one host call on our paired enemy. Trust boundary: the args come from the peer and go into a
/// native game function, so only calls this class has made in our own game are replayed (others could crash it).
pub fn apply(b: *mut Pl0000, args: [u32; 4]) {
    let [a, v, a3, a4] = args;
    let Some(vt) = (unsafe { b.as_ref() }).map(|o| o.vtable as usize) else { return };
    let ok = crate::state::lock(&HOOKS).as_ref().is_some_and(|h| h.orig.contains_key(&vt) && h.known.contains(&(vt, args)));
    if !ok {
        return;
    }
    APPLYING.store(b as usize, Relaxed);
    unsafe { on_start(b, a, v, a3, a4) };
    APPLYING.store(0, Relaxed);
}

/// Client: the objects the host currently drives (empty when the host's snapshot is stale: own AI runs again).
pub fn set_blocked(objs: impl IntoIterator<Item = *mut Pl0000>) {
    if let Some(h) = crate::state::lock(&HOOKS).as_mut() {
        h.blocked = objs.into_iter().map(|b| b as usize).collect();
    }
}

/// Death, reload, peer gone: objects are freed and their addresses reused.
pub fn reset() {
    if let Some(h) = crate::state::lock(&HOOKS).as_mut() {
        h.last.clear();
        h.blocked.clear();
    }
}
