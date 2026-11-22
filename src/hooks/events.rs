//! Cutscene sync (`world_sync=true`, one player `host=true`). Every scene action (cutscenes = PhaseEventAction,
//! triggers, quest steps) changes state through `Action::request_state`, hooked here. When a cutscene starts playing
//! on the host, its layout id is sent; the client finds its own action with that id and calls the game's
//! PhaseEventAction.play on it (which loads it first if needed). Scene signals (how scripts trigger actions: an
//! NPC talk that blows up debris, a door, a lift) are forwarded the same way: the host's post is hooked, the client
//! posts the same signal from its own action with that id. Action.signal cannot be used for that: outside the script
//! engine it finds no sender context and sends from "nobody", which no listener reacts to. So each action's context
//! is learned from the signals the client's own game posts. State changes and signals are also logged.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use retour::RawDetour;

use super::quest::text;
use super::spawn::script_fn;
use crate::scan;
use crate::sdk::{ActionScene, HashedStr, SceneAction, patterns};
use crate::state::{self, HOST, LOCAL_EVENTS, LOCAL_SIGNALS, REMOTE_ACTIVE, REMOTE_EVENTS, REMOTE_SIGNALS, WORLD_SYNC};

static TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);
static POST_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);
static LINES: AtomicU32 = AtomicU32::new(0);
/// Id of the action whose peer event is being applied (0: none), so its own state changes are not sent back.
static APPLYING: AtomicU32 = AtomicU32::new(0);
const MAX_LINES: u32 = 5000;
/// Set when a cutscene finishes: the cutscene suspended/re-AI'd the spawned 2B, `on_tick` sets her up again.
pub static EVENT_ENDED: AtomicBool = AtomicBool::new(false);
/// PhaseEventAction state hashes (names from the game's state registry).
const PLAYING: u32 = 0xecbccdf8;
const PLAY_END: u32 = 0xcee3b8c9;
/// Unnamed state between LOADED and PLAYING: play was requested.
const PLAY_REQ: u32 = 0xa09a3421; // reached the last frames, holding
const DONE: u32 = 0x26b80d1f;
/// HapGoalLocationAction: WAIT (0x4b7b6e51) shows the objective, then its update moves to ACTIVE.
const ACTIVE: u32 = 0xbbe63b84;
const WAIT: u32 = 0x4b7b6e51;
/// EnemySetArea (exe 2026-10-09, update 0x3a08e0): WAIT until its area condition holds, SPAWN_DELAY counts down,
/// SPAWN spawns the set, then BATTLE.
const SPAWN_DELAY: u32 = 0x45ce422b;
const SPAWN: u32 = 0x3aa89d11;
/// How long a host spawn waits for the client's (loaded) enemy set to start.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a host cutscene waits for the one playing here to end.
const PLAY_TIMEOUT: Duration = Duration::from_secs(60);
/// Client: (cutscene id, queued) the host played, waiting for the one playing here.
static PENDING_PLAYS: std::sync::Mutex<Vec<(u32, Instant)>> = std::sync::Mutex::new(Vec::new());
/// Client: (enemy set id, queued) the host spawned, loaded here but not yet started.
static PENDING_SPAWNS: std::sync::Mutex<Vec<(u32, Instant)>> = std::sync::Mutex::new(Vec::new());
/// Host events whose action is not loaded here yet, retried until it is: each game loads action groups by itself,
/// and a session log showed the client load a boss-fight group 57 s after the host had spawned its 26 enemy sets.
/// Dropping those events left the client without the host's enemies for the rest of the fight.
static PENDING_LOADS: std::sync::Mutex<Vec<([u32; 4], Instant)>> = std::sync::Mutex::new(Vec::new());
/// How long an event waits for its action to load here; a cutscene only as long as `PLAY_TIMEOUT`.
const LOAD_TIMEOUT: Duration = Duration::from_secs(180);
/// Frames between retries of `PENDING_LOADS` (each walks every loaded action).
const RETRY_EVERY: u32 = 30;
const MAX_PENDING_LOADS: usize = 512;
static TICKS: AtomicU32 = AtomicU32::new(0);
const MAX_GROUPS: usize = 0x10000;
const MAX_ACTIONS: usize = 0x10000;
/// A cutscene id received again within this time is the same start (sent several times against loss).
const REPEAT: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct Addr {
    scene: usize,    // static ActionScene
    phase_vt: usize, // PhaseEventAction vtable
    send_vt: usize,  // SendCommand vtable
    goal_vt: usize,  // HapGoalLocationAction vtable
    enemy_vt: usize, // EnemySetArea vtable
    play: usize,     // bool PhaseEventAction.play(this)
    queue: usize,    // scene signal queue
    build: usize,    // StateReq* build(StateReq* out, u32 hash): fills hash and the registered name (+1 ref)
}
static ADDR: OnceLock<Addr> = OnceLock::new();
static LAST: std::sync::Mutex<Option<([u32; 3], Instant)>> = std::sync::Mutex::new(None);
/// Sender context last seen per action id (from the game's own signals), with the action it came from. Areas
/// stream in and out while the player stays alive, so an id can be unloaded and loaded again with a new context:
/// a context is only used while the action with that id is still the same object.
static CONTEXTS: std::sync::Mutex<Option<std::collections::HashMap<u32, (usize, usize)>>> = std::sync::Mutex::new(None);

