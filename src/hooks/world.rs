//! World sync (`world_sync=true`, one player `host=true`): the host's game owns the enemies, the client copies
//! them. Nobody waits: the host sends a snapshot every frame, the client pulls its enemies toward it and sends
//! back the damage it dealt, which the host applies and acknowledges. Game thread only.
//!
//! Damage totals are counted per generation (`state::WorldGen`): each side starts a new one whenever it forgets
//! its state (death, respawn, hacking, peer timeout), and totals counted against another generation are never
//! applied, so no total is applied twice and none is ignored after a reset.

use std::collections::{HashMap, HashSet};
use std::ptr::addr_of_mut;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::Duration;

use super::{PL_VTABLE, enemy_anim, entity_by_handle, for_each_object, in_module};
use crate::sdk::{Pl0000, cVec4};
use crate::state::{
    self, Damage, ENEMY_ANIM, EnemyState, HOST, LOCAL_DAMAGE, LOCAL_WORLD, MAX_ENEMIES, REMOTE_ACTIVE, REMOTE_CORRECTION, REMOTE_DAMAGE,
    REMOTE_WORLD, WORLD_SYNC, WorldGen,
};

/// A snapshot older than this is not applied (host loading or gone): the client's own AI keeps running.
const STALE: Duration = Duration::from_millis(500);
/// Past this distance (m) a client enemy is teleported to the host's position, else pulled by `PULL` per frame.
const SNAP_DIST: f32 = 10.0;
const PULL: f32 = 0.5;
/// A host enemy is paired with our nearest unpaired enemy of the same type within this distance (m). Wide on
/// purpose: when this game loads an enemy group later than the host (seen in a session log: 57 s later), its sets
/// spawn a fresh wave at their spawn points, far from where the host's survivors fight. With the old 30 m limit
/// those never paired and the client saw the host fight nothing. Once paired they are snapped to the host's spot.
const MATCH_DIST: f32 = 150.0;
/// Pairings farther than this (m) are logged: they mean the two games spawned this enemy out of step.
const FAR_PAIR: f32 = 30.0;
/// Frames a killed enemy is still sent at hp 0 (so the client sees the kill) before it stops taking a slot.
const DEAD_FRAMES: u32 = 120;
/// Frames between status lines (~10 s).
const REPORT_EVERY: u32 = 600;

/// Generations of this game's world-sync state; a fresh `World` takes the next one.
static NEXT_GEN: AtomicU32 = AtomicU32::new(1);

#[derive(Default)]
struct World {
    generation: u32, // host: world generation; client: damage-total generation
    seen: HashSet<u32>,           // host: handles sent alive, still sent at hp 0 so the client sees the kill
    dead: HashMap<u32, u32>,      // host: frames each killed enemy has been sent at hp 0
    applied: HashMap<u32, u32>,   // host: client damage total already applied per handle, for `client_gen`
    client_gen: u32,              // host: the client generation `applied` belongs to (0: none yet)
    host_gen: Option<u32>,        // client: the host generation `dealt` and `written` belong to
    written: HashMap<u32, u32>,   // client: hp we wrote last frame per handle
    dealt: HashMap<u32, u32>,     // client: damage total dealt per handle
    paired: HashMap<u32, u32>,    // client: host handle -> our handle (each game numbers its objects itself)
    anim_seen: HashMap<u32, u32>, // client: host handle -> last host anim_seq seen
    far_logs: u32,                // client: far pairings logged so far
    logged: bool,
    frames: u32,
}
static WORLD: Mutex<Option<World>> = Mutex::new(None);

impl World {
    fn new() -> Self {
        Self { generation: NEXT_GEN.fetch_add(1, Relaxed), ..Default::default() }
    }
}

