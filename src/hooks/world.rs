//! World sync (`world_sync=true`, one player `host=true`): the host's game owns the enemies, the client copies
//! them. Nobody waits: the host sends a snapshot every frame, the client pulls its enemies toward it and sends
//! back the damage it dealt, which the host applies. Game thread only.

use std::collections::{HashMap, HashSet};
use std::ptr::addr_of_mut;
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use super::{PL_VTABLE, entity_by_handle, for_each_object, in_module};
use crate::sdk::{Pl0000, cVec4};
use crate::state::{
    self, Damage, EnemyState, HOST, LOCAL_DAMAGE, LOCAL_WORLD, MAX_ENEMIES, REMOTE_ACTIVE, REMOTE_CORRECTION, REMOTE_DAMAGE,
    REMOTE_WORLD, WORLD_SYNC,
};

/// A snapshot older than this is not applied (host loading or gone): the client's own AI keeps running.
const STALE: Duration = Duration::from_millis(500);
/// Past this distance (m) a client enemy is teleported to the host's position, else pulled by `PULL` per frame.
const SNAP_DIST: f32 = 10.0;
const PULL: f32 = 0.5;

#[derive(Default)]
struct World {
    seen: HashSet<u32>,         // host: handles sent alive, still sent at hp 0 so the client sees the kill
    applied: HashMap<u32, u32>, // host: damage total already applied per handle
    written: HashMap<u32, u32>, // client: hp we wrote last frame per handle
    dealt: HashMap<u32, u32>,   // client: damage total dealt per handle
    logged: bool,
}
static WORLD: Mutex<Option<World>> = Mutex::new(None);

pub fn tick(pl: *mut Pl0000) {
    let mut guard = state::lock(&WORLD);
    if !WORLD_SYNC.load(Relaxed) || !REMOTE_ACTIVE.load(Relaxed) {
        *guard = None; // peer gone: forget everything, handles mean nothing in the next session
        return;
    }
    let w = guard.get_or_insert_with(World::default);
    if HOST.load(Relaxed) { host(w, pl) } else { client(w) }
}

/// Synced objects only: enemies ("em" + id names), never a `Pl0000` (players, buddy). Writing health to an object
/// that is not a BehaviorAppBase would corrupt it, so the name filter is the safety check.
fn enemy(handle: u32, kind: Option<u32>) -> Option<(*mut Pl0000, u32)> {
    let e = entity_by_handle(handle)?;
    let name = unsafe { addr_of_mut!((*e).name).read() };
    let b = unsafe { addr_of_mut!((*e).behavior).read() };
    let vt = unsafe { b.as_ref()? }.vtable as usize;
    let k = name_hash(&name);
    let pl = PL_VTABLE.load(Relaxed);
    // shortcut: a game vtable + "em" name is the type check; an allowlist of enemy vtables would be stricter, add one if a non-Behavior "em*" object shows up.
    let ok = pl != 0 && vt != pl && in_module(vt) && name.starts_with(b"em");
    (ok && kind.is_none_or(|x| x == k)).then_some((b, k))
}

fn name_hash(name: &[u8]) -> u32 {
    name.iter().take_while(|&&c| c != 0).fold(0x811C_9DC5, |h, &c| (h ^ c as u32).wrapping_mul(0x0100_0193))
}

