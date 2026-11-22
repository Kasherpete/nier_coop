//! World sync (`world_sync=true`, one player `host=true`): the host's game owns the enemies, the client copies
//! them. Nobody waits: the host sends a snapshot every frame, the client pulls its enemies toward it and sends
//! back the damage it dealt, which the host applies. Game thread only.

use std::collections::{HashMap, HashSet};
use std::ptr::addr_of_mut;
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use super::{PL_VTABLE, enemy_anim, entity_by_handle, for_each_object, in_module};
use crate::sdk::{Pl0000, cVec4};
use crate::state::{
    self, Damage, ENEMY_ANIM, EnemyState, HOST, LOCAL_DAMAGE, LOCAL_WORLD, MAX_ENEMIES, REMOTE_ACTIVE, REMOTE_CORRECTION, REMOTE_DAMAGE,
    REMOTE_WORLD, WORLD_SYNC,
};

/// A snapshot older than this is not applied (host loading or gone): the client's own AI keeps running.
const STALE: Duration = Duration::from_millis(500);
/// Past this distance (m) a client enemy is teleported to the host's position, else pulled by `PULL` per frame.
const SNAP_DIST: f32 = 10.0;
const PULL: f32 = 0.5;
/// A host enemy is paired with our nearest unpaired enemy of the same type within this distance (m).
const MATCH_DIST: f32 = 30.0;

#[derive(Default)]
struct World {
    seen: HashSet<u32>,         // host: handles sent alive, still sent at hp 0 so the client sees the kill
    applied: HashMap<u32, u32>, // host: damage total already applied per handle
    written: HashMap<u32, u32>, // client: hp we wrote last frame per handle
    dealt: HashMap<u32, u32>,   // client: damage total dealt per handle
    paired: HashMap<u32, u32>,  // client: host handle -> our handle (each game numbers its objects itself)
    anim_seen: HashMap<u32, u32>, // client: host handle -> last host anim_seq seen
    logged: bool,
    frames: u32, // host: frames with no enemy found, for the one-time name dump
}
static WORLD: Mutex<Option<World>> = Mutex::new(None);

pub fn tick(pl: *mut Pl0000) {
    let mut guard = state::lock(&WORLD);
    if !WORLD_SYNC.load(Relaxed) || !REMOTE_ACTIVE.load(Relaxed) {
        if guard.take().is_some() {
            enemy_anim::reset();
        }
        *guard = None; // peer gone: forget everything, handles mean nothing in the next session
        return;
    }
    let w = guard.get_or_insert_with(World::default);
    if HOST.load(Relaxed) { host(w, pl) } else { client(w) }
}

/// Synced objects only: enemies (type id 0x2xxxx), never a `Pl0000` (players, buddy). Writing health to an object
/// that is not a BehaviorAppBase would corrupt it, so the type filter is the safety check. Returns the type id as kind.
fn enemy(handle: u32, kind: Option<u32>) -> Option<(*mut Pl0000, u32)> {
    let e = entity_by_handle(handle)?;
    let k = unsafe { addr_of_mut!((*e).object_id).read() };
    let b = unsafe { addr_of_mut!((*e).behavior).read() };
    let vt = unsafe { b.as_ref()? }.vtable as usize;
    let pl = PL_VTABLE.load(Relaxed);
    // shortcut: a game vtable + enemy type id is the type check; an allowlist of enemy vtables would be stricter, add one if a non-Behavior 0x2xxxx object shows up.
    let ok = pl != 0 && vt != pl && in_module(vt) && k >> 16 == 2;
    (ok && kind.is_none_or(|x| x == k)).then_some((b, k))
}

/// Local death or respawn: forget every pairing and damage total, so the host sends all enemies fresh and no old
/// damage total lands on a reloaded enemy that got a reused handle.
pub fn reset() {
    *state::lock(&WORLD) = None;
    enemy_anim::reset();
    state::lock(&LOCAL_WORLD).clear();
    state::lock(&LOCAL_DAMAGE).clear();
    state::lock(&REMOTE_DAMAGE).clear();
}

fn host(w: &mut World, pl: *mut Pl0000) {
    // The client's hits, as totals: apply only what is new since the last packet, and only to enemies sent since the last reset.
    for d in std::mem::take(&mut *state::lock(&REMOTE_DAMAGE)) {
        if !w.seen.contains(&d.handle) {
            continue;
        }
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
        if ENEMY_ANIM.load(Relaxed) {
            enemy_anim::hook(b);
        }
        let hp = unsafe { addr_of_mut!((*b).health).read() };
        if hp > 0 {
            w.seen.insert(handle);
        } else if !w.seen.contains(&handle) {
            return;
        }
        let yaw = unsafe { addr_of_mut!((*b).rotation).read().y };
        let (anim, anim_seq) = enemy_anim::last(b);
        snap.push(EnemyState { handle, kind, hp, yaw, pos, anim, anim_seq });
    });
    snap.sort_by(|a, b| dist2(a.pos, center).total_cmp(&dist2(b.pos, center)));
    snap.truncate(MAX_ENEMIES);
    if !w.logged && !snap.is_empty() {
        w.logged = true;
        tracing::info!("world sync (host): sending {} enemies", snap.len());
    } else if !w.logged {
        w.frames += 1;
        if w.frames % 1800 == 0 && w.frames <= 1800 * 5 {
            // Every ~30 s (5 times) with no "em*" enemy found: dump each distinct name, so the filter can be fixed.
            let mut names = Vec::new();
            for_each_object(|handle, b, _| {
                let Some(e) = entity_by_handle(handle) else { return };
                let name = unsafe { addr_of_mut!((*e).name).read() };
                let vt = unsafe { b.as_ref() }.map_or(0, |o| o.vtable as usize);
                let n = String::from_utf8_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())]).into_owned();
                let id = unsafe { addr_of_mut!((*e).object_id).read() };
                let s = format!("{n}#{id:x}(vt+{:#x})",vt.wrapping_sub(super::MODULE.get().map_or(0, |m| m.as_ptr() as usize)));
                if !names.contains(&s) {
                    names.push(s);
                }
            });
            tracing::warn!("world sync (host): no enemies found; objects here: {}", names.join(" "));
        }
    }
    *state::lock(&LOCAL_WORLD) = snap;
}