pub fn tick(pl: *mut Pl0000) {
    let mut guard = state::lock(&WORLD);
    if !WORLD_SYNC.load(Relaxed) || !REMOTE_ACTIVE.load(Relaxed) {
        if guard.take().is_some() {
            enemy_anim::reset();
        }
        *guard = None; // peer gone: forget everything, handles mean nothing in the next session
        return;
    }
    let w = guard.get_or_insert_with(World::new);
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
/// damage total lands on a reloaded enemy that got a reused handle. The next `World` is a new generation.
pub fn reset() {
    *state::lock(&WORLD) = None;
    enemy_anim::reset();
    state::lock(&LOCAL_WORLD).1.clear();
    state::lock(&LOCAL_DAMAGE).1.clear();
    *state::lock(&REMOTE_DAMAGE) = None;
}

fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Host: whether a damage list counted against generations `g` applies here. A newer client generation (the client
/// reset) restarts the applied totals; a list for another host world, or from an older client generation (reordered
/// packet), is dropped.
fn accept(w: &mut World, g: WorldGen) -> bool {
    if g.host != w.generation {
        return false;
    }
    if g.client != w.client_gen {
        if w.client_gen != 0 && !newer(g.client, w.client_gen) {
            return false;
        }
        if w.client_gen != 0 {
            tracing::info!("world sync (host): the client reset; its damage totals restart");
        }
        w.client_gen = g.client;
        w.applied.clear();
    }
    true
}

fn host(w: &mut World, pl: *mut Pl0000) {
    // The client's hits, as totals: apply only what is new since the last packet, and only to enemies sent since the last reset.
    if let Some((g, list)) = state::lock(&REMOTE_DAMAGE).take() {
        if accept(w, g) {
            for d in list {
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
        }
    }
    w.frames += 1;
    if w.frames % REPORT_EVERY == 0 {
        // Handles of removed enemies never come back (the engine bumps their generation bits).
        w.seen.retain(|&h| entity_by_handle(h).is_some());
        w.dead.retain(|&h, _| entity_by_handle(h).is_some());
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
            w.dead.remove(&handle);
        } else if !w.seen.contains(&handle) {
            return;
        } else {
            // A corpse is sent long enough for the client to see the kill, then stops taking one of the slots.
            let n = w.dead.entry(handle).or_insert(0);
            *n += 1;
            if *n > DEAD_FRAMES {
                return;
            }
        }
        let yaw = unsafe { addr_of_mut!((*b).rotation).read().y };
        let (anim, anim_seq) = enemy_anim::last(b);
        let applied = w.applied.get(&handle).copied().unwrap_or(0);
        snap.push(EnemyState { handle, kind, hp, yaw, pos, anim, anim_seq, applied });
    });
    // Live enemies first, nearest the client first.
    snap.sort_by(|a, b| (a.hp == 0).cmp(&(b.hp == 0)).then(dist2(a.pos, center).total_cmp(&dist2(b.pos, center))));
    snap.truncate(MAX_ENEMIES);
    if !w.logged && !snap.is_empty() {
        w.logged = true;
        tracing::info!("world sync (host): sending {} enemies", snap.len());
    } else if !w.logged && w.frames % 1800 == 0 && w.frames <= 1800 * 5 {
        // Every ~30 s (5 times) with no "em*" enemy found: dump each distinct name, so the filter can be fixed.
        let mut names = Vec::new();
        for_each_object(|handle, b, _| {
            let Some(e) = entity_by_handle(handle) else { return };
            let name = unsafe { addr_of_mut!((*e).name).read() };
            let vt = unsafe { b.as_ref() }.map_or(0, |o| o.vtable as usize);
            let n = String::from_utf8_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())]).into_owned();
            let id = unsafe { addr_of_mut!((*e).object_id).read() };
            let s = format!("{n}#{id:x}(vt+{:#x})", vt.wrapping_sub(super::MODULE.get().map_or(0, |m| m.as_ptr() as usize)));
            if !names.contains(&s) {
                names.push(s);
            }
        });
        tracing::warn!("world sync (host): no enemies found; objects here: {}", names.join(" "));
    }
    *state::lock(&LOCAL_WORLD) = (WorldGen { host: w.generation, client: w.client_gen }, snap);
}

