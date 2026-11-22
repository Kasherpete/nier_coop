//! Per-frame game update hook (read local player / drive the remote one), a `start_animation` detour
//! (record local animations) and a mid-function hook that forces the remote's held buttons.
//! The remote player is the local player's buddy: a spawned 2B when `spawn_2b=true`, else the
//! game's own buddy. It is driven by the peer's controller input, one frame of input per game
//! frame (stick speed and direction, held/pressed buttons, pod aim), so the engine plays its own
//! locomotion and attacks; the peer's real position only corrects drift.
//! Everything here runs on the game thread; it only touches `state`, never the network.

use std::ptr::addr_of_mut;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};

use retour::{RawDetour, static_detour};

use crate::scan;
use crate::sdk::{self, Entity, EntityDescriptor, Pl0000, cVec4, patterns};
use crate::state::{
    self, Correction, HOST, InputFrame, LOCAL_CORRECTION, LOCAL_INPUTS, LOCAL_READY, LOCKSTEP, REDUNDANCY, REMOTE_ACTIVE,
    REMOTE_CORRECTION, REMOTE_INPUTS,
};

mod lockstep;
pub mod savefile;
mod spawn;
pub(crate) use spawn::crc32;
mod enemy_anim;
mod events;
mod quest;
mod world;

static_detour! {
    static StartAnim: unsafe extern "system" fn(*mut Pl0000, u32, u32, u32, u32);
}

static MODULE: OnceLock<&'static [u8]> = OnceLock::new();
/// Absolute addresses resolved at install.
static ENTITY_LIST: AtomicUsize = AtomicUsize::new(0); // global `EntityDescriptor*`
static POSSESSED: AtomicUsize = AtomicUsize::new(0); // global u32 handle of the controlled entity
static TICK_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0); // read by `tick_thunk`'s jmp
/// Learned on the first tick: the `Pl0000` vtable, and this frame's player / buddy behaviors.
static PL_VTABLE: AtomicUsize = AtomicUsize::new(0);
static PLAYER: AtomicUsize = AtomicUsize::new(0);
static BUDDY: AtomicUsize = AtomicUsize::new(0);
/// Local player's last start_animation args + counter (game thread only).
static ANIM: [AtomicU32; 5] = [const { AtomicU32::new(0) }; 5];
static APPLIED_ANIM_SEQ: AtomicU32 = AtomicU32::new(0);
/// Set from `nier_coop.cfg`: replay the peer's animations on top of the inputs.
pub static ANIM_REPLAY: AtomicBool = AtomicBool::new(false);
/// Local tick counter stamped on each sent input frame (never 0, which marks an empty packet slot).
static FRAME: AtomicU32 = AtomicU32::new(0);
/// The remote input applied this tick; read again by the held and free-aim hooks later in the frame.
static APPLIED: Mutex<InputFrame> = Mutex::new(InputFrame { frame: 0, speed: 0.0, facing2: 0.0, held: 0, buttons: [0; 8], aim: cVec4 { x: 0.0, y: 0.0, z: 0.0, w: 0.0 }, checksum: 0, _pad: 0 });
/// held_flags the mid-hook writes into the remote's controller.
static REMOTE_HELD: AtomicU32 = AtomicU32::new(0);
static HELD_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0); // read by `held_thunk`'s jmp
static FREE_AIM_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0); // read by `free_aim_thunk`'s jmps
static FREE_AIM_RESUME: AtomicUsize = AtomicUsize::new(0);
static POD_PROGRAM_FN: AtomicUsize = AtomicUsize::new(0);
static WARP_FN: AtomicUsize = AtomicUsize::new(0); // `Pl0000::warp`; 0 = not found, snaps write `position` only
/// Set from `nier_coop.cfg` before `install`.
pub static SPAWN_2B: AtomicBool = AtomicBool::new(false);
/// Handle of the spawned 2B (0 = none yet); she is spawned again once the game removes her (restart, area load).
static SPAWNED: AtomicU32 = AtomicU32::new(0);
/// Ticks until the next spawn attempt, so a failing spawn is not retried every frame.
static SPAWN_WAIT: AtomicU32 = AtomicU32::new(0);
/// Frames the peer's health has been 0 while ours was not; u32::MAX (disarmed) until the peer is seen alive, and
/// again after each follow-death and whenever the local player dies or unloads.
static PEER_DEAD_FRAMES: AtomicU32 = AtomicU32::new(u32::MAX);
const PEER_DEAD_HOLD: u32 = 120; // ~2 s at 0 hp: a real death, not a 1-frame revive
/// Local player present with health > 0 last tick; its edges reset world sync and bring the spawned 2B back.
static LOCAL_ALIVE: AtomicBool = AtomicBool::new(false);
/// Client: we died (not an area unload), so after the respawn we join the host's spot: each game reloads its own
/// last checkpoint, and the client's can be a different one (seen in a boss fight: host at its save point, client
/// at the boss start).
static REJOIN_HOST: AtomicBool = AtomicBool::new(false);
const REJOIN_DIST: f32 = 30.0; // m; closer than this both reloaded the same checkpoint

/// Next `PEER_DEAD_FRAMES` value for the peer's current health. While we are dead or loading ourselves we reload
/// anyway, so it disarms: the peer's 0 hp (its own death, or a stale value sent while it loads) must not kill the
/// player again right after the respawn.
fn peer_dead_step(frames: u32, hp: u32, local_alive: bool) -> u32 {
    match (local_alive, hp, frames) {
        (false, _, _) => u32::MAX,
        (true, 1.., _) => 0,
        (true, 0, u32::MAX) => u32::MAX,
        (true, 0, n) => n + 1,
    }
}