/// Argument of `Action::request_state` (built by the engine from a state hash).
#[repr(C)]
struct StateReq {
    hash: u32,              // 0x00
    _pad_04: u32,           // 0x04
    name: *const HashedStr, // 0x08 null if the hash has no registered name
    flag: u32,              // 0x10
}

/// `request_state` is the call at +0x8C in PhaseEventAction.load; its first bytes are checked too.
fn resolve_request(m: &[u8], load: usize) -> Option<usize> {
    let at = load + 0x8C;
    let t = (m.get(at) == Some(&0xE8)).then(|| scan::rip_target(m, at, 1, 5))??;
    scan::matches_at(m, t, &scan::parse(patterns::REQUEST_STATE)?).then_some(t)
}

/// In Action.signal: `lea rcx,[queue]` at +0x48 and the `post` call at +0x53; `build` is the call at +0x77 in
/// PhaseEventAction.load. Returns (post, build, queue).
fn resolve_signal(m: &[u8], signal: usize, load: usize) -> Option<(usize, usize, usize)> {
    let call = |at: usize| (m.get(at) == Some(&0xE8)).then(|| scan::rip_target(m, at, 1, 5)).flatten();
    let post = call(signal + 0x53)?;
    let queue = (m.get(signal + 0x48..signal + 0x4B) == Some(&[0x48, 0x8D, 0x0D])).then(|| scan::rip_target(m, signal + 0x48, 3, 7))??;
    scan::matches_at(m, post, &scan::parse(patterns::SIGNAL_POST)?).then_some((post, call(load + 0x77)?, queue))
}

/// Module offsets of the action scene and the PhaseEventAction, SendCommand and HapGoalLocationAction vtables.
/// `image_base` is what absolute pointers in `m` are relative to (the module address at runtime, the PE ImageBase
/// for a file image).
fn resolve(m: &[u8], image_base: usize) -> Result<(usize, usize, usize, usize), String> {
    let at = scan::parse(patterns::ACTION_SCENE).and_then(|p| scan::find_unique(m, &p)).ok_or("ACTION_SCENE not found")?;
    let scene = scan::rip_target(m, at + 0x14, 3, 7).ok_or("ACTION_SCENE: bad rip target")?;
    let vt = |class| rtti_vtable(m, image_base, class);
    Ok((scene, vt(patterns::PHASE_EVENT_RTTI)?, vt(patterns::SEND_COMMAND_RTTI)?, vt(patterns::GOAL_RTTI)?))
}

/// RTTI: type name -> TypeDescriptor (name - 0x10) -> CompleteObjectLocator (signature 1, offset 0) -> vtable.
fn rtti_vtable(m: &[u8], image_base: usize, class: &str) -> Result<usize, String> {
    let name: Vec<_> = class.bytes().chain([0]).map(Some).collect();
    let td = (scan::find_unique(m, &name).ok_or(format!("{class}: RTTI not found"))? - 0x10) as u32;
    let u32_at = |o: usize| u32::from_le_bytes(m[o..o + 4].try_into().unwrap());
    let col = (0x0C..m.len() - 4)
        .step_by(4)
        .find(|&o| u32_at(o) == td && u32_at(o - 0x0C) == 1 && u32_at(o - 8) == 0)
        .ok_or(format!("{class}: locator not found"))?
        - 0x0C;
    let ptr = (image_base + col).to_le_bytes();
    Ok((8..m.len() - 8).step_by(8).find(|&o| m[o..o + 8] == ptr).ok_or(format!("{class}: vtable not found"))? + 8)
}

