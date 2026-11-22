//! Reliable, ordered delivery for the few packets that must arrive (cutscene/objective/spawn events, scene
//! signals). Each message gets a sequence number and is resent until the peer acks it; the receiver delivers in
//! sequence order, holding back anything that arrives early. Packets: `NCRL` [seq, base, body], `NCRA` [seq].
//!
//! `base` is the sender's oldest unacked seq: the receiver never waits for a seq the sender no longer has (it
//! dropped it, or the receiver reset after a timeout), so a reset on either side cannot stall delivery. The
//! sender's seq only ever grows (never reset), so a receiver that kept its state never mistakes new messages
//! for old ones.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub const DATA_MAGIC: u32 = u32::from_le_bytes(*b"NCRL");
pub const ACK_MAGIC: u32 = u32::from_le_bytes(*b"NCRA");
const HEAD: usize = 12;
/// Unacked messages are resent this often.
const RESEND: Duration = Duration::from_millis(100);
/// At most this many packets per `due` call, so a long backlog cannot flood the link.
const BURST: usize = 64;
/// Unacked messages kept; past this the oldest is dropped (and `base` tells the receiver to skip it).
const MAX_OUT: usize = 2048;
/// Early messages the receiver holds while waiting for a gap to fill; anything further ahead is not acked (resent later).
const MAX_HELD: u32 = 1024;
/// Largest body accepted (events and signals are 16-20 bytes).
pub const MAX_BODY: usize = 256;