/// Kills the local player the way a kill area does (see `sdk::ANIM_DEATH`), so the game plays the death and
/// reloads the last checkpoint. Only for a player still alive; dying twice would restart the death.
fn kill_local(pl: *mut Pl0000) {
    unsafe {
        if addr_of_mut!((*pl).health).read() == 0 || addr_of_mut!((*pl).life).read() == 0 {
            return;
        }
        addr_of_mut!((*pl).life).write(0);
        addr_of_mut!((*pl).health).write(0);
        if StartAnim.is_enabled() {
            StartAnim.call(pl, sdk::ANIM_DEATH, 0, 0, 1);
            return;
        }
        let slot = (*pl).vtable.add(sdk::START_ANIMATION_INDEX);
        if in_module(slot as usize) && in_module(slot.read()) {
            std::mem::transmute::<usize, unsafe extern "system" fn(*mut Pl0000, u32, u32, u32, u32)>(slot.read())(pl, sdk::ANIM_DEATH, 0, 0, 1);
        }
    }
}

/// Moves `b` to `pos` facing `rot` with the engine's warp, so its collision comes along; plain write without it.
fn teleport(b: *mut Pl0000, pos: cVec4, rot: cVec4) {
    match WARP_FN.load(Relaxed) {
        0 => unsafe { addr_of_mut!((*b).position).write(pos) },
        f => unsafe {
            type Warp = unsafe extern "system" fn(*mut Pl0000, *const cVec4, *const cVec4, u32, f32);
            std::mem::transmute::<usize, Warp>(f)(b, &pos, &rot, 0, 0.0)
        },
    }
}

pub fn install() -> Result<(), String> {
    let m = scan::module().ok_or("could not read game module")?;
    let _ = MODULE.set(m);
    let base = m.as_ptr() as usize;
    let (list, possessed, update) = resolve(m)?;

    ENTITY_LIST.store(base + list, Relaxed);
    POSSESSED.store(base + possessed, Relaxed);
    // shortcut: patches code while game threads run (retour does not suspend them); fine at startup, not mid-combat.
    unsafe {
        let d = RawDetour::new((base + update) as *const (), tick_thunk as *const ()).map_err(|e| format!("UPDATE detour: {e}"))?;
        TICK_TRAMPOLINE.store(d.trampoline() as *const () as usize, Relaxed);
        d.enable().map_err(|e| format!("UPDATE detour: {e}"))?;
        Box::leak(Box::new(d)); // never unhooked; see DllMain
    }
    tracing::info!("hooks installed: update={:#x} entity_list={:#x} possessed={:#x}", base + update, base + list, base + possessed);

    // Optional parts: without them the remote still moves, but cannot be a 2B / hold buttons.
    match spawn::resolve(m) {
        Ok(o) => {
            let o = spawn::Offsets { spawn: base + o.spawn, spawner: base + o.spawner, routine: base + o.routine, scripts: base + o.scripts, held_store: base + o.held_store };
            tracing::info!("{o:x?}");
            // shortcut: mid-function detour; assumes no jump lands inside the overwritten instructions (true for the
            // checked exe: a 7-byte mov then `mov rbx,rcx`). Re-check `held_store` bytes after a game update.
            match unsafe { RawDetour::new(o.held_store as *const (), held_thunk as *const ()) } {
                Ok(d) => {
                    HELD_TRAMPOLINE.store(d.trampoline() as *const () as usize, Relaxed);
                    match unsafe { d.enable() } {
                        Ok(()) => drop(Box::leak(Box::new(d))),
                        Err(e) => tracing::error!("held-buttons hook: {e}"),
                    }
                }
                Err(e) => tracing::error!("held-buttons hook: {e}"),
            }
            let _ = spawn::ADDR.set(o);
        }
        Err(e) => tracing::error!("spawn/held-buttons lookups failed, using the game's buddy: {e}"),
    }
    match scan::parse(patterns::POD_PROGRAM).and_then(|p| scan::find_unique(m, &p)) {
        Some(at) => POD_PROGRAM_FN.store(base + at, Relaxed),
        None => tracing::error!("POD_PROGRAM not found; the remote's pod skill stays off"),
    }
    match scan::parse(patterns::WARP).and_then(|p| scan::find_unique(m, &p)) {
        Some(at) => WARP_FN.store(base + at, Relaxed),
        None => tracing::error!("WARP not found; the partner is snapped by writing her position (may not stick far away)"),
    }
    if crate::state::WORLD_SYNC.load(Relaxed) {
        if let Err(e) = events::install(m, base) {
            tracing::error!("event log off: {e}");
        }
    }
    if let Err(e) = quest::install(m, base) {
        tracing::error!("quest sync off: {e}");
    }
    if let Err(e) = install_free_aim(m, base) {
        tracing::error!("free-aim pod hook: {e}"); // her pod then only fires at entities near the aim point
    }
    Ok(())
}

/// Image offsets of (hook site, resume address) for the free-aim hook.
fn resolve_free_aim(m: &[u8]) -> Result<(usize, usize), String> {
    let at = scan::parse(patterns::POD_NO_TARGET).and_then(|p| scan::find_unique(m, &p)).ok_or("POD_NO_TARGET not found")?;
    let resume = at + 4 + m[at + 3] as i8 as isize as usize;
    if m.get(resume..resume + 7) != Some(&patterns::AIM_COPY[..]) {
        return Err(format!("unexpected code at aim copy {resume:#x}"));
    }
    Ok((at + 0x1C, resume))
}

fn install_free_aim(m: &[u8], base: usize) -> Result<(), String> {
    let (site, resume) = resolve_free_aim(m)?;
    FREE_AIM_RESUME.store(base + resume, Relaxed);
    // shortcut: mid-function detour like `held_store`; the overwritten `test rax,rax; je rel32` is relocated by retour.
    unsafe {
        let d = RawDetour::new((base + site) as *const (), free_aim_thunk as *const ()).map_err(|e| e.to_string())?;
        FREE_AIM_TRAMPOLINE.store(d.trampoline() as *const () as usize, Relaxed);
        d.enable().map_err(|e| e.to_string())?;
        Box::leak(Box::new(d));
    }
    tracing::info!("free-aim pod hook at {:#x}", base + site);
    Ok(())
}

