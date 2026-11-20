//! Shared state between the game thread (hooks) and the network thread. Each lock is held for a copy only.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32};

use bytemuck::{Pod, Zeroable};

use crate::sdk::cVec4;

/// One game frame of a player's controller input: the `CharacterController` fields the engine turns into
/// movement, attacks and pod use. Sent for every frame, so the remote 2B gets the same input sequence.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Pod, Zeroable)]
pub struct InputFrame {
    pub frame: u32,        // 0x00 sender's tick counter, starts at 1 (0 = empty packet slot)
    pub speed: f32,        // 0x04 stick magnitude
    pub facing2: f32,      // 0x08 stick direction
    pub held: u32,         // 0x0C held_flags
    pub buttons: [u32; 8], // 0x10 this frame's buttons
    pub aim: cVec4,        // 0x30 pod aim point (Pl0000::pod_aim)
    pub checksum: u32,     // 0x40 lockstep: hash of world entities at the start of this frame (0 outside lockstep)
    pub _pad: u32,         // 0x44
}
const _: () = assert!(size_of::<InputFrame>() == 0x48);

/// Sender's actual state, used only to correct drift the inputs did not reproduce.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Pod, Zeroable)]
pub struct Correction {
    pub pos: cVec4,        // 0x00
    pub rot: cVec4,        // 0x10 .y is yaw
    pub hp: u32,           // 0x20
    pub pod_cooldown: f32, // 0x24 active pod's remaining program cooldown
    pub anim: [u32; 4],    // 0x28 last start_animation args (replayed only with anim_replay=true)
    pub anim_seq: u32,     // 0x38 bumped on every start_animation
    pub _pad: u32,         // 0x3C
}
const _: () = assert!(size_of::<Correction>() == 0x40);

/// Inputs per packet: each packet repeats the last few frames, so a lost packet loses no input.
pub const REDUNDANCY: usize = 8;
/// Remote frames buffered beyond this are merged into the next one so the copy catches up (~4 frames of jitter).
const MAX_BUFFERED: usize = 4;
/// Hard limit on queued remote frames (~2 s at 60 fps).
const MAX_QUEUED: usize = 128;

/// Remote inputs in frame order: the network thread pushes, the game thread takes one per tick.
#[derive(Default)]
pub struct InputQueue {
    q: VecDeque<InputFrame>,
    last: Option<u32>,
}

impl InputQueue {
    pub const fn new() -> Self {
        Self { q: VecDeque::new(), last: None }
    }

    /// Adds the frames newer than any seen before (wrapping compare); duplicates from redundancy are skipped.
    pub fn push(&mut self, frames: &[InputFrame]) {
        let mut v: Vec<_> = frames.iter().filter(|f| f.frame != 0).copied().collect();
        v.sort_by_key(|f| f.frame);
        for f in v {
            if self.last.is_none_or(|l| (f.frame.wrapping_sub(l) as i32) > 0) {
                self.last = Some(f.frame);
                self.q.push_back(f);
            }
        }
        // Hard cap while nothing pops (loading, paused game, flooding peer): stale inputs are dropped.
        while self.q.len() > MAX_QUEUED {
            self.q.pop_front();
        }
    }

    /// The next frame to apply. When too far behind, later frames are merged in: their presses are kept and
    /// their stick/aim win, so no attack is dropped while catching up. `None` when empty.
    pub fn pop(&mut self) -> Option<InputFrame> {
        let mut f = self.q.pop_front()?;
        while self.q.len() > MAX_BUFFERED {
            let n = self.q.pop_front().unwrap();
            for (b, nb) in f.buttons.iter_mut().zip(n.buttons) {
                if nb != 0 {
                    *b = nb;
                }
            }
            f = InputFrame { buttons: f.buttons, held: f.held | n.held, ..n };
        }
        Some(f)
    }

    /// For a restarted peer: forget everything, its frame counter starts at 1 again.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Lockstep: true once frame `target` (or a later one) has arrived, so waiting longer is pointless.
    pub fn reached(&self, target: u32) -> bool {
        self.last.is_some_and(|l| (l.wrapping_sub(target) as i32) >= 0)
    }

    /// Lockstep: exactly frame `target`, dropping older frames. `None` if it is missing (lost beyond redundancy)
    /// or not here yet; no merging, since every frame must run with its own input.
    pub fn pop_frame(&mut self, target: u32) -> Option<InputFrame> {
        while self.q.front().is_some_and(|f| (f.frame.wrapping_sub(target) as i32) < 0) {
            self.q.pop_front();
        }
        self.q.front().is_some_and(|f| f.frame == target).then(|| self.q.pop_front().unwrap())
    }
}

/// Local player: the last `REDUNDANCY` frames (newest last) and current state; written by the hook, read by net.
pub static LOCAL_INPUTS: Mutex<VecDeque<InputFrame>> = Mutex::new(VecDeque::new());
pub static LOCAL_CORRECTION: Mutex<Option<Correction>> = Mutex::new(None);
/// Peer: written by the network thread, read by the hook.
pub static REMOTE_INPUTS: Mutex<InputQueue> = Mutex::new(InputQueue::new());
pub static REMOTE_CORRECTION: Mutex<Option<Correction>> = Mutex::new(None);
/// True while packets from the peer arrived recently; gates AI suppression.
pub static REMOTE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Config (`lockstep=true`, `host=true`), set before hooks and net start.
pub static LOCKSTEP: AtomicBool = AtomicBool::new(false);
pub static HOST: AtomicBool = AtomicBool::new(false);
/// Lockstep status, sent in every packet header. Ready = a local player character exists (area loaded).
/// Epoch = id of the running lockstep session (0 = stopped); the host starts epochs, the client joins them.
pub static LOCAL_READY: AtomicBool = AtomicBool::new(false);
pub static LOCAL_EPOCH: AtomicU32 = AtomicU32::new(0);
pub static REMOTE_READY: AtomicBool = AtomicBool::new(false);
pub static REMOTE_EPOCH: AtomicU32 = AtomicU32::new(0);