fn client(w: &mut World) {
    let (wg, snap) = match &*state::lock(&REMOTE_WORLD) {
        Some((at, g, s)) if at.elapsed() < STALE => (*g, s.clone()),
        _ => {
            enemy_anim::set_blocked([]); // host loading or gone: our own AI drives our enemies again
            return;
        }
    };
    if w.host_gen != Some(wg.host) {
        // The host forgot its applied totals: ours start again, against its new world.
        if w.host_gen.is_some() {
            tracing::info!("world sync (client): the host reset; damage totals restart");
        }
        w.host_gen = Some(wg.host);
        w.dealt.clear();
        w.written.clear();
    }
    // The host's `applied` totals are ours only once it has seen our current generation.
    let acked_here = wg.client == w.generation;
    w.frames += 1;
    let report = !w.logged || w.frames % REPORT_EVERY == 0;
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
    let (mut matched, mut unmatched) = (0, Vec::new());
    for e in &snap {
        if !w.paired.contains_key(&e.handle) {
            // A host corpse is never paired: writing its hp 0 would stall a live enemy of ours.
            if e.hp == 0 {
                continue;
            }
            // Pair with our nearest unpaired enemy of the same type; a one-of-a-kind enemy (a boss) at any distance,
            // since after separate reloads it can stand far from where the host's is.
            let unique = snap.iter().filter(|o| o.kind == e.kind).count() == 1 && free.iter().filter(|f| f.1 == e.kind).count() == 1;
            let best = free
                .iter()
                .enumerate()
                .filter(|(_, f)| f.1 == e.kind && (unique || dist2(f.2, e.pos) < MATCH_DIST * MATCH_DIST))
                .min_by(|a, b| dist2(a.1.2, e.pos).total_cmp(&dist2(b.1.2, e.pos)))
                .map(|(i, _)| i);
            let Some(i) = best else {
                if report && unmatched.len() < 8 {
                    let near = free.iter().filter(|f| f.1 == e.kind).map(|f| dist2(f.2, e.pos).sqrt()).min_by(f32::total_cmp);
                    unmatched.push(match near {
                        Some(d) => format!("{:#x} (nearest of that type here: {d:.0} m)", e.kind),
                        None => format!("{:#x} (none of that type loaded here)", e.kind),
                    });
                }
                continue;
            };
            let (ours, _, at) = free.swap_remove(i);
            let d = dist2(at, e.pos).sqrt();
            if d > FAR_PAIR && w.far_logs < 20 {
                w.far_logs += 1;
                tracing::info!("world sync (client): host enemy {:#x} paired with ours {d:.0} m away (spawned out of step); snapping it over", e.kind);
            }
            w.paired.insert(e.handle, ours);
        }
        let Some((b, _)) = enemy(w.paired[&e.handle], Some(e.kind)) else { continue };
        matched += 1;
        unsafe {
            let hp = addr_of_mut!((*b).health);
            let acked = if acked_here { e.applied } else { 0 };
            let total = w.dealt.get(&e.handle).copied().unwrap_or(0);
            let (write, total) = merge_hp(e.hp, hp.read(), w.written.get(&e.handle).copied(), total, acked);
            w.dealt.insert(e.handle, total);
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
    if report {
        w.logged = true;
        tracing::info!("world sync (client): {matched}/{} host enemies matched here", snap.iter().filter(|e| e.hp > 0).count());
        if !unmatched.is_empty() {
            tracing::warn!("world sync (client): host enemies with no copy here: {}", unmatched.join(", "));
        }
    }
    let mut out: Vec<Damage> =
        snap.iter().filter_map(|e| w.dealt.get(&e.handle).filter(|&&t| t > 0).map(|&total| Damage { handle: e.handle, total })).collect();
    out.truncate(MAX_ENEMIES);
    *state::lock(&LOCAL_DAMAGE) = (WorldGen { host: wg.host, client: w.generation }, out);
}

/// Client: replay the host's last enemy action? Not on first sight (an old call would restart an action midway).
fn replay(prev_seq: Option<u32>, seq: u32) -> bool {
    seq != 0 && prev_seq.is_some_and(|p| p != seq)
}

/// Client, one enemy: (hp to write, our new damage total). Our hits since last frame are added to the total, and
/// the part the host has not yet acknowledged applying is taken off the host's hp: our hit shows at once, both
/// agree once the host applies it, and our copy never stays below the host's (the old `min(host, ours)` kept a
/// copy low forever whenever the host missed a total, so the client killed copies the host still fought).
fn merge_hp(host_hp: u32, now: u32, written: Option<u32>, total: u32, acked: u32) -> (u32, u32) {
    let total = total.saturating_add(written.map_or(0, |w| w.saturating_sub(now)));
    (host_hp.saturating_sub(total.saturating_sub(acked)), total)
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
        assert_eq!(merge_hp(100, 100, None, 0, 0), (100, 0)); // first sight
        assert_eq!(merge_hp(100, 30, None, 0, 0), (100, 0)); // a copy that drifted low takes the host's value
        assert_eq!(merge_hp(100, 80, Some(100), 0, 0), (80, 20)); // our hit: shown now, 20 reported
        assert_eq!(merge_hp(100, 80, Some(80), 20, 0), (80, 20)); // host has not applied it yet: no flicker back to 100
        assert_eq!(merge_hp(80, 80, Some(80), 20, 20), (80, 20)); // applied and acknowledged
        assert_eq!(merge_hp(60, 80, Some(80), 20, 20), (60, 20)); // host's own hits come through
        assert_eq!(merge_hp(0, 60, Some(60), 20, 20), (0, 20)); // host killed it
    }

    #[test]
    fn host_accepts_only_current_generations() {
        let mut w = World::new();
        let g = w.generation;
        assert!(!accept(&mut w, WorldGen { host: g.wrapping_add(1), client: 5 })); // for another host world
        assert!(accept(&mut w, WorldGen { host: g, client: 5 }));
        w.applied.insert(1, 50);
        assert!(accept(&mut w, WorldGen { host: g, client: 5 }));
        assert_eq!(w.applied[&1], 50);
        assert!(!accept(&mut w, WorldGen { host: g, client: 4 })); // older client generation (reordered packet)
        assert!(accept(&mut w, WorldGen { host: g, client: 6 })); // client reset: its totals restart from 0
        assert!(w.applied.is_empty());
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
        let mut w = World::new();
        w.seen.insert(1);
        w.paired.insert(1, 2);
        *state::lock(&WORLD) = Some(w);
        state::lock(&LOCAL_DAMAGE).1.push(Damage { handle: 1, total: 5 });
        *state::lock(&REMOTE_DAMAGE) = Some((WorldGen::default(), vec![Damage { handle: 1, total: 5 }]));
        reset();
        assert!(state::lock(&WORLD).is_none()); // host re-sends every enemy, client re-pairs
        assert!(state::lock(&LOCAL_DAMAGE).1.is_empty() && state::lock(&REMOTE_DAMAGE).is_none()); // no old totals on reused handles
        assert_ne!(World::new().generation, World::new().generation); // and the next session is a new generation
    }

    #[test]
    fn pull_and_snap() {
        let o = cVec4::default();
        assert_eq!(pull(o, cVec4 { x: 2.0, ..o }).x, 1.0);
        assert_eq!(pull(o, cVec4 { x: 20.0, ..o }).x, 20.0);
    }
}