/// Mid-function hook in pod_shot where a non-player pod found no target (rax = 0; rdi = Pl0000, rbp = frame).
/// For the remote, `on_free_aim` writes the peer's aim point into the frame's aim vector ([rbp-0x60]) and the
/// shot continues at the aim copy as if a target were there; otherwise the original `test; je exit` runs.
#[unsafe(naked)]
unsafe extern "system" fn free_aim_thunk() {
    core::arch::naked_asm!(
        "test rax, rax",
        "jnz 2f",
        "pushfq",
        "push rax",
        "push rcx",
        "push rdx",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push rbx",
        "mov rcx, rdi",
        "lea rdx, [rbp-0x60]",
        "mov rbx, rsp",
        "and rsp, -16",
        "sub rsp, 0x80",
        "movdqa [rsp+0x20], xmm0",
        "movdqa [rsp+0x30], xmm1",
        "movdqa [rsp+0x40], xmm2",
        "movdqa [rsp+0x50], xmm3",
        "movdqa [rsp+0x60], xmm4",
        "movdqa [rsp+0x70], xmm5",
        "call {f}",
        "movdqa xmm0, [rsp+0x20]",
        "movdqa xmm1, [rsp+0x30]",
        "movdqa xmm2, [rsp+0x40]",
        "movdqa xmm3, [rsp+0x50]",
        "movdqa xmm4, [rsp+0x60]",
        "movdqa xmm5, [rsp+0x70]",
        "mov rsp, rbx",
        "mov [rbx+0x38], rax", // the saved rax slot: pops hand the result back in rax (it was 0 on entry)
        "pop rbx",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "popfq",
        "test rax, rax",
        "jnz 3f",
        "2:",
        "jmp qword ptr [rip + {tramp}]",
        "3:",
        "jmp qword ptr [rip + {resume}]",
        f = sym on_free_aim,
        tramp = sym FREE_AIM_TRAMPOLINE,
        resume = sym FREE_AIM_RESUME,
    )
}

/// Returns 1 after writing the peer's aim point to `out` when `pl` is the remote and the peer is firing.
extern "system" fn on_free_aim(pl: usize, out: *mut cVec4) -> usize {
    let b = BUDDY.load(Relaxed);
    if b == 0 || pl != b || out.is_null() || !REMOTE_ACTIVE.load(Relaxed) {
        return 0;
    }
    let f = *state::lock(&APPLIED);
    if f.held & sdk::HELD_POD_FIRE == 0 {
        return 0;
    }
    // `out` is pod_shot's own stack slot for the aim vector (see POD_NO_TARGET).
    unsafe { out.write_unaligned(f.aim) };
    1
}

/// Mid-function hook at `held_store`: saves every volatile register and flags, calls `on_held(rcx)`,
/// restores them and continues with the relocated original instruction. Stack alignment is unknown
/// mid-function, so it aligns explicitly.
#[unsafe(naked)]
unsafe extern "system" fn held_thunk() {
    core::arch::naked_asm!(
        "pushfq",
        "push rax",
        "push rcx",
        "push rdx",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push rbx",
        "mov rbx, rsp",
        "and rsp, -16",
        "sub rsp, 0x80", // 0x20 shadow + 6 xmm
        "movdqa [rsp+0x20], xmm0",
        "movdqa [rsp+0x30], xmm1",
        "movdqa [rsp+0x40], xmm2",
        "movdqa [rsp+0x50], xmm3",
        "movdqa [rsp+0x60], xmm4",
        "movdqa [rsp+0x70], xmm5",
        "call {f}", // rcx is still the controller
        "movdqa xmm0, [rsp+0x20]",
        "movdqa xmm1, [rsp+0x30]",
        "movdqa xmm2, [rsp+0x40]",
        "movdqa xmm3, [rsp+0x50]",
        "movdqa xmm4, [rsp+0x60]",
        "movdqa xmm5, [rsp+0x70]",
        "mov rsp, rbx",
        "pop rbx",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "popfq",
        "jmp qword ptr [rip + {tramp}]",
        f = sym on_held,
        tramp = sym HELD_TRAMPOLINE,
    )
}

/// The engine just rebuilt held_flags from its input source (pad or AI); for the remote, use the peer's.
extern "system" fn on_held(ctrl: usize) {
    let b = BUDDY.load(Relaxed);
    // Only an exact match with this frame's validated buddy is dereferenced.
    if b != 0 && ctrl.wrapping_sub(sdk::CONTROLLER_OFFSET) == b && REMOTE_ACTIVE.load(Relaxed) {
        set_held(b as *mut Pl0000, REMOTE_HELD.load(Relaxed));
    }
}

/// The remote counts as AI-controlled, so its pod reads `ai_pod_fire` instead of the pod-fire held bit.
fn set_held(b: *mut Pl0000, held: u32) {
    unsafe {
        addr_of_mut!((*b).held_flags).write(held);
        addr_of_mut!((*b).ai_pod_fire).write((held & sdk::HELD_POD_FIRE != 0) as i32);
    }
}