pub fn install(m: &[u8], base: usize) -> Result<(), String> {
    let load = script_fn("PhaseEventAction.load").ok_or("no PhaseEventAction.load")?;
    let play = script_fn("PhaseEventAction.play").ok_or("no PhaseEventAction.play")?;
    let signal = script_fn("Action.signal").ok_or("no Action.signal")?;
    let f = resolve_request(m, load - base).ok_or("request_state not found")?;
    let (post, build, queue) = resolve_signal(m, signal - base, load - base).ok_or("signal post not found")?;
    let (scene, vt, send, goal) = resolve(m, base)?;
    let a = Addr { scene: base + scene, phase_vt: base + vt, send_vt: base + send, goal_vt: base + goal, enemy_vt: base + rtti_vtable(m, base, patterns::ENEMY_SET_RTTI)?, play, queue: base + queue, build: base + build };
    tracing::info!("cutscene sync: {a:x?}");
    let _ = ADDR.set(a);
    for (at, to, tramp) in [(f, on_request as *const (), &TRAMPOLINE), (post, on_post as *const (), &POST_TRAMPOLINE)] {
        unsafe {
            let d = RawDetour::new((base + at) as *const (), to).map_err(|e| e.to_string())?;
            tramp.store(d.trampoline() as *const () as usize, Relaxed);
            d.enable().map_err(|e| e.to_string())?;
            Box::leak(Box::new(d));
        }
    }
    tracing::info!("event log: request_state hooked at {:#x}, signal post at {:#x}", base + f, base + post);
    Ok(())
}

/// `bool Action::request_state(this, const StateReq*)`; `this` is a `SceneAction`.
extern "system" fn on_request(this: *mut SceneAction, req: *const StateReq) -> usize {
    let orig = unsafe { std::mem::transmute::<usize, extern "system" fn(*mut SceneAction, *const StateReq) -> usize>(TRAMPOLINE.load(Relaxed)) };
    let (Some(act), Some(_)) = (unsafe { this.as_ref() }, unsafe { req.as_ref() }) else {
        return orig(this, req);
    };
    let (id, old, vt) = (act.id, act.state, act.vtable);
    let r = orig(this, req);
    let q = unsafe { &*req };
    let cutscene = ADDR.get().is_some_and(|a| a.phase_vt == vt);
    if cutscene && q.hash == DONE && (old == PLAYING || old == PLAY_END) {
        EVENT_ENDED.store(true, Relaxed);
    }
    let sending = HOST.load(Relaxed) && WORLD_SYNC.load(Relaxed) && REMOTE_ACTIVE.load(Relaxed);
    if sending && cutscene && q.hash == PLAYING && old != PLAYING {
        tracing::info!("cutscene sync: host plays {id:08x}");
        state::lock(&LOCAL_EVENTS).push([id, 0, q.hash, q.flag]);
    }
    // A SendCommand reaches DONE right after running its commands (the explosion itself).
    if sending && q.hash == DONE && old != DONE && ADDR.get().is_some_and(|a| a.send_vt == vt) {
        tracing::info!("cutscene sync: host ran SendCommand {id:08x}");
        state::lock(&LOCAL_EVENTS).push([id, 1, q.hash, q.flag]);
    }
    // Quest objectives advance on things that may only happen here (a camera shot ending): mirror their states.
    // Not ACTIVE: the objective's own update sets the HUD objective and map marker while in WAIT, then requests
    // ACTIVE itself; mirroring ACTIVE skipped that step on the client.
    let goal = q.hash != old && ADDR.get().is_some_and(|a| a.goal_vt == vt) && APPLYING.load(Relaxed) != id;
    if sending && goal && q.hash != ACTIVE {
        tracing::info!("cutscene sync: host objective {id:08x} -> {:08x}", q.hash);
        state::lock(&LOCAL_EVENTS).push([id, 2, q.hash, q.flag]);
    }
    // An enemy set spawns when the host's player enters its area; the client's never would (the area is empty there).
    if sending && q.hash == SPAWN && old != SPAWN && ADDR.get().is_some_and(|a| a.enemy_vt == vt) {
        tracing::info!("cutscene sync: host spawns enemy set {id:08x}");
        state::lock(&LOCAL_EVENTS).push([id, 3, q.hash, 0]);
    }
    // The client's player reached an objective first: the host finishes it too (and sends what follows).
    let client = !HOST.load(Relaxed) && WORLD_SYNC.load(Relaxed) && REMOTE_ACTIVE.load(Relaxed);
    if client && goal && q.hash == DONE {
        tracing::info!("cutscene sync: reached objective {id:08x}");
        state::lock(&LOCAL_EVENTS).push([id, 2, q.hash, q.flag]);
    }
    if q.hash != old && LINES.load(Relaxed) < MAX_LINES {
        LINES.fetch_add(1, Relaxed);
        let name = unsafe { q.name.as_ref() }.and_then(|n| text(n.text)).unwrap_or_default();
        let base = super::MODULE.get().map_or(0, |m| m.as_ptr() as usize);
        tracing::info!("event: action {id:08x} vt+{:#x} {old:08x} -> {name} {:08x} flag {} ok {}", vt.wrapping_sub(base), q.hash, q.flag, r & 0xFF);
    }
    r
}