/// World sync (`world_sync=true`): the host's game owns the enemies. One enemy as the host sees it.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Pod, Zeroable)]
pub struct EnemyState {
    pub handle: u32, // 0x00 entity handle (assumed equal in both games: same area, same load order)
    pub kind: u32,   // 0x04 FNV-1a of the entity name, so a handle that means another entity is skipped
    pub hp: u32,     // 0x08 BehaviorAppBase::health
    pub yaw: f32,    // 0x0C rotation.y
    pub pos: cVec4,  // 0x10
}
const _: () = assert!(size_of::<EnemyState>() == 0x20);

/// Total damage the client has dealt to one enemy (cumulative, so a lost packet loses nothing).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Pod, Zeroable)]
pub struct Damage {
    pub handle: u32, // 0x00
    pub total: u32,  // 0x04
}
const _: () = assert!(size_of::<Damage>() == 0x8);

/// Enemies per snapshot (the ones nearest the client), and damage entries per packet.
pub const MAX_ENEMIES: usize = 32;
pub static WORLD_SYNC: AtomicBool = AtomicBool::new(false);
/// Host: this frame's snapshot (game thread writes, net sends). Client: newest snapshot received and when.
pub static LOCAL_WORLD: Mutex<Vec<EnemyState>> = Mutex::new(Vec::new());
pub static REMOTE_WORLD: Mutex<Option<(std::time::Instant, Vec<EnemyState>)>> = Mutex::new(None);
/// Client: damage totals to send. Host: newest totals received.
pub static LOCAL_DAMAGE: Mutex<Vec<Damage>> = Mutex::new(Vec::new());
pub static REMOTE_DAMAGE: Mutex<Vec<Damage>> = Mutex::new(Vec::new());

/// Quest sync: the host's scene state names, newline-separated (game thread writes, net sends every second).
/// Client: the newest list received, taken by the game thread when applied.
pub static LOCAL_SCENE: Mutex<Vec<u8>> = Mutex::new(Vec::new());
pub static REMOTE_SCENE: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// Cutscene sync: [action id, kind, state hash, state flag] the host just ran (game thread pushes, net sends), and
/// those received. Kind 0: a cutscene started playing; 1: a SendCommand ran its commands (debris blown up, doors
/// opened); 2: a quest objective (HapGoalLocationAction) changed state.
pub static LOCAL_EVENTS: Mutex<Vec<[u32; 4]>> = Mutex::new(Vec::new());
pub static REMOTE_EVENTS: Mutex<Vec<[u32; 4]>> = Mutex::new(Vec::new());
/// Signal sync: scene signals the host posted, [sender action id, signal hash, arg] (game -> net, net -> game).
pub static LOCAL_SIGNALS: Mutex<Vec<[u32; 3]>> = Mutex::new(Vec::new());
pub static REMOTE_SIGNALS: Mutex<Vec<[u32; 3]>> = Mutex::new(Vec::new());

/// Locks ignoring poison: a panic elsewhere must not stop the game thread (values are plain data).
pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(frame: u32, b0: u32) -> InputFrame {
        InputFrame { frame, buttons: [b0, 0, 0, 0, 0, 0, 0, 0], ..Default::default() }
    }

    #[test]
    fn queue_orders_dedups_and_catches_up() {
        let mut q = InputQueue::new();
        q.push(&[f(2, 0), f(1, 0), f(0, 9)]); // out of order, plus an empty slot
        q.push(&[f(1, 0), f(2, 0), f(3, 0)]); // redundant repeats
        assert_eq!([q.pop().unwrap().frame, q.pop().unwrap().frame, q.pop().unwrap().frame], [1, 2, 3]);
        assert_eq!(q.pop(), None);

        // 8 frames behind: catches up to MAX_BUFFERED left, keeping the press from a merged frame.
        q.push(&(4..12).map(|i| f(i, (i == 5) as u32)).collect::<Vec<_>>());
        let first = q.pop().unwrap();
        assert_eq!((first.frame, first.buttons[0]), (7, 1));
        assert_eq!(q.q.len(), MAX_BUFFERED);

        // Wrapping frame counter still counts as newer.
        let mut w = InputQueue::new();
        w.push(&[f(u32::MAX, 0)]);
        w.push(&[f(1, 0)]);
        assert_eq!(w.q.len(), 2);

        // Nothing popping: capped, newest kept.
        let mut c = InputQueue::new();
        c.push(&(1..=300).map(|i| f(i, 0)).collect::<Vec<_>>());
        assert_eq!((c.q.len(), c.q.back().unwrap().frame), (MAX_QUEUED, 300));
    }

    #[test]
    fn lockstep_exact_frames() {
        let mut q = InputQueue::new();
        assert!(!q.reached(1));
        q.push(&[f(1, 0), f(2, 0), f(4, 0)]); // frame 3 lost
        assert!(q.reached(3) && !q.reached(5));
        assert_eq!(q.pop_frame(2).map(|f| f.frame), Some(2)); // stale frame 1 dropped
        assert_eq!(q.pop_frame(3), None); // missing: caller runs without it
        assert_eq!(q.pop_frame(4).map(|f| f.frame), Some(4));
        assert_eq!(q.pop_frame(5), None);
    }
}