/// Image offsets of (entity-list global, possessed-handle global, update function).
fn resolve(m: &[u8]) -> Result<(usize, usize, usize), String> {
    let pat = |name: &str, p: &str| scan::parse(p).ok_or(format!("{name}: invalid pattern"));
    let unique = |name: &str, p: &[Option<u8>]| scan::find_unique(m, p).ok_or(format!("{name}: not found or not unique"));

    let (p, disp, end) = patterns::ENTITY_LIST;
    let at = unique("ENTITY_LIST", &pat("ENTITY_LIST", p)?)?;
    let list = scan::rip_target(m, at, disp, end).ok_or("ENTITY_LIST: bad rip target")?;
    if list < sdk::ENTITY_COUNT_BEFORE_LIST {
        return Err("ENTITY_LIST: count outside module".into());
    }

    let s: Vec<_> = patterns::POSSESSED_STRING.bytes().chain([0]).map(Some).collect();
    let s = unique("POSSESSED_STRING", &s)?;
    let r = scan::find_ref(m, s).ok_or("POSSESSED_STRING: no code reference")? + 4;
    let window = m.get(r..r + 10).ok_or("POSSESSED_LEA: past module end")?;
    let lea = r + scan::find_unique(window, &pat("POSSESSED_LEA", patterns::POSSESSED_LEA)?)
        .ok_or("POSSESSED_LEA: not found after string reference")?;
    let possessed = scan::rip_target(m, lea, 3, 7).ok_or("POSSESSED_LEA: bad rip target")?;

    let mid = unique("UPDATE_MIDDLE", &pat("UPDATE_MIDDLE", patterns::UPDATE_MIDDLE)?)?;
    let update = scan::rfind(m, mid, 0x100, &pat("int3", "CC CC CC")?).ok_or("UPDATE: function start not found")? + 3;
    Ok((list, possessed, update))
}

/// Entry hook for the update function, whose signature is unknown: saves every argument register,
/// calls `on_tick`, restores them and jumps to the original with the stack untouched.
#[unsafe(naked)]
unsafe extern "system" fn tick_thunk() {
    core::arch::naked_asm!(
        "push rcx",
        "push rdx",
        "push r8",
        "push r9",
        "sub rsp, 0x68", // 0x20 shadow + 4 xmm; entry rsp%16==8, +0x20 pushes +0x68 -> aligned
        "movdqu [rsp+0x20], xmm0",
        "movdqu [rsp+0x30], xmm1",
        "movdqu [rsp+0x40], xmm2",
        "movdqu [rsp+0x50], xmm3",
        "call {tick}",
        "movdqu xmm0, [rsp+0x20]",
        "movdqu xmm1, [rsp+0x30]",
        "movdqu xmm2, [rsp+0x40]",
        "movdqu xmm3, [rsp+0x50]",
        "add rsp, 0x68",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rcx",
        "jmp qword ptr [rip + {tramp}]",
        tick = sym on_tick,
        tramp = sym TICK_TRAMPOLINE,
    )
}