/// Who posted a signal: a scene context and the sender action's id (0 without a context).
#[repr(C)]
struct Sender {
    context: usize,  // 0x00
    id: u32,         // 0x08
    _pad_0c: u32,    // 0x0C
}

/// `bool post(queue, const Sender*, const StateReq* signal)`: every scene signal goes through here.
extern "system" fn on_post(queue: usize, from: *const Sender, sig: *const StateReq) -> usize {
    let orig = unsafe { std::mem::transmute::<usize, extern "system" fn(usize, *const Sender, *const StateReq) -> usize>(POST_TRAMPOLINE.load(Relaxed)) };
    let r = orig(queue, from, sig);
    if let (Some(f), Some(q)) = (unsafe { from.as_ref() }, unsafe { sig.as_ref() }) {
        if f.id != 0 && f.context != 0 {
            let mut g = state::lock(&CONTEXTS);
            let m = g.get_or_insert_with(Default::default);
            // Looked up only when new or changed: actions post signals every frame.
            if m.get(&f.id).is_none_or(|&(c, _)| c != f.context) {
                if let Some(act) = ADDR.get().and_then(|a| find(a, f.id, None)) {
                    m.insert(f.id, (f.context, act as usize));
                }
            }
        }
        if f.id != 0 && HOST.load(Relaxed) && WORLD_SYNC.load(Relaxed) && REMOTE_ACTIVE.load(Relaxed) {
            state::lock(&LOCAL_SIGNALS).push([f.id, q.hash, q.flag]);
        }
        if LINES.load(Relaxed) < MAX_LINES {
            LINES.fetch_add(1, Relaxed);
            let name = unsafe { q.name.as_ref() }.and_then(|n| text(n.text)).unwrap_or_default();
            tracing::info!("signal: from {:08x} {name} {:08x} arg {}", f.id, q.hash, q.flag);
        }
    }
    r
}

/// Death or unload: drop the saved sender contexts, which an area unload frees (a stale one would be posted).
pub fn forget_contexts() {
    *state::lock(&CONTEXTS) = None;
}