fn ahead(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

pub struct Sender {
    next: u32,
    out: VecDeque<(u32, Vec<u8>, Option<Instant>)>, // seq, body, last sent
}

impl Sender {
    pub fn new() -> Self {
        Self { next: 1, out: VecDeque::new() }
    }

    pub fn push(&mut self, body: Vec<u8>) {
        debug_assert!(body.len() <= MAX_BODY);
        if self.out.len() == MAX_OUT {
            self.out.pop_front();
        }
        self.out.push_back((self.next, body, None));
        self.next = self.next.wrapping_add(1);
    }

    /// Packets to send now: messages never sent, and unacked ones last sent `RESEND` ago.
    pub fn due(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let base = self.out.front().map_or(self.next, |m| m.0);
        let mut v = Vec::new();
        for (seq, body, sent) in self.out.iter_mut() {
            if v.len() == BURST {
                break;
            }
            if sent.is_none_or(|t| now.duration_since(t) >= RESEND) {
                *sent = Some(now);
                let mut p = Vec::with_capacity(HEAD + body.len());
                for w in [DATA_MAGIC, *seq, base] {
                    p.extend_from_slice(&w.to_le_bytes());
                }
                p.extend_from_slice(body);
                v.push(p);
            }
        }
        v
    }

    pub fn on_ack(&mut self, p: &[u8]) {
        if let Some(seq) = word(p, 1).filter(|_| p.len() == 8 && word(p, 0) == Some(ACK_MAGIC)) {
            self.out.retain(|m| m.0 != seq);
        }
    }

    /// Peer gone: stop resending (its next session starts from our `base`). The seq keeps counting.
    pub fn clear(&mut self) {
        self.out.clear();
    }

    #[cfg(test)]
    fn pending(&self) -> usize {
        self.out.len()
    }
}

#[derive(Default)]
pub struct Receiver {
    expected: Option<u32>,
    held: HashMap<u32, Vec<u8>>,
}

impl Receiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// One `NCRL` packet: the ack to send back (None: not one, or too far ahead to hold) and the bodies now
    /// deliverable, in order. Trust boundary: length-checked; the bodies are parsed by the caller.
    pub fn on_packet(&mut self, p: &[u8]) -> (Option<Vec<u8>>, Vec<Vec<u8>>) {
        let (Some(DATA_MAGIC), Some(seq), Some(base)) = (word(p, 0), word(p, 1), word(p, 2)) else { return (None, Vec::new()) };
        let body = &p[HEAD..];
        if body.len() > MAX_BODY {
            return (None, Vec::new());
        }
        let exp = self.expected.get_or_insert(base);
        if ahead(base, *exp) {
            // The sender no longer has the messages before `base`: skip them.
            let b = base;
            self.held.retain(|&s, _| !ahead(b, s));
            *exp = base;
        }
        let exp_now = *exp;
        if ahead(seq, exp_now) && seq.wrapping_sub(exp_now) >= MAX_HELD {
            return (None, Vec::new());
        }
        let ack: Vec<u8> = [ACK_MAGIC, seq].iter().flat_map(|w| w.to_le_bytes()).collect();
        if ahead(exp_now, seq) {
            return (Some(ack), Vec::new()); // delivered before: only the ack was lost
        }
        self.held.insert(seq, body.to_vec());
        let mut out = Vec::new();
        let exp = self.expected.as_mut().unwrap();
        while let Some(b) = self.held.remove(exp) {
            out.push(b);
            *exp = exp.wrapping_add(1);
        }
        (Some(ack), out)
    }

    /// Peer gone: its next session's `base` tells us where to start.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

fn word(p: &[u8], i: usize) -> Option<u32> {
    p.get(i * 4..i * 4 + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bodies(r: &mut Receiver, p: &[u8]) -> Vec<u8> {
        r.on_packet(p).1.into_iter().map(|b| b[0]).collect()
    }

    #[test]
    fn lost_and_reordered_arrive_once_in_order() {
        let (mut s, mut r) = (Sender::new(), Receiver::new());
        let t = Instant::now();
        for i in 1..=4u8 {
            s.push(vec![i]);
        }
        let p = s.due(t);
        assert_eq!(p.len(), 4);
        assert_eq!(bodies(&mut r, &p[0]), [1]);
        assert_eq!(bodies(&mut r, &p[2]), Vec::<u8>::new()); // 2 lost: 3 is held
        assert_eq!(bodies(&mut r, &p[3]), Vec::<u8>::new());
        assert!(s.due(t + Duration::from_millis(10)).is_empty()); // not due yet
        let again = s.due(t + RESEND);
        assert_eq!(again.len(), 4); // nothing acked yet
        assert_eq!(bodies(&mut r, &again[1]), [2, 3, 4]); // gap filled: delivered in order
        assert_eq!(bodies(&mut r, &again[2]), Vec::<u8>::new()); // duplicate
        let (ack, _) = r.on_packet(&again[0]);
        s.on_ack(&ack.unwrap());
        assert_eq!(s.pending(), 3);
    }

    #[test]
    fn receiver_reset_skips_to_base() {
        let (mut s, mut r) = (Sender::new(), Receiver::new());
        let t = Instant::now();
        for i in 1..=3u8 {
            s.push(vec![i]);
        }
        for p in s.due(t) {
            let (ack, _) = r.on_packet(&p);
            s.on_ack(&ack.unwrap());
        }
        r.reset(); // peer timed out on our side; the sender kept counting
        s.push(vec![9]);
        assert_eq!(bodies(&mut r, &s.due(t)[0]), [9]);
    }

    #[test]
    fn sender_drop_does_not_stall() {
        let (mut s, mut r) = (Sender::new(), Receiver::new());
        let t = Instant::now();
        s.push(vec![1]);
        let first = s.due(t);
        assert_eq!(bodies(&mut r, &first[0]), [1]);
        s.push(vec![2]);
        s.push(vec![3]);
        let p = s.due(t);
        s.clear(); // gave up on 2 and 3 (peer timeout on the sender side)
        s.push(vec![4]);
        let q = s.due(t);
        assert_eq!(bodies(&mut r, &q[0]), [4]); // base 4: no wait for 2 and 3
        assert_eq!(bodies(&mut r, &p[0]), Vec::<u8>::new()); // late 2: skipped
    }

    #[test]
    fn rejects_bad_packets() {
        let mut r = Receiver::new();
        assert!(r.on_packet(b"NCRL\x01\x00").0.is_none());
        let mut big = DATA_MAGIC.to_le_bytes().to_vec();
        big.extend_from_slice(&[1, 0, 0, 0, 1, 0, 0, 0]);
        big.extend(vec![0u8; MAX_BODY + 1]);
        assert!(r.on_packet(&big).0.is_none());
        let mut far = Receiver::new();
        let pk = |seq: u32, base: u32| [DATA_MAGIC, seq, base, 0].iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
        assert!(far.on_packet(&pk(1, 1)).0.is_some());
        assert!(far.on_packet(&pk(1 + 1 + MAX_HELD, 1)).0.is_none()); // too far ahead: not acked, resent later
    }
}