extern "system" fn on_tick() {
    let pl = player();
    PLAYER.store(pl.map_or(0, |p| p as usize), Relaxed);
    let spawned = entity_by_handle(SPAWNED.load(Relaxed)).and_then(as_pl0000);
    // She is registered as the partner, so her death fails the mission: keep her invincible (her real damage
    // happens on the peer's machine) and showing the peer's health, also while the local player is in a flight unit.
    if let Some(b) = spawned {
        unsafe { addr_of_mut!((*b).muteki_timer).write(300.0) }; // the value Pl0000.setEventMuteki uses
        if let Some(c) = *state::lock(&REMOTE_CORRECTION) {
            unsafe { addr_of_mut!((*b).health).write(c.hp.max(1)) };
        }
    }
    // Death or respawn (checkpoint reload, area load): the host re-sends every enemy and the client re-pairs them,
    // and on respawn the spawned 2B comes back at once next to the player instead of after the 5 s retry.
    let alive = pl.is_some_and(|p| unsafe { addr_of_mut!((*p).health).read() } > 0);
    if LOCAL_ALIVE.swap(alive, Relaxed) != alive {
        tracing::info!("local player {}: resetting world sync", if alive { "respawned" } else { "died or unloaded" });
        world::reset();
        events::forget_contexts();
        if alive {
            SPAWN_WAIT.store(0, Relaxed);
            if let (Some(b), Some(p)) = (spawned, pl) {
                unsafe { teleport(b, addr_of_mut!((*p).position).read(), addr_of_mut!((*p).rotation).read()) };
            }
        } else {
            PEER_DEAD_FRAMES.store(u32::MAX, Relaxed);
            REJOIN_HOST.store(pl.is_some() && !HOST.load(Relaxed), Relaxed);
        }
    }
    LOCAL_READY.store(pl.is_some(), Relaxed);
    let ls_frame = if LOCKSTEP.load(Relaxed) { lockstep::step_clock(pl.is_some()) } else { None };
    let Some(pl) = pl else {
        BUDDY.store(0, Relaxed);
        return;
    };
    hook_start_animation(pl);
    // The peer died: its game reloads the last checkpoint, so die too and reload the same one (else the two worlds split).
    if let (Some(c), true) = (*state::lock(&REMOTE_CORRECTION), REMOTE_ACTIVE.load(Relaxed)) {
        let n = peer_dead_step(PEER_DEAD_FRAMES.load(Relaxed), c.hp, alive);
        if n == PEER_DEAD_HOLD {
            tracing::info!("partner died: killing the local player to restart at the last checkpoint");
            kill_local(pl);
        }
        PEER_DEAD_FRAMES.store(if n >= PEER_DEAD_HOLD { u32::MAX } else { n }, Relaxed);
        // Correction is None while the host loads, so hp > 0 here means the host has respawned.
        if alive && c.hp > 0 && REJOIN_HOST.swap(false, Relaxed) {
            let p = unsafe { addr_of_mut!((*pl).position).read() };
            let d = ((c.pos.x - p.x).powi(2) + (c.pos.y - p.y).powi(2) + (c.pos.z - p.z).powi(2)).sqrt();
            if d > REJOIN_DIST {
                tracing::info!("respawned {d:.0} m from the host (different checkpoint): warping to the host");
                teleport(pl, c.pos, c.rot);
            }
        }
    }
    let wait = SPAWN_WAIT.load(Relaxed);
    SPAWN_WAIT.store(wait.saturating_sub(1), Relaxed);
    if REMOTE_ACTIVE.load(Relaxed) && SPAWN_2B.load(Relaxed) && spawned.is_none() && wait == 0 {
        SPAWN_WAIT.store(300, Relaxed); // ~5 s
        let h = unsafe { (POSSESSED.load(Relaxed) as *const u32).read_volatile() };
        // On success she is now pl's buddy; on failure the game's buddy is used until the next attempt.
        if spawn::partner(pl, h).is_some() {
            SPAWNED.store(unsafe { addr_of_mut!((*pl).buddy_handle).read() }, Relaxed);
        }
    } else if let Some(b) = spawned.filter(|_| wait == 0) {
        // Cutscenes suspend her, give her buddy AI back or hand the player another buddy without removing her: she
        // then stands frozen or hidden. Set her up again after each cutscene, and whenever the buddy link breaks.
        let (h, mine) = (unsafe { (POSSESSED.load(Relaxed) as *const u32).read_volatile() }, SPAWNED.load(Relaxed));
        let broken = unsafe { addr_of_mut!((*pl).buddy_handle).read() != mine || addr_of_mut!((*b).buddy_handle).read() != h };
        if broken || events::EVENT_ENDED.swap(false, Relaxed) {
            SPAWN_WAIT.store(if broken { 300 } else { 60 }, Relaxed);
            spawn::relink(pl, h, b, mine);
        }
    }
    let buddy = unsafe { entity_by_handle(addr_of_mut!((*pl).buddy_handle).read()) }.and_then(as_pl0000);
    BUDDY.store(buddy.map_or(0, |b| b as usize), Relaxed);
    world::tick(pl);
    quest::tick();
    events::tick();

    // Record this frame's input (sent every frame) and current state (for drift correction).
    // In lockstep the frame number is the shared lockstep frame and carries a world checksum.
    let anim: [u32; 5] = std::array::from_fn(|i| ANIM[i].load(Relaxed));
    let (frame, checksum) = match ls_frame {
        Some(n) => (n, lockstep::world_checksum(pl, buddy)),
        None if LOCKSTEP.load(Relaxed) => (0, 0), // lockstep on but not running: send nothing but heartbeats
        None => {
            let n = match FRAME.load(Relaxed).wrapping_add(1) {
                0 => 1,
                n => n,
            };
            FRAME.store(n, Relaxed);
            (n, 0)
        }
    };
    unsafe {
        let input = InputFrame {
            frame,
            speed: addr_of_mut!((*pl).speed).read(),
            facing2: addr_of_mut!((*pl).facing2).read(),
            held: addr_of_mut!((*pl).held_flags).read(),
            buttons: addr_of_mut!((*pl).buttons).read(),
            aim: addr_of_mut!((*pl).pod_aim).read(),
            checksum,
            _pad: 0,
        };
        if frame != 0 {
            let mut local = state::lock(&LOCAL_INPUTS);
            if local.len() == REDUNDANCY {
                local.pop_front();
            }
            local.push_back(input);
        }
        *state::lock(&LOCAL_CORRECTION) = Some(Correction {
            pos: addr_of_mut!((*pl).position).read(),
            rot: addr_of_mut!((*pl).rotation).read(),
            hp: addr_of_mut!((*pl).health).read(),
            pod_cooldown: pod_cooldown_slot(pl).map_or(0.0, |c| c.read()),
            anim: [anim[0], anim[1], anim[2], anim[3]],
            anim_seq: anim[4],
            _pad: 0,
        });
    }
    // Lockstep: wait for the peer's input of frame N - delay (ours for N is already queued for sending).
    let remote = match ls_frame {
        Some(n) => Some(lockstep::barrier(n, checksum)),
        None if LOCKSTEP.load(Relaxed) => return, // stopped (waiting for the peer): nothing to apply
        None => None,
    };
    // Her, even while the game has handed the player another buddy (relinked within ~5 s).
    if let (Some(b), true) = (spawned.or(buddy), REMOTE_ACTIVE.load(Relaxed)) {
        // One remote frame per local frame; with none buffered, keep the stick and held buttons but press nothing new.
        let next = match remote {
            Some(r) => r,
            None => state::lock(&REMOTE_INPUTS).pop(),
        };
        let input = {
            let mut applied = state::lock(&APPLIED);
            *applied = next.unwrap_or(InputFrame { buttons: [0; 8], ..*applied });
            *applied
        };
        apply_input(b, &input);
        if let Some(c) = *state::lock(&REMOTE_CORRECTION) {
            correct_drift(b, &c);
            run_pod_program(b, c.pod_cooldown);
        }
    }
}

/// u32 at a byte offset of a validated `Pl0000`, for fields not worth a struct member.
fn pl_u32(b: *mut Pl0000, off: usize) -> u32 {
    assert!(!b.is_null() && off + 4 <= size_of::<Pl0000>());
    unsafe { (b as *const u8).add(off).cast::<u32>().read_unaligned() }
}

/// The remaining-cooldown slot of the active pod; None if the index is out of range.
fn pod_cooldown_slot(pl: *mut Pl0000) -> Option<*mut f32> {
    let i = pl_u32(pl, sdk::POD_INDEX) as usize;
    let all = unsafe { addr_of_mut!((*pl).pod_cooldown) };
    (i < 3).then(|| unsafe { addr_of_mut!((*all)[i][1]) })
}

/// The engine runs the pod-program update only for the player-controlled character, so the remote's skill
/// press never starts a program; run it for her each frame behind the gates the engine applies to everyone.
/// shortcut: skips the rest of 0x528c70 (player-only input handling); fine while pod skills are all she misses.
fn run_pod_program(b: *mut Pl0000, peer_cooldown: f32) {
    // The cooldown only ticks down in the player-only routine; the peer's own value is the right one anyway.
    if let Some(c) = pod_cooldown_slot(b) {
        unsafe { c.write(peer_cooldown) };
    }
    let f = POD_PROGRAM_FN.load(Relaxed);
    let state = pl_u32(b, sdk::PL_STATE);
    if f == 0
        || sdk::POD_PROGRAM_BLOCKERS.iter().any(|&o| pl_u32(b, o) != 0)
        || pl_u32(b, sdk::POD_PROGRAM_FLAGS) & 0x20 != 0
        || state == 0x41
        || state == 0x46
    {
        return;
    }
    unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(*mut Pl0000)>(f)(b) }
}

