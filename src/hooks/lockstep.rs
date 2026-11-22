//! Lockstep steps 1–3 (see notes/lockstep-world-sync.md): a shared frame clock, a frame barrier and per-frame
//! world checksums. Game thread only. It measures how far the two games diverge; it does not yet make the
//! engine deterministic (fixed delta time and a shared RNG seed are the next steps).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::{Duration, Instant};

use std::ptr::addr_of_mut;

use super::{entity_by_handle, for_each_object};
use crate::sdk::{Pl0000, cVec4};
use crate::state::{self, HOST, InputFrame, LOCAL_EPOCH, LOCAL_INPUTS, REMOTE_ACTIVE, REMOTE_EPOCH, REMOTE_INPUTS, REMOTE_READY};

/// Remote input runs this many frames after it was made (~50 ms at 60 fps), so it is usually already here.
pub const INPUT_DELAY: u32 = 3;
/// Longest the game thread waits for one frame of peer input before running without it (counted as a stall).
const BARRIER_TIMEOUT: Duration = Duration::from_millis(100);
/// Frames between summary log lines (~10 s).
const REPORT_EVERY: u32 = 600;

static FRAME: AtomicU32 = AtomicU32::new(0);
static LAST_JOINED: AtomicU32 = AtomicU32::new(0);
static NEXT_HOST_EPOCH: AtomicU32 = AtomicU32::new(0);

#[derive(Default)]
struct Stats {
    history: VecDeque<(u32, u32)>, // (frame, local checksum), newest last
    compared: u32,
    mismatches: u32,
    first_mismatch: Option<u32>,
    missing: u32,
    stalls: u32,
    max_wait: Duration,
}
static STATS: Mutex<Option<Stats>> = Mutex::new(None);

#[derive(Debug, PartialEq)]
enum Clock {
    Keep,
    Stop,
    Start(u32),
}

/// Host: runs while both players are ready, a fresh epoch each start. Client: runs the host's current epoch,
/// joining each epoch once (so it does not rejoin an epoch it just left).
fn decide(host: bool, epoch: u32, ready: bool, peer_ready: bool, peer_epoch: u32, last_joined: u32, next_host: u32) -> Clock {
    let running = epoch != 0;
    let keep = ready && peer_ready && (host || peer_epoch == epoch);
    match (running, keep) {
        (true, true) => Clock::Keep,
        (true, false) => Clock::Stop,
        (false, _) if !(ready && peer_ready) => Clock::Keep,
        (false, _) if host => Clock::Start(next_host),
        (false, _) if peer_epoch != 0 && peer_epoch != last_joined => Clock::Start(peer_epoch),
        _ => Clock::Keep,
    }
}

/// Advances the lockstep clock; the current lockstep frame (from 1) while a session runs.
pub fn step_clock(ready: bool) -> Option<u32> {
    let host = HOST.load(Relaxed);
    let next_host = match NEXT_HOST_EPOCH.load(Relaxed).wrapping_add(1) {
        0 => 1,
        n => n,
    };
    let epoch = LOCAL_EPOCH.load(Relaxed);
    let peer_ready = REMOTE_ACTIVE.load(Relaxed) && REMOTE_READY.load(Relaxed);
    match decide(host, epoch, ready, peer_ready, REMOTE_EPOCH.load(Relaxed), LAST_JOINED.load(Relaxed), next_host) {
        Clock::Keep => {}
        Clock::Stop => {
            report(epoch, FRAME.load(Relaxed), "stopped");
            LOCAL_EPOCH.store(0, Relaxed);
        }
        Clock::Start(e) => {
            if host {
                NEXT_HOST_EPOCH.store(e, Relaxed);
            } else {
                LAST_JOINED.store(e, Relaxed);
            }
            // Frames restart at 1: nothing from the previous session may be sent or compared.
            state::lock(&LOCAL_INPUTS).clear();
            *state::lock(&STATS) = Some(Stats::default());
            FRAME.store(0, Relaxed);
            LOCAL_EPOCH.store(e, Relaxed);
            tracing::info!("lockstep epoch {e} started ({})", if host { "host" } else { "joined host" });
        }
    }
    if LOCAL_EPOCH.load(Relaxed) == 0 {
        return None;
    }
    let n = FRAME.load(Relaxed) + 1;
    FRAME.store(n, Relaxed);
    Some(n)
}

/// Waits (bounded) for the peer's input of frame `n - INPUT_DELAY`, compares its world checksum with ours for
/// that frame, and returns it. `None` before the delay has passed, or when it did not arrive in time.
pub fn barrier(n: u32, local_checksum: u32) -> Option<InputFrame> {
    let epoch = LOCAL_EPOCH.load(Relaxed);
    let mut guard = state::lock(&STATS);
    let s = guard.get_or_insert_with(Stats::default);
    s.history.push_back((n, local_checksum));
    if s.history.len() > 64 {
        s.history.pop_front();
    }
    if n <= INPUT_DELAY {
        return None;
    }
    let target = n - INPUT_DELAY;
    let start = Instant::now();
    // shortcut: spins with yield (Windows sleep is ~1-15 ms); fine for a few ms, use an event if waits grow long.
    while !state::lock(&REMOTE_INPUTS).reached(target) {
        let peer_left = !REMOTE_ACTIVE.load(Relaxed) || (!HOST.load(Relaxed) && REMOTE_EPOCH.load(Relaxed) != epoch);
        if peer_left || start.elapsed() > BARRIER_TIMEOUT {
            s.stalls += 1;
            break;
        }
        std::thread::yield_now();
    }
    s.max_wait = s.max_wait.max(start.elapsed());
    let got = state::lock(&REMOTE_INPUTS).pop_frame(target);
    match got {
        None => s.missing += 1,
        Some(r) => {
            if let Some(&(_, mine)) = s.history.iter().find(|(f, _)| *f == target) {
                s.compared += 1;
                if mine != r.checksum {
                    s.mismatches += 1;
                    if s.first_mismatch.is_none() {
                        s.first_mismatch = Some(target);
                        tracing::warn!("lockstep epoch {epoch}: worlds differ from frame {target} (ours {mine:#010x}, peer {:#010x})", r.checksum);
                    }
                }
            }
        }
    }
    drop(guard);
    if n % REPORT_EVERY == 0 {
        report(epoch, n, "running");
    }
    got
}