/// Client, game thread: start the cutscenes the host started and repeat its signals.
pub fn tick() {
    if !REMOTE_ACTIVE.load(Relaxed) {
        // Peer gone: what it ran belongs to a session that is over.
        state::lock(&PENDING_LOADS).clear();
    }
    let sigs = std::mem::take(&mut *state::lock(&REMOTE_SIGNALS));
    if let Some(a) = ADDR.get().filter(|_| !HOST.load(Relaxed)) {
        for [id, hash, arg] in sigs {
            // A context is only trusted while the very action it was seen with is still loaded (an area unload
            // frees both; a reload of that id makes a new action with a new context).
            let entry = state::lock(&CONTEXTS).as_ref().and_then(|c| c.get(&id).copied());
            let here = find(a, id, None).map(|p| p as usize);
            let Some((context, _)) = entry.filter(|&(_, act)| here == Some(act)) else {
                if entry.is_some() {
                    // Reloaded or gone: forget it, the action's next own signal teaches the new context.
                    if let Some(c) = state::lock(&CONTEXTS).as_mut() {
                        c.remove(&id);
                    }
                }
                tracing::debug!("signal sync: sender {id:08x} not loaded here");
                continue;
            };
            let build = unsafe { std::mem::transmute::<usize, extern "system" fn(*mut StateReq, u32) -> *mut StateReq>(a.build) };
            let post = unsafe { std::mem::transmute::<usize, extern "system" fn(usize, *const Sender, *const StateReq) -> usize>(POST_TRAMPOLINE.load(Relaxed)) };
            let mut q = StateReq { hash: 0, _pad_04: 0, name: std::ptr::null(), flag: 0 };
            build(&mut q, hash);
            q.flag = arg;
            post(a.queue, &Sender { context, id, _pad_0c: 0 }, &q);
            let mut n = q.name as *mut HashedStr;
            if !n.is_null() {
                super::quest::release(&mut n);
            }
            if LINES.fetch_add(1, Relaxed) < MAX_LINES {
                tracing::info!("signal sync: {id:08x} {hash:08x} arg {arg}");
            }
        }
    }
    let ids = std::mem::take(&mut *state::lock(&REMOTE_EVENTS));
    let Some(a) = ADDR.get() else { return };
    if !state::lock(&PENDING_SPAWNS).is_empty() {
        spawn_pending(a);
    }
    if !state::lock(&PENDING_PLAYS).is_empty() {
        play_pending(a);
    }
    if !ids.is_empty() {
        apply_events(a, ids);
    }
    if TICKS.fetch_add(1, Relaxed) % RETRY_EVERY == 0 && !state::lock(&PENDING_LOADS).is_empty() {
        retry_loads(a);
    }
    APPLYING.store(0, Relaxed);
}

/// Client: everything the host ran; host: objectives the client reached (filtered by the net thread).
fn apply_events(a: &Addr, ids: Vec<[u32; 4]>) {
    for ev in ids {
        let [id, kind, hash, _] = ev;
        {
            // The reliable channel delivers each event once; this still drops a quick repeat of the same step.
            let mut last = state::lock(&LAST);
            if last.is_some_and(|(l, t)| l == [id, kind, hash] && t.elapsed() < REPEAT) {
                continue;
            }
            *last = Some(([id, kind, hash], Instant::now()));
        }
        if !apply_event(a, ev) {
            let mut p = state::lock(&PENDING_LOADS);
            if p.iter().any(|(e, _)| e[..3] == ev[..3]) {
                continue;
            }
            if p.len() == MAX_PENDING_LOADS {
                p.remove(0);
            }
            tracing::info!("cutscene sync: host ran {id:08x} (kind {kind}), not loaded here yet: waiting for it to load");
            p.push((ev, Instant::now()));
        }
    }
}

fn load_timeout(kind: u32) -> Duration {
    if kind == 0 { PLAY_TIMEOUT } else { LOAD_TIMEOUT }
}

/// Client: the host events whose action has loaded here since, applied now (late, but the host's enemies and
/// doors then exist here too); the rest keep waiting until their timeout.
fn retry_loads(a: &Addr) {
    let pending = std::mem::take(&mut *state::lock(&PENDING_LOADS));
    let mut keep = Vec::new();
    for (ev, t) in pending {
        let [id, kind, ..] = ev;
        if t.elapsed() >= load_timeout(kind) {
            tracing::warn!("cutscene sync: host ran {id:08x} (kind {kind}), never loaded here in {} s; dropped", load_timeout(kind).as_secs());
        } else if apply_event(a, ev) {
            tracing::info!("cutscene sync: {id:08x} (kind {kind}) loaded here {:.0} s after the host ran it; applied", t.elapsed().as_secs_f32());
        } else {
            keep.push((ev, t));
        }
    }
    // Events received meanwhile go after the older ones.
    let mut p = state::lock(&PENDING_LOADS);
    keep.append(&mut p);
    *p = keep;
}