fn in_module(addr: usize) -> bool {
    MODULE.get().is_some_and(|m| (m.as_ptr() as usize..m.as_ptr() as usize + m.len()).contains(&addr))
}

/// Looks up a live entity; rejects out-of-range indices and stale handles like the engine does.
fn entity_by_handle(handle: u32) -> Option<*mut Entity> {
    let g = ENTITY_LIST.load(Relaxed);
    if g == 0 || handle == 0 {
        return None;
    }
    unsafe {
        let list = (g as *const *const EntityDescriptor).read_volatile();
        let count = ((g - sdk::ENTITY_COUNT_BEFORE_LIST) as *const u32).read_volatile() as usize;
        let i = ((handle >> 8) & 0xFFFF) as usize;
        if list.is_null() || i >= count {
            return None;
        }
        let d = list.add(i).read_volatile();
        ((d.handle ^ handle) & 0xFFFF_FF00 == 0 && !d.ent.is_null()).then_some(d.ent)
    }
}

/// Within this distance of the aim point an entity counts as aimed at.
const AIM_RADIUS: f32 = 4.0;

/// Handle of the entity whose position is closest to `aim` (within `AIM_RADIUS`), skipping `skip`.
/// None means free aim: `on_free_aim` then shoots at the aim point itself.
fn nearest_entity(aim: cVec4, skip: *mut Pl0000) -> Option<u32> {
    let mut best = (AIM_RADIUS * AIM_RADIUS, None);
    for_each_object(|handle, b, p| {
        let dd = (p.x - aim.x).powi(2) + (p.y - aim.y).powi(2) + (p.z - aim.z).powi(2);
        if b != skip && dd < best.0 {
            best = (dd, Some(handle));
        }
    });
    best.1
}

/// Calls `f(handle, behavior, position)` for every live game object in the entity list, in list order.
fn for_each_object(mut f: impl FnMut(u32, *mut Pl0000, cVec4)) {
    let g = ENTITY_LIST.load(Relaxed);
    if g == 0 {
        return;
    }
    unsafe {
        let list = (g as *const *const EntityDescriptor).read_volatile();
        let count = ((g - sdk::ENTITY_COUNT_BEFORE_LIST) as *const u32).read_volatile() as usize;
        if list.is_null() {
            return;
        }
        for i in 0..count {
            let d = list.add(i).read_volatile();
            let Some(e) = d.ent.as_ref() else { continue };
            let b = e.behavior;
            // Only real game objects: every list behavior with a game vtable is a Behavior, which is larger than
            // Model::position (0x50..0x60). pod_shot itself rejects non-model targets (0x385f10 returns null).
            if b.is_null() || !in_module(b.as_ref().map_or(0, |o| o.vtable as usize)) {
                continue;
            }
            f(e.handle, b, addr_of_mut!((*b).position).read());
        }
    }
}

/// The entity's behavior, only if its vtable is the `Pl0000` one (so the struct size is known).
fn as_pl0000(e: *mut Entity) -> Option<*mut Pl0000> {
    let b = unsafe { addr_of_mut!((*e).behavior).read() };
    let vt = unsafe { b.as_ref()? }.vtable as usize;
    let known = PL_VTABLE.load(Relaxed);
    (known != 0 && vt == known).then_some(b)
}

/// The possessed entity as a `Pl0000`. The first time it is named "Player", its vtable becomes the reference.
fn player() -> Option<*mut Pl0000> {
    let h = match POSSESSED.load(Relaxed) {
        0 => return None,
        g => unsafe { (g as *const u32).read_volatile() },
    };
    let e = entity_by_handle(h)?;
    if PL_VTABLE.load(Relaxed) == 0 {
        let name = unsafe { addr_of_mut!((*e).name).read() };
        let b = unsafe { addr_of_mut!((*e).behavior).read().as_ref()? };
        if name.starts_with(b"Player\0") && in_module(b.vtable as usize) {
            PL_VTABLE.store(b.vtable as usize, Relaxed);
            tracing::info!("Pl0000 vtable {:#x}", b.vtable as usize);
        }
    }
    as_pl0000(e) // None while hacking/controlling a non-Pl0000
}

fn hook_start_animation(pl: *mut Pl0000) {
    if StartAnim.is_enabled() {
        return;
    }
    unsafe {
        let slot = (*pl).vtable.add(sdk::START_ANIMATION_INDEX);
        if !in_module(slot as usize) || !in_module(slot.read()) {
            return;
        }
        type F = unsafe extern "system" fn(*mut Pl0000, u32, u32, u32, u32);
        match StartAnim.initialize(std::mem::transmute::<usize, F>(slot.read()), start_animation).and_then(|d| d.enable()) {
            Ok(()) => tracing::info!("start_animation hooked at {:#x}", slot.read()),
            Err(e) => tracing::error!("start_animation detour: {e}"),
        }
    }
}

fn start_animation(this: *mut Pl0000, anim: u32, variant: u32, a3: u32, a4: u32) {
    let p = this as usize;
    let who = if p == 0 { "?" } else if p == PLAYER.load(Relaxed) { "player" } else if p == BUDDY.load(Relaxed) { "buddy" } else { "other" };
    tracing::debug!("anim {who}: {anim:#x} {variant:#x} {a3:#x} {a4:#x}");
    // The remote's own calls are kept (they come from the synced inputs), except the partner AI's wait state.
    if p != 0 && p == BUDDY.load(Relaxed) && anim == sdk::ANIM_PARTNER_WAIT && REMOTE_ACTIVE.load(Relaxed) {
        return;
    }
    if p != 0 && p == PLAYER.load(Relaxed) {
        for (a, v) in ANIM.iter().zip([anim, variant, a3, a4]) {
            a.store(v, Relaxed);
        }
        ANIM[4].fetch_add(1, Relaxed);
    }
    unsafe { StartAnim.call(this, anim, variant, a3, a4) }
}

