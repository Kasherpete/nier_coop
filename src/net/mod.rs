//! UDP state sync. Runs on its own thread; never touches game memory, only `state` slots.

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::state::{LOCAL, PlayerState, REMOTE, REMOTE_ACTIVE};

const MAGIC: u32 = u32::from_le_bytes(*b"NCO1");
const HEADER: usize = 8; // magic u32, seq u32
pub const PACKET_LEN: usize = HEADER + size_of::<PlayerState>();
const SEND_EVERY: Duration = Duration::from_millis(16); // ~60 Hz, one packet per game frame (~0.1 Mbit/s)
const TIMEOUT: Duration = Duration::from_secs(2);

pub struct Config {
    pub bind: SocketAddr,
    pub peer: SocketAddr,
    /// `log_anims=true`: log every start_animation call (debug level).
    pub log_anims: bool,
    /// `spawn_2b=true`: show the peer as a spawned 2B instead of the game's buddy.
    pub spawn_2b: bool,
    /// `mirror_idle=true` (mirror test only): the copy gets an idle, motionless state, so any movement or
    /// attack it still makes comes from the local controller, which must never happen in co-op.
    pub mirror_idle: bool,
}

/// Reads `bind=` and `peer=` lines from `nier_coop.cfg` next to the game exe
/// (Steam's working directory is not the game folder).
pub fn load_config() -> Option<Config> {
    let path = std::env::current_exe().ok()?.with_file_name("nier_coop.cfg");
    let text = std::fs::read_to_string(path).ok()?;
    let get = |k: &str| {
        text.lines()
            .filter_map(|l| l.split_once('='))
            .find(|(key, _)| key.trim() == k)
            .and_then(|(_, v)| v.trim().parse().ok())
    };
    let flag = |k: &str| text.lines().any(|l| l.split_whitespace().collect::<String>() == format!("{k}=true"));
    Some(Config { bind: get("bind")?, peer: get("peer")?, log_anims: flag("log_anims"), spawn_2b: flag("spawn_2b"), mirror_idle: flag("mirror_idle") })
}

pub fn spawn(cfg: Config) {
    std::thread::spawn(move || {
        if let Err(e) = run(&cfg) {
            tracing::error!("network thread stopped: {e}");
        }
        REMOTE_ACTIVE.store(false, Ordering::Relaxed);
    });
}

fn run(cfg: &Config) -> std::io::Result<()> {
    let sock = UdpSocket::bind(cfg.bind)?;
    sock.set_read_timeout(Some(Duration::from_millis(5)))?;
    tracing::info!("udp bound to {}, peer {}", cfg.bind, cfg.peer);

    let (mut tx_seq, mut rx_seq) = (0u32, None::<u32>);
    let mut last_rx = None::<Instant>;
    let mut next_send = Instant::now();
    let mut buf = [0u8; 512];
    let mut idle = None::<PlayerState>;
    loop {
        if Instant::now() >= next_send {
            next_send += SEND_EVERY;
            if let Some(s) = LOCAL.load() {
                tx_seq = tx_seq.wrapping_add(1);
                sock.send_to(&encode(tx_seq, &s), cfg.peer)?;
            }
        }
        match sock.recv_from(&mut buf) {
            // Only the configured peer is trusted; anything else is dropped.
            Ok((n, from)) if from == cfg.peer => {
                if let Some((seq, mut s)) = decode(&buf[..n]) {
                    // Mirror test (peer == bind): stand the copy 2 m aside, or it collides with the player it copies.
                    if cfg.peer == cfg.bind {
                        s.pos.x += 2.0;
                        if cfg.mirror_idle {
                            // Frozen where it first appeared, no inputs, no animation or button replays.
                            let first = *idle.get_or_insert(s);
                            s = PlayerState { pos: first.pos, rot: first.rot, hp: s.hp, ..Default::default() };
                        }
                    }
                    // Drop reordered/duplicate packets (wrapping compare).
                    if rx_seq.is_none_or(|r| (seq.wrapping_sub(r) as i32) > 0) {
                        rx_seq = Some(seq);
                        REMOTE.store(&s);
                        last_rx = Some(Instant::now());
                        REMOTE_ACTIVE.store(true, Ordering::Relaxed);
                    }
                }
            }
            Ok(_) => {}
            // ConnectionReset: Windows reports ICMP port-unreachable from a previous send_to; harmless.
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::ConnectionReset) => {}
            Err(e) => return Err(e),
        }
        if last_rx.is_some_and(|t| t.elapsed() > TIMEOUT) && REMOTE_ACTIVE.swap(false, Ordering::Relaxed) {
            tracing::warn!("peer timed out; buddy AI restored");
        }
    }
}

pub fn encode(seq: u32, s: &PlayerState) -> [u8; PACKET_LEN] {
    let mut p = [0u8; PACKET_LEN];
    p[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    p[4..8].copy_from_slice(&seq.to_le_bytes());
    p[HEADER..].copy_from_slice(bytemuck::bytes_of(s));
    p
}

/// Trust boundary: exact length, magic, and finite floats only.
pub fn decode(p: &[u8]) -> Option<(u32, PlayerState)> {
    if p.len() != PACKET_LEN || p[0..4] != MAGIC.to_le_bytes() {
        return None;
    }
    let seq = u32::from_le_bytes(p[4..8].try_into().ok()?);
    let s: PlayerState = bytemuck::pod_read_unaligned(&p[HEADER..]);
    let v = [s.pos.x, s.pos.y, s.pos.z, s.pos.w, s.rot.x, s.rot.y, s.rot.z, s.rot.w, s.facing2, s.speed, s.aim.x, s.aim.y, s.aim.z, s.aim.w];
    v.iter().all(|f| f.is_finite() && f.abs() < 1.0e6).then_some((seq, s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::cVec4;
    #[test]
    fn packet_roundtrip_and_rejects() {
        let s = PlayerState { pos: cVec4 { x: 1.0, y: 2.0, z: 3.0, w: 1.0 }, anim: [7, 0, 0, 0], hp: 100, ..Default::default() };
        let p = encode(5, &s);
        assert_eq!(decode(&p), Some((5, s)));
        assert_eq!(decode(&p[..PACKET_LEN - 1]), None);
        let mut bad = p;
        bad[0] ^= 1;
        assert_eq!(decode(&bad), None);
        let mut nan = p;
        nan[HEADER..HEADER + 4].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(decode(&nan), None);
    }
}