/// Applies one host event; false when its action is not loaded here (yet).
fn apply_event(a: &Addr, [id, kind, hash, flag]: [u32; 4]) -> bool {
    APPLYING.store(id, Relaxed);
    let vt = match kind {
        0 => a.phase_vt,
        1 => a.send_vt,
        2 => a.goal_vt,
        _ => a.enemy_vt,
    };
    let Some(act) = find(a, id, Some(vt)) else { return false };
    {
        let state = unsafe { (*act).state };
        if kind == 3 {
            // Its own update spawns the enemies (then BATTLE) once past this state, area condition or not.
            // Not started here yet (state 0): its start, a moment later, would request WAIT over our spawn.
            state::lock(&PENDING_SPAWNS).push((id, Instant::now()));
            spawn_pending(a);
        } else if kind == 2 {
            let finish = unsafe { (vt as *const usize).add(patterns::GOAL_FINISH).read() };
            if hash == DONE && state != DONE && super::in_module(finish) {
                // Not a bare DONE request: finish also removes the map marker and HUD objective.
                unsafe { std::mem::transmute::<usize, extern "system" fn(*mut SceneAction)>(finish)(act) };
                tracing::info!("cutscene sync: objective {id:08x} finished ({state:08x} -> {:08x})", unsafe { (*act).state });
            } else if state != hash {
                let ok = request(a, act, hash, flag);
                tracing::info!("cutscene sync: objective {id:08x} {state:08x} -> {hash:08x}: ok {}", ok & 0xFF);
                // On the host the update runs right after WAIT; here the objective was never started, so nothing
                // would update it: run it once to show the objective (it moves itself to ACTIVE).
                let update = unsafe { (vt as *const usize).add(patterns::GOAL_UPDATE).read() };
                if hash == WAIT && super::in_module(update) {
                    unsafe { std::mem::transmute::<usize, extern "system" fn(*mut SceneAction)>(update)(act) };
                    tracing::info!("cutscene sync: objective {id:08x} updated -> {:08x}", unsafe { (*act).state });
                }
            }
        } else if kind == 0 && [PLAY_REQ, PLAYING, PLAY_END, DONE].contains(&state) {
            // Started here too (the player triggered it): playing it again left the player stuck in event mode.
            tracing::info!("cutscene sync: {id:08x} already started here (state {state:08x})");
        } else if kind == 0 {
            state::lock(&PENDING_PLAYS).push((id, Instant::now()));
            play_pending(a);
        } else if state != DONE {
            // The run method only writes a status word at +0x0C of `out`; 0x40 bytes is ample.
            let run = unsafe { (vt as *const usize).add(patterns::SEND_COMMAND_RUN).read() };
            if !super::in_module(run) {
                return true;
            }
            let mut out = [0u8; 0x40];
            unsafe { std::mem::transmute::<usize, extern "system" fn(*mut SceneAction, *mut u8)>(run)(act, out.as_mut_ptr()) };
            tracing::info!("cutscene sync: ran SendCommand {id:08x} (state {state:08x} -> {:08x})", unsafe { (*act).state });
        }
    }
    true
}

/// Client: plays the cutscenes the host played, one at a time. Starting one while another still plays here (the
/// client started the previous one later) ran both, and the player stayed locked in event mode afterwards. Drops one
/// that started here meanwhile (the game chains some) or after `PLAY_TIMEOUT`.
fn play_pending(a: &Addr) {
    let started = [PLAY_REQ, PLAYING, PLAY_END];
    if find_by(a, |x| x.vtable == a.phase_vt && started.contains(&x.state)).is_some() {
        return;
    }
    let mut pending = state::lock(&PENDING_PLAYS);
    while !pending.is_empty() {
        let (id, t) = pending.remove(0);
        let Some(act) = find(a, id, Some(a.phase_vt)) else {
            tracing::warn!("cutscene sync: host played {id:08x}, not loaded here");
            continue;
        };
        let state = unsafe { (*act).state };
        if started.contains(&state) || state == DONE {
            tracing::info!("cutscene sync: {id:08x} already started here (state {state:08x})");
            continue;
        }
        if t.elapsed() >= PLAY_TIMEOUT {
            tracing::warn!("cutscene sync: {id:08x} dropped, waited too long");
            continue;
        }
        let play = unsafe { std::mem::transmute::<usize, extern "system" fn(*mut SceneAction) -> u8>(a.play) };
        tracing::info!("cutscene sync: playing {id:08x} (state {state:08x}): ok {}", play(act));
        return; // the next one waits for this one to end
    }
}