/// Feeds one frame of the peer's controller input to her controller; the engine does the rest.
fn apply_input(b: *mut Pl0000, f: &InputFrame) {
    unsafe {
        addr_of_mut!((*b).run_speed_type).write(0); // speed from the controller, not buddy AI
        addr_of_mut!((*b).speed).write(f.speed);
        addr_of_mut!((*b).facing2).write(f.facing2);
        addr_of_mut!((*b).buttons).write(f.buttons);
    }
    let held = f.held | f.buttons.iter().enumerate().filter(|(_, v)| **v != 0).fold(0, |h, (i, _)| h | 1 << i);
    REMOTE_HELD.store(held, Relaxed);
    set_held(b, held); // also forced in `on_held` when that hook is installed
    if held & sdk::HELD_POD_FIRE != 0 {
        // Her pod aims at a target entity, not a camera ray: use what sits at the remote player's aim point.
        unsafe { addr_of_mut!((*b).pod_target).write(nearest_entity(f.aim, b).unwrap_or(0)) };
    }
}

/// Past this distance (m) the inputs have drifted: pull her toward the peer's real position.
const CORRECT_DIST: f32 = 1.0;
/// Past this distance (m) she is teleported (area load, respawn, long stall).
const SNAP_DIST: f32 = 10.0;
/// Still this far off (m) after `STUCK_FRAMES` frames of pulling: something blocks her here only (debris that
/// is gone on the peer's side), so she is teleported.
const STUCK_DIST: f32 = 3.0;
const STUCK_FRAMES: u32 = 60;
static STUCK: AtomicU32 = AtomicU32::new(0);
static SNAPS: AtomicU32 = AtomicU32::new(0);
/// Past this yaw error (rad) her rotation is set to the peer's.
const CORRECT_YAW: f32 = 0.5;

/// Drift correction only: small errors are left to the inputs, so her motion stays the engine's own.
fn correct_drift(b: *mut Pl0000, c: &Correction) {
    unsafe {
        let pos = addr_of_mut!((*b).position);
        let (p, t) = (pos.read(), c.pos);
        let d = ((t.x - p.x).powi(2) + (t.y - p.y).powi(2) + (t.z - p.z).powi(2)).sqrt();
        let stuck = if d > STUCK_DIST {
            STUCK.fetch_add(1, Relaxed) + 1
        } else {
            STUCK.store(0, Relaxed);
            0
        };
        if d > SNAP_DIST || stuck > STUCK_FRAMES {
            STUCK.store(0, Relaxed);
            teleport(b, t, c.rot);
            // Snaps every frame mean the game ignores her position (she stands frozen on screen).
            if SNAPS.fetch_add(1, Relaxed) % 120 == 0 {
                tracing::info!("partner snapped {d:.1} m to ({:.1}, {:.1}, {:.1}), now at {:?}", t.x, t.y, t.z, pos.read().x);
            }
        } else if d > CORRECT_DIST {
            let k = 0.2;
            pos.write(cVec4 { x: p.x + (t.x - p.x) * k, y: p.y + (t.y - p.y) * k, z: p.z + (t.z - p.z) * k, w: t.w });
        }
        let rot = addr_of_mut!((*b).rotation);
        if yaw_error(rot.read().y, c.rot.y) > CORRECT_YAW {
            rot.write(c.rot);
        }
        if !ANIM_REPLAY.load(Relaxed) {
            return;
        }
        let [a, ..] = c.anim;
        let replay = !sdk::ANIM_CRASHES.contains(&a) && !sdk::ANIM_FROM_INPUT.contains(&a) && a != sdk::ANIM_DEATH;
        if c.anim_seq != APPLIED_ANIM_SEQ.swap(c.anim_seq, Relaxed) && replay && StartAnim.is_enabled() {
            let [a, v, a3, a4] = c.anim;
            tracing::debug!("anim replay on buddy: {a:#x} {v:#x} {a3:#x} {a4:#x}");
            StartAnim.call(b, a, v, a3, a4); // original, bypassing the buddy filter
        }
    }
}

