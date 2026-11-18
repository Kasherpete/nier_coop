//! Shared state between the game thread (hooks) and the network thread. Lock-free.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};

use bytemuck::{Pod, Zeroable};

use crate::sdk::cVec4;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Pod, Zeroable)]
pub struct PlayerState {
    pub pos: cVec4,     // 0x00
    pub rot: cVec4,     // 0x10
    pub facing2: f32,   // 0x20
    pub hp: u32,        // 0x24
    pub anim: [u32; 4], // 0x28 last start_animation args: anim, variant, a3, a4
    pub anim_seq: u32,  // 0x38 bumped on every start_animation, so repeats replay
    pub speed: f32,     // 0x3C controller stick magnitude
    pub held: u32,      // 0x40 controller held_flags
    pub buttons: [u32; 8], // 0x44 last non-zero controller buttons
    pub btn_seq: u32,   // 0x64 bumped whenever `buttons` was non-zero, so presses replay
    pub aim: cVec4,     // 0x68 pod aim point (Pl0000::pod_aim)
    pub pod_cooldown: f32, // 0x78 active pod's remaining program cooldown
}
const WORDS: usize = size_of::<PlayerState>() / 4;
const _: () = assert!(size_of::<PlayerState>() == 0x7C);

/// Single-writer seqlock holding the latest value. Words are atomics so torn reads are retried, never UB.
pub struct Slot {
    seq: AtomicU32,
    words: [AtomicU32; WORDS],
}

impl Slot {
    pub const fn new() -> Self {
        Self { seq: AtomicU32::new(0), words: [const { AtomicU32::new(0) }; WORDS] }
    }

    /// Only one thread may call `store` on a given slot.
    pub fn store(&self, s: &PlayerState) {
        let n = self.seq.load(Ordering::Relaxed);
        self.seq.store(n.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (a, w) in self.words.iter().zip(bytemuck::cast::<_, [u32; WORDS]>(*s)) {
            a.store(w, Ordering::Relaxed);
        }
        self.seq.store(n.wrapping_add(2), Ordering::Release);
    }

    /// `None` until the first `store`.
    pub fn load(&self) -> Option<PlayerState> {
        loop {
            let a = self.seq.load(Ordering::Acquire);
            if a == 0 {
                return None;
            }
            if a % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let w: [u32; WORDS] = std::array::from_fn(|i| self.words[i].load(Ordering::Relaxed));
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == a {
                return Some(bytemuck::cast(w));
            }
        }
    }
}

/// Written by the hook (local player), read by the network thread.
pub static LOCAL: Slot = Slot::new();
/// Written by the network thread (peer), read by the hook.
pub static REMOTE: Slot = Slot::new();
/// True while packets from the peer arrived recently; gates AI suppression.
pub static REMOTE_ACTIVE: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slot_concurrent() {
        static S: Slot = Slot::new();
        assert_eq!(S.load(), None);
        let w = std::thread::spawn(|| {
            for i in 1..=100_000u32 {
                let f = i as f32;
                S.store(&PlayerState { pos: cVec4 { x: f, y: f, z: f, w: f }, anim_seq: i, hp: i, ..Default::default() });
            }
        });
        while !w.is_finished() {
            if let Some(s) = S.load() {
                // every field written together must be read together
                assert_eq!(s.pos.x as u32, s.anim_seq);
                assert_eq!(s.pos.w as u32, s.anim_seq);
                assert_eq!(s.hp, s.anim_seq);
            }
        }
        assert_eq!(S.load().unwrap().anim_seq, 100_000);
    }
}