/// Client: starts the spawn of the enemy sets the host spawned, once each has started here (WAIT); drops each
/// after `SPAWN_TIMEOUT`, or when it spawned by itself. (A set not loaded here at all waits in `PENDING_LOADS`.)
fn spawn_pending(a: &Addr) {
    state::lock(&PENDING_SPAWNS).retain(|&(id, t)| {
        let Some(act) = find(a, id, Some(a.enemy_vt)) else {
            return t.elapsed() < SPAWN_TIMEOUT;
        };
        let state = unsafe { (*act).state };
        if state == WAIT || state == SPAWN_DELAY {
            let ok = request(a, act, SPAWN, 0);
            tracing::info!("cutscene sync: enemy set {id:08x} {state:08x} -> spawn: ok {}", ok & 0xFF);
            return false;
        }
        if state != 0 {
            return false; // spawning or done already
        }
        if t.elapsed() >= SPAWN_TIMEOUT {
            tracing::warn!("cutscene sync: enemy set {id:08x} never started here");
            return false;
        }
        true
    });
}

/// `act.request_state(hash)` with the registered state name, as the engine builds it; returns its bool.
fn request(a: &Addr, act: *mut SceneAction, hash: u32, flag: u32) -> usize {
    let build = unsafe { std::mem::transmute::<usize, extern "system" fn(*mut StateReq, u32) -> *mut StateReq>(a.build) };
    let request = unsafe { std::mem::transmute::<usize, extern "system" fn(*mut SceneAction, *const StateReq) -> usize>(TRAMPOLINE.load(Relaxed)) };
    let mut q = StateReq { hash: 0, _pad_04: 0, name: std::ptr::null(), flag: 0 };
    build(&mut q, hash);
    q.flag = flag;
    let ok = request(act, &q);
    let mut n = q.name as *mut HashedStr;
    if !n.is_null() {
        super::quest::release(&mut n);
    }
    ok
}

/// Our action with this layout id (and vtable, when given).
fn find(a: &Addr, id: u32, vt: Option<usize>) -> Option<*mut SceneAction> {
    find_by(a, |x| x.id == id && vt.is_none_or(|v| x.vtable == v))
}

/// First loaded action matching `f`.
fn find_by(a: &Addr, f: impl Fn(&SceneAction) -> bool) -> Option<*mut SceneAction> {
    let s = unsafe { (a.scene as *const ActionScene).as_ref() }?;
    if s.groups.is_null() || s.count > MAX_GROUPS {
        return None;
    }
    for i in 0..s.count {
        let Some(g) = (unsafe { s.groups.add(i).read().as_ref() }) else { continue };
        if g.actions.is_null() || g.count > MAX_ACTIONS {
            continue;
        }
        for j in 0..g.count {
            let p = unsafe { g.actions.add(j).read() };
            if unsafe { p.as_ref() }.is_some_and(&f) {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    /// `NIER_EXE=<exe> cargo test events -- --ignored`; offsets from the exe of 2026-11-19.
    #[test]
    #[ignore]
    fn resolves_against_installed_exe() {
        let img = super::super::tests::map_image(&std::fs::read(std::env::var("NIER_EXE").unwrap()).unwrap());
        assert_eq!(super::resolve_request(&img, 0x38e2a0), Some(0x3ec650));
        assert_eq!(super::resolve(&img, 0x1_4000_0000), Ok((0xfca888, 0xc40758, 0xc46d70, 0xc4ce60)));
        assert_eq!(super::resolve_signal(&img, 0x3eca90, 0x38e2a0), Some((0x34cab0, 0x3cffb0, 0xfca918)));
        let slot = 0xc4ce60 + 8 * crate::sdk::patterns::GOAL_UPDATE;
        assert_eq!(u64::from_le_bytes(img[slot..slot + 8].try_into().unwrap()), 0x1_4000_0000 + 0x3e34c0);
        assert_eq!(super::rtti_vtable(&img, 0x1_4000_0000, crate::sdk::patterns::ENEMY_SET_RTTI), Ok(0xc40458));
        let slot = 0xc4ce60 + 8 * crate::sdk::patterns::GOAL_FINISH;
        assert_eq!(u64::from_le_bytes(img[slot..slot + 8].try_into().unwrap()), 0x1_4000_0000 + 0x3e07f0);
    }
}