/// Absolute difference of two yaw angles (rad), wrapped to [0, pi].
fn yaw_error(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(std::f32::consts::TAU);
    d.min(std::f32::consts::TAU - d)
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn peer_dead_needs_alive_first() {
        let s = |frames, hp| super::peer_dead_step(frames, hp, true);
        assert_eq!(s(u32::MAX, 0), u32::MAX); // never seen alive (not in game yet): stay disarmed
        assert_eq!(s(u32::MAX, 50), 0);
        assert_eq!(s(0, 0), 1);
        assert_eq!(s(7, 0), 8);
        assert_eq!(s(7, 1), 0); // revived: count restarts
    }

    #[test]
    fn peer_dead_disarms_while_local_dead() {
        use super::peer_dead_step as s;
        // We died first: the peer then dies too (following us), which must not count against our respawn.
        assert_eq!(s(50, 0, false), u32::MAX);
        assert_eq!(s(50, 80, false), u32::MAX);
        // Respawned while the peer still loads (its last packet said 0 hp): stays disarmed until it is seen alive.
        assert_eq!(s(u32::MAX, 0, true), u32::MAX);
        assert_eq!(s(u32::MAX, 80, true), 0);
    }

    #[test]
    fn yaw_error_wraps() {
        use std::f32::consts::PI;
        assert!((super::yaw_error(PI - 0.1, -PI + 0.1) - 0.2).abs() < 1e-5);
        assert!((super::yaw_error(0.0, PI) - PI).abs() < 1e-5);
        assert!(super::yaw_error(1.0, 1.0 + 2.0 * PI) < 1e-5);
    }

    /// Maps a PE file's sections to their virtual offsets, like the loader does (no relocations or imports).
    pub fn map_image(f: &[u8]) -> Vec<u8> {
        let rd = |o: usize, n: usize| f.get(o..o + n).map(|b| b.iter().rev().fold(0usize, |a, &x| a << 8 | x as usize)).unwrap();
        let pe = rd(0x3C, 4);
        let (n, opt) = (rd(pe + 6, 2), rd(pe + 20, 2));
        let mut img = vec![0u8; rd(pe + 24 + 56, 4)]; // SizeOfImage
        img[..0x1000.min(f.len())].copy_from_slice(&f[..0x1000.min(f.len())]);
        for i in 0..n {
            let s = pe + 24 + opt + 40 * i;
            let (va, raw, len) = (rd(s + 12, 4), rd(s + 20, 4), rd(s + 16, 4).min(rd(s + 8, 4)));
            img[va..va + len].copy_from_slice(&f[raw..raw + len]);
        }
        img
    }

    /// Run with `NIER_EXE=<path to NieRAutomata.exe> cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn resolves_against_installed_exe() {
        let img = map_image(&std::fs::read(std::env::var("NIER_EXE").unwrap()).unwrap());
        let r = super::resolve(&img);
        println!("{r:x?}");
        r.unwrap();
        // Checked exe: hook on `test rax,rax` at 0x48b943, resume at the aim copy 0x48b992 (both inside pod_shot).
        let (site, resume) = super::resolve_free_aim(&img).unwrap();
        assert_eq!(&img[site..site + 5], &[0x48, 0x85, 0xC0, 0x0F, 0x84]);
        assert_eq!((site, resume), (0x48b943, 0x48b992));
        let pp = crate::scan::parse(crate::sdk::patterns::POD_PROGRAM).and_then(|p| crate::scan::find_unique(&img, &p));
        assert_eq!(pp, Some(0x529080));
        let warp = crate::scan::parse(crate::sdk::patterns::WARP).and_then(|p| crate::scan::find_unique(&img, &p));
        assert_eq!(warp, Some(0x4f0fa0));
        // The kill-area death this mod copies: `mov [r12+0x10668],esi; mov [r12+0x858],esi; ...; lea edx,[r9+0x35]`.
        assert_eq!(&img[0x47b172..0x47b182], &[0x41, 0x89, 0xB4, 0x24, 0x68, 0x06, 0x01, 0x00, 0x41, 0x89, 0xB4, 0x24, 0x58, 0x08, 0x00, 0x00]);
        assert_eq!(&img[0x47b191..0x47b195], &[0x41, 0x8D, 0x51, crate::sdk::ANIM_DEATH as u8]);
    }
}

#[cfg(test)]
pub(crate) mod exe_checks {
    use super::tests::map_image;
    use crate::scan;

    fn u32_at(m: &[u8], o: usize) -> usize {
        u32::from_le_bytes(m[o..o + 4].try_into().unwrap()) as usize
    }
    /// Begin RVAs from .pdata (exception directory), i.e. every real function start.
    pub(crate) fn func_starts(m: &[u8]) -> Vec<usize> {
        let pe = u32_at(m, 0x3C);
        let (rva, size) = (u32_at(m, pe + 24 + 112 + 3 * 8), u32_at(m, pe + 24 + 112 + 3 * 8 + 4));
        (rva..rva + size).step_by(12).map(|o| u32_at(m, o)).collect()
    }
    fn count(m: &[u8], p: &str) -> usize {
        let p = scan::parse(p).unwrap();
        m.windows(p.len()).filter(|w| w.iter().zip(&p).all(|(b, q)| q.is_none_or(|q| q == *b))).count()
    }

    /// Prints layout evidence from the installed exe. `NIER_EXE=... cargo test exe_checks -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn layout_evidence() {
        let m = map_image(&std::fs::read(std::env::var("NIER_EXE").unwrap()).unwrap());
        let pe = u32_at(&m, 0x3C);
        let image_base = u64::from_le_bytes(m[pe + 24 + 24..pe + 24 + 32].try_into().unwrap());
        let starts = func_starts(&m);
        let (_, _, update) = super::resolve(&m).unwrap();
        println!("update {update:#x} is a function start: {}", starts.binary_search(&update).is_ok());
        println!("update first bytes: {:02x?}", &m[update..update + 16]);

        // RTTI: TypeDescriptor name -> CompleteObjectLocator -> vtable.
        let name: Vec<_> = b".?AVPl0000@@\0".iter().map(|&b| Some(b)).collect();
        let td = scan::find_unique(&m, &name).expect("Pl0000 RTTI name") - 0x10;
        let col = (0..m.len() - 0x18).step_by(4)
            .find(|&o| u32_at(&m, o) == 1 && u32_at(&m, o + 4) == 0 && u32_at(&m, o + 0xC) == td && u32_at(&m, o + 0x14) == o)
            .expect("Pl0000 COL");
        let col_va = (image_base + col as u64).to_le_bytes();
        let meta = (0..m.len() - 8).step_by(8).find(|&o| m[o..o + 8] == col_va).expect("Pl0000 vtable");
        let vt = meta + 8;
        let slot = |i: usize| (u64::from_le_bytes(m[vt + 8 * i..vt + 8 * i + 8].try_into().unwrap()) - image_base) as usize;
        println!("Pl0000 vtable rva {vt:#x}");
        for i in 16..=20 {
            let f = slot(i);
            println!("  slot {i}: {f:#x} func_start={} bytes {:02x?}", starts.binary_search(&f).is_ok(), &m[f..f + 12]);
        }

        // Struct size and field displacements as they appear in code (disp32 little-endian).
        for (what, p) in [
            ("alloc 0x17920 (mov reg,imm32)", "20 79 01 00"),
            ("buddy_handle 0x1647C", "7C 64 01 00"),
            ("facing2 0x143C", "3C 14 00 00"),
            ("health 0x858", "58 08 00 00"),
            ("controller 0xCA0", "A0 0C 00 00"),
            ("old-size neighbours 0x17910/0x17930", "10 79 01 00"),
        ] {
            println!("{what}: {}", count(&m[0x1000..0xc1b000], p));
        }
    }
}