fn client(w: &mut World) {
    let snap = match &*state::lock(&REMOTE_WORLD) {
        Some((at, s)) if at.elapsed() < STALE => s.clone(),
        _ => {
            enemy_anim::set_blocked([]); // host loading or gone: our own AI drives our enemies again
            return;
        }
    };
    let anim = ENEMY_ANIM.load(Relaxed);
    let mut driven = Vec::new();
    w.paired.retain(|_, ours| enemy(*ours, None).is_some());
    let mut free: Vec<(u32, u32, cVec4)> = Vec::new(); // our unpaired enemies: handle, kind, position
    if snap.iter().any(|e| !w.paired.contains_key(&e.handle)) {
        for_each_object(|h, _, pos| {
            if let Some((_, k)) = enemy(h, None) {
                if !w.paired.values().any(|&o| o == h) {
                    free.push((h, k, pos));
                }
            }
        });
    }
    let mut matched = 0;
    for e in &snap {
        if !w.paired.contains_key(&e.handle) {
            // Pair with our nearest unpaired enemy of the same type; a one-of-a-kind enemy (a boss) at any distance,
            // since after separate reloads it can stand far from where the host's is.
            let unique = snap.iter().filter(|o| o.kind == e.kind).count() == 1 && free.iter().filter(|f| f.1 == e.kind).count() == 1;
            let best = free
                .iter()
                .enumerate()
                .filter(|(_, f)| f.1 == e.kind && (unique || dist2(f.2, e.pos) < MATCH_DIST * MATCH_DIST))
                .min_by(|a, b| dist2(a.1.2, e.pos).total_cmp(&dist2(b.1.2, e.pos)))
                .map(|(i, _)| i);
            let Some(i) = best else { continue };
            w.paired.insert(e.handle, free.swap_remove(i).0);
        }
        let Some((b, _)) = enemy(w.paired[&e.handle], Some(e.kind)) else { continue };
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
        if anim {
            enemy_anim::hook(b);
            driven.push(b);
            if replay(w.anim_seen.insert(e.handle, e.anim_seq), e.anim_seq) {
                enemy_anim::apply(b, e.anim);
            }
        }
    }
    enemy_anim::set_blocked(driven);
    w.frames += 1;
    if !w.logged || w.frames % 600 == 0 {
        w.logged = true;
        tracing::info!("world sync (client): {matched}/{} host enemies matched here", snap.len());
    }
    let mut out: Vec<Damage> =
        snap.iter().filter_map(|e| w.dealt.get(&e.handle).filter(|&&t| t > 0).map(|&total| Damage { handle: e.handle, total })).collect();
    out.truncate(MAX_ENEMIES);
    *state::lock(&LOCAL_DAMAGE) = out;
}

/// Client: replay the host's last enemy action? Not on first sight (an old call would restart an action midway).
fn replay(prev_seq: Option<u32>, seq: u32) -> bool {
    seq != 0 && prev_seq.is_some_and(|p| p != seq)
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
    fn replay_only_new_actions() {
        assert!(!replay(None, 4)); // first sight
        assert!(!replay(Some(4), 4)); // same action, sent again
        assert!(replay(Some(4), 5));
        assert!(!replay(Some(4), 0)); // host reset: nothing recorded yet
    }

    #[test]
    fn reset_forgets_pairings_and_damage() {
        let mut w = World::default();
        w.seen.insert(1);
        w.paired.insert(1, 2);
        *state::lock(&WORLD) = Some(w);
        state::lock(&LOCAL_DAMAGE).push(Damage { handle: 1, total: 5 });
        state::lock(&REMOTE_DAMAGE).push(Damage { handle: 1, total: 5 });
        reset();
        assert!(state::lock(&WORLD).is_none()); // host re-sends every enemy, client re-pairs
        assert!(state::lock(&LOCAL_DAMAGE).is_empty() && state::lock(&REMOTE_DAMAGE).is_empty()); // no old totals on reused handles
    }

    #[test]
    fn pull_and_snap() {
        let o = cVec4::default();
        assert_eq!(pull(o, cVec4 { x: 2.0, ..o }).x, 1.0);
        assert_eq!(pull(o, cVec4 { x: 20.0, ..o }).x, 20.0);
    }
}