fn host(w: &mut World, pl: *mut Pl0000) {
    // The client's hits, as totals: apply only what is new since the last packet.
    for d in std::mem::take(&mut *state::lock(&REMOTE_DAMAGE)) {
        let Some((b, _)) = enemy(d.handle, None) else { continue };
        let last = w.applied.entry(d.handle).or_insert(0);
        if d.total > *last {
            // shortcut: writes health directly; if a kill this way leaves the enemy standing at 0, call the engine's damage function instead.
            unsafe { addr_of_mut!((*b).health).write((*b).health.saturating_sub(d.total - *last)) };
            *last = d.total;
        }
    }
    // Snapshot of the enemies nearest the client (or the host when the client has no character).
    let center = state::lock(&REMOTE_CORRECTION).map_or_else(|| unsafe { addr_of_mut!((*pl).position).read() }, |c| c.pos);
    let mut snap = Vec::new();
    for_each_object(|handle, b, pos| {
        let Some((_, kind)) = enemy(handle, None) else { return };
        let hp = unsafe { addr_of_mut!((*b).health).read() };
        if hp > 0 {
            w.seen.insert(handle);
        } else if !w.seen.contains(&handle) {
            return;
        }
        let yaw = unsafe { addr_of_mut!((*b).rotation).read().y };
        snap.push(EnemyState { handle, kind, hp, yaw, pos });
    });
    snap.sort_by(|a, b| dist2(a.pos, center).total_cmp(&dist2(b.pos, center)));
    snap.truncate(MAX_ENEMIES);
    if !w.logged && !snap.is_empty() {
        w.logged = true;
        tracing::info!("world sync (host): sending {} enemies", snap.len());
    }
    *state::lock(&LOCAL_WORLD) = snap;
}

fn client(w: &mut World) {
    let snap = match &*state::lock(&REMOTE_WORLD) {
        Some((at, s)) if at.elapsed() < STALE => s.clone(),
        _ => return,
    };
    let mut matched = 0;
    for e in &snap {
        let Some((b, _)) = enemy(e.handle, Some(e.kind)) else { continue };
        matched += 1;
        unsafe {
            let hp = addr_of_mut!((*b).health);
            let (write, dealt) = merge_hp(e.hp, hp.read(), w.written.get(&e.handle).copied());
            *w.dealt.entry(e.handle).or_insert(0) += dealt;
            hp.write(write);
            w.written.insert(e.handle, write);
            let pos = addr_of_mut!((*b).position);
            pos.write(pull(pos.read(), e.pos));
            addr_of_mut!((*b).rotation.y).write(e.yaw);
        }
    }
    if !w.logged {
        w.logged = true;
        tracing::info!("world sync (client): {matched}/{} host enemies matched here", snap.len());
    }
    let mut out: Vec<Damage> =
        snap.iter().filter_map(|e| w.dealt.get(&e.handle).filter(|&&t| t > 0).map(|&total| Damage { handle: e.handle, total })).collect();
    out.truncate(MAX_ENEMIES);
    *state::lock(&LOCAL_DAMAGE) = out;
}

/// Client, one enemy: the hp to write and the damage our own hits did since last frame. The lower of the host's
/// value and ours wins, so our hit shows at once instead of flickering back until the host has applied it.
fn merge_hp(host_hp: u32, now: u32, written: Option<u32>) -> (u32, u32) {
    (host_hp.min(now), written.map_or(0, |w| w.saturating_sub(now)))
}

fn pull(p: cVec4, t: cVec4) -> cVec4 {
    if dist2(p, t) > SNAP_DIST * SNAP_DIST {
        return t;
    }
    cVec4 { x: p.x + (t.x - p.x) * PULL, y: p.y + (t.y - p.y) * PULL, z: p.z + (t.z - p.z) * PULL, w: t.w }
}

fn dist2(a: cVec4, b: cVec4) -> f32 {
    (a.x - b.x).powi(2) + (a.y - b.y).powi(2) + (a.z - b.z).powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_hp_and_damage() {
        assert_eq!(merge_hp(100, 100, None), (100, 0)); // first sight
        assert_eq!(merge_hp(100, 80, Some(100)), (80, 20)); // our hit: shown now, 20 reported
        assert_eq!(merge_hp(100, 80, Some(80)), (80, 0)); // host has not applied it yet: no flicker back to 100
        assert_eq!(merge_hp(60, 80, Some(80)), (60, 0)); // host's own hits come through
        assert_eq!(merge_hp(0, 60, Some(60)), (0, 0)); // host killed it
    }

    #[test]
    fn names_and_pull() {
        assert_eq!(name_hash(b"em1000\0junk"), name_hash(b"em1000"));
        assert_ne!(name_hash(b"em1000"), name_hash(b"em1001"));
        let o = cVec4::default();
        assert_eq!(pull(o, cVec4 { x: 2.0, ..o }).x, 1.0);
        assert_eq!(pull(o, cVec4 { x: 20.0, ..o }).x, 20.0);
    }
}