fn report(epoch: u32, n: u32, what: &str) {
    if let Some(s) = state::lock(&STATS).as_ref() {
        tracing::info!(
            "lockstep epoch {epoch} {what} at frame {n}: {}/{} frames matched, first difference {:?}, {} inputs missing, {} stalls, longest wait {:?}",
            s.compared - s.mismatches,
            s.compared,
            s.first_mismatch,
            s.missing,
            s.stalls,
            s.max_wait
        );
    }
}

/// FNV-1a over (type id, position in cm) of every game object except the two players, sorted. Not handles or
/// list order: each game numbers and lists its objects itself, so those differ even when the worlds agree.
/// Positions are rounded to 1 cm so the hash reports real divergence, not last-bit float noise.
pub fn world_checksum(pl: *mut Pl0000, buddy: Option<*mut Pl0000>) -> u32 {
    let skip = buddy.unwrap_or(std::ptr::null_mut());
    let mut objs = Vec::new();
    for_each_object(|handle, b, p| {
        if b != pl && b != skip {
            let kind = entity_by_handle(handle).map_or(0, |e| unsafe { addr_of_mut!((*e).object_id).read() });
            objs.push(object_key(kind, p));
        }
    });
    checksum_of(objs)
}

fn object_key(kind: u32, p: cVec4) -> [u32; 4] {
    let cm = |v: f32| (v * 100.0).round() as i32 as u32;
    [kind, cm(p.x), cm(p.y), cm(p.z)]
}

fn checksum_of(mut objs: Vec<[u32; 4]>) -> u32 {
    objs.sort_unstable();
    let mut h = Fnv::new();
    for w in objs.iter().flatten() {
        h.word(*w);
    }
    h.0
}

struct Fnv(u32);
impl Fnv {
    fn new() -> Self {
        Self(0x811C_9DC5)
    }
    fn word(&mut self, w: u32) {
        for b in w.to_le_bytes() {
            self.0 = (self.0 ^ b as u32).wrapping_mul(0x0100_0193);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_host_and_client() {
        // host: starts a new epoch once both are ready, stops when either is not
        assert_eq!(decide(true, 0, true, false, 0, 0, 1), Clock::Keep);
        assert_eq!(decide(true, 0, true, true, 0, 0, 1), Clock::Start(1));
        assert_eq!(decide(true, 1, true, true, 0, 0, 2), Clock::Keep); // ignores the client's epoch
        assert_eq!(decide(true, 1, true, false, 1, 0, 2), Clock::Stop);
        // client: joins the host's epoch, leaves when it changes, joins the new one, never rejoins an old one
        assert_eq!(decide(false, 0, true, true, 0, 0, 1), Clock::Keep);
        assert_eq!(decide(false, 0, true, true, 4, 0, 1), Clock::Start(4));
        assert_eq!(decide(false, 4, true, true, 4, 4, 1), Clock::Keep);
        assert_eq!(decide(false, 4, true, true, 5, 4, 1), Clock::Stop);
        assert_eq!(decide(false, 0, true, true, 5, 4, 1), Clock::Start(5));
        assert_eq!(decide(false, 0, true, true, 4, 4, 1), Clock::Keep);
        assert_eq!(decide(false, 4, false, true, 4, 4, 1), Clock::Stop);
    }

    #[test]
    fn checksum_ignores_order() {
        let a = object_key(0x2_0001, cVec4 { x: 1.0, y: 2.0, z: 3.0, w: 1.0 });
        let b = object_key(0x2_0002, cVec4 { x: -4.0, y: 0.004, z: 9.0, w: 1.0 });
        assert_eq!(checksum_of(vec![a, b]), checksum_of(vec![b, a])); // list order differs between games
        let moved = object_key(0x2_0002, cVec4 { x: -4.0, y: 0.1, z: 9.0, w: 1.0 });
        assert_ne!(checksum_of(vec![a, b]), checksum_of(vec![a, moved]));
        assert_eq!(b, object_key(0x2_0002, cVec4 { x: -4.0, y: 0.0041, z: 9.0, w: 1.0 })); // sub-cm noise
    }

    #[test]
    fn fnv_known() {
        // FNV-1a of the 4 bytes 61 00 00 00 ("a" then three zero bytes)
        let mut h = Fnv::new();
        h.word(0x61);
        let mut r = 0x811C_9DC5u32;
        for b in [0x61u8, 0, 0, 0] {
            r = (r ^ b as u32).wrapping_mul(0x0100_0193);
        }
        assert_eq!(h.0, r);
        assert_ne!(h.0, Fnv::new().0);
    }
}
