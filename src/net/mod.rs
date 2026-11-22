//! UDP input sync. Runs on its own thread; never touches game memory, only `state`.
//!
//! Each game frame the local controller input is sent with the previous `REDUNDANCY - 1` frames, so a lost
//! packet loses nothing; the receiver applies the frames in order (one per tick) to the remote 2B. The
//! sender's real state rides along only to correct drift.

mod reliable;
mod save;

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::state::{
    self, Correction, Damage, EnemyState, InputFrame, WorldGen, HOST, LOCAL_CORRECTION, LOCAL_DAMAGE, LOCAL_EPOCH, LOCAL_INPUTS, LOCAL_READY,
    LOCAL_EVENTS, LOCAL_SCENE, LOCAL_SIGNALS, LOCAL_WORLD, MAX_ENEMIES, REDUNDANCY, REMOTE_ACTIVE, REMOTE_CORRECTION, REMOTE_DAMAGE, REMOTE_EPOCH, REMOTE_INPUTS,
    REMOTE_EVENTS, REMOTE_READY, REMOTE_SCENE, REMOTE_SIGNALS, REMOTE_WORLD,
};

/// World sync packets: magic, seq, host generation, client generation, count, then `count` items (see
/// `state::WorldGen`). Host -> client snapshots, client -> host damage.
const SNAPSHOT_MAGIC: u32 = u32::from_le_bytes(*b"NCW2");
const DAMAGE_MAGIC: u32 = u32::from_le_bytes(*b"NCD2");
/// Header words between the magic and the count: seq, host generation, client generation.
const LIST_HEAD: usize = 3;
/// Host -> client, every second: the host's scene state names (newline-separated), for quest sync.
const SCENE_MAGIC: u32 = u32::from_le_bytes(*b"NCSS");
const SCENE_EVERY: Duration = Duration::from_secs(1);
/// [magic, action id, kind, hash, flag] just ran here (see `state::LOCAL_EVENTS`), over the reliable channel.
/// Host -> client, and client -> host for objectives the client's player reached.
const EVENT_MAGIC: u32 = u32::from_le_bytes(*b"NCEV");
/// Host -> client: a scene signal, [magic, sender id, hash, arg], over the reliable channel (in order, once).
const SIGNAL_MAGIC: u32 = u32::from_le_bytes(*b"NCSG");

/// Failed sends so far; a send error (adapter down, Wi-Fi drop) is logged, never fatal.
static SEND_ERRORS: AtomicU32 = AtomicU32::new(0);

fn send_failed(to: SocketAddr, e: &std::io::Error) {
    let n = SEND_ERRORS.fetch_add(1, Ordering::Relaxed);
    if n % 600 == 0 {
        tracing::warn!("send to {to} failed: {e} ({} so far); still trying", n + 1);
    }
}

fn send(sock: &UdpSocket, p: &[u8], to: SocketAddr) {
    if let Err(e) = sock.send_to(p, to) {
        send_failed(to, &e);
    }
}

/// Both ways, every second: [magic, this build's version text], so different builds say so instead of failing silently.
const VERSION_MAGIC: u32 = u32::from_le_bytes(*b"NCVR");
const VERSION: &str = env!("CARGO_PKG_VERSION");

const MAGIC: u32 = u32::from_le_bytes(*b"NCO4");
const HEADER: usize = 16; // magic u32, seq u32, epoch u32, flags u32
const FRAMES_AT: usize = HEADER + size_of::<Correction>();
pub const PACKET_LEN: usize = FRAMES_AT + REDUNDANCY * size_of::<InputFrame>(); // 784 bytes, ~0.38 Mbit/s at 60 fps
const TIMEOUT: Duration = Duration::from_secs(2);
/// Heartbeat while no new frame is produced (loading, lockstep stopped), so ready/epoch still reach the peer.
const HEARTBEAT: Duration = Duration::from_millis(50);

/// Packet header flags.
pub const READY: u32 = 1; // sender has a player character (area loaded)
pub const IS_HOST: u32 = 2; // sender runs with host=true
pub const HAS_CORRECTION: u32 = 4; // the correction block holds real state

/// Lockstep status carried in every packet.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Header {
    pub seq: u32,
    pub epoch: u32,
    pub flags: u32,
}

pub struct Config {
    pub bind: SocketAddr,
    pub peer: SocketAddr,
    /// `log_anims=true`: log every start_animation call (debug level).
    pub log_anims: bool,
    /// `spawn_2b=true`: show the peer as a spawned 2B instead of the game's buddy.
    pub spawn_2b: bool,
    /// `mirror_idle=true` (mirror test only): the copy gets idle inputs, so any movement or attack it still
    /// makes comes from the local controller, which must never happen in co-op.
    pub mirror_idle: bool,
    /// `anim_replay=true`: also replay the peer's animations (fallback if inputs alone leave her not walking).
    pub anim_replay: bool,
    /// `lockstep=true`: both games advance frame by frame together and compare world checksums.
    pub lockstep: bool,
    /// `host=true` (exactly one player): starts lockstep sessions; the other player joins them.
    pub host: bool,
    /// `world_sync=true` (both players): the host's game owns the enemies, the client copies them.
    pub world_sync: bool,
    /// `save_sync=true` (both players): the host's newest save slot is copied to the client (see `save.rs`).
    pub save_sync: bool,
    /// `enemy_anim=true` (both players, with world_sync): the client's enemies play the host's enemies' actions.
    pub enemy_anim: bool,
}

/// Reads `key=value` lines from `nier_coop.cfg` next to the game exe
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
    Some(Config {
        bind: get("bind")?,
        peer: get("peer")?,
        log_anims: flag("log_anims"),
        spawn_2b: flag("spawn_2b"),
        mirror_idle: flag("mirror_idle"),
        anim_replay: flag("anim_replay"),
        lockstep: flag("lockstep"),
        host: flag("host"),
        world_sync: flag("world_sync"),
        save_sync: flag("save_sync"),
        enemy_anim: flag("enemy_anim"),
    })
}

pub fn spawn(cfg: Config) {
    std::thread::spawn(move || {
        crate::hooks::savefile::NO_WAIT.with(|n| n.set(true));
        if let Err(e) = run(&cfg) {
            tracing::error!("network thread stopped: {e}");
        }
        REMOTE_ACTIVE.store(false, Ordering::Relaxed);
    });
}

fn run(cfg: &Config) -> std::io::Result<()> {
    let sock = UdpSocket::bind(cfg.bind)?;
    // Short wait so a new local frame goes out within ~1 ms of the game producing it.
    sock.set_read_timeout(Some(Duration::from_millis(1)))?;
    tracing::info!("udp bound to {}, peer {}", cfg.bind, cfg.peer);

    let mirror = cfg.peer == cfg.bind;
    // The peer's IP is trusted, its port is learned: a home router may change it (no port forward needed then).
    let mut peer = cfg.peer;
    let (mut tx_seq, mut rx_seq) = (0u32, None::<u32>);
    let mut last_rx = None::<Instant>;
    let (mut sent_frame, mut sent_at) = (0u32, Instant::now());
    let mut rx_epoch = None::<u32>;
    let mut warned_hosts = false;
    let mut world_rx_seq = None::<u32>;
    let mut scene_at = Instant::now();
    let mut version_at = Instant::now() - SCENE_EVERY;
    let (mut peer_version, mut warned_layout) = (None::<String>, false);
    // Story events and scene signals: resent until acked, delivered once and in order.
    let (mut rel_tx, mut rel_rx) = (reliable::Sender::new(), reliable::Receiver::new());
    // shortcut: one datagram per scene list (~1-4 KB, IP-fragmented); split it if late-game lists outgrow 64 KB.
    let mut buf = vec![0u8; 65536];
    let mut idle = None::<Correction>;
    let mut saves = if cfg.save_sync { save::SaveSync::new(cfg.host) } else { None };
    if cfg.save_sync && saves.is_none() {
        tracing::error!("save sync: save folder (Documents\\My Games\\NieR_Automata) not found");
    }
    loop {
        if let Some(s) = saves.as_mut().filter(|_| REMOTE_ACTIVE.load(Ordering::Relaxed)) {
            if let Err(e) = s.poll(&sock, peer) {
                send_failed(peer, &e);
            }
        }
        // One packet per new local frame, plus a heartbeat while no frames are produced.
        let frames: Vec<InputFrame> = state::lock(&LOCAL_INPUTS).iter().copied().collect();
        let corr = *state::lock(&LOCAL_CORRECTION);
        let newest = frames.last().map_or(0, |f| f.frame);
        if newest != sent_frame || sent_at.elapsed() >= HEARTBEAT {
            sent_frame = newest;
            sent_at = Instant::now();
            tx_seq = tx_seq.wrapping_add(1);
            let flags = (LOCAL_READY.load(Ordering::Relaxed) as u32 * READY)
                | (HOST.load(Ordering::Relaxed) as u32 * IS_HOST)
                | (corr.is_some() as u32 * HAS_CORRECTION);
            let h = Header { seq: tx_seq, epoch: LOCAL_EPOCH.load(Ordering::Relaxed), flags };
            send(&sock, &encode(h, &corr.unwrap_or_default(), &frames), peer);
            if cfg.world_sync {
                // Same pace as the inputs: the host's newest snapshot, or the client's damage totals.
                let p = if cfg.host {
                    let w = state::lock(&LOCAL_WORLD);
                    encode_list(SNAPSHOT_MAGIC, [tx_seq, w.0.host, w.0.client], &w.1)
                } else {
                    let d = state::lock(&LOCAL_DAMAGE);
                    encode_list(DAMAGE_MAGIC, [tx_seq, d.0.host, d.0.client], &d.1)
                };
                send(&sock, &p, peer);
            }
        }
        if cfg.world_sync && cfg.host {
            for [id, hash, arg] in std::mem::take(&mut *state::lock(&LOCAL_SIGNALS)) {
                rel_tx.push([SIGNAL_MAGIC, id, hash, arg].iter().flat_map(|v| v.to_le_bytes()).collect());
            }
        }
        // Both ways: the client sends the objectives its player reached (see `hooks::events`).
        if cfg.world_sync {
            for e in std::mem::take(&mut *state::lock(&LOCAL_EVENTS)) {
                rel_tx.push([EVENT_MAGIC].iter().chain(&e).flat_map(|v| v.to_le_bytes()).collect());
            }
        }
        for p in rel_tx.due(Instant::now()) {
            send(&sock, &p, peer);
        }
        if cfg.world_sync && cfg.host && scene_at.elapsed() >= SCENE_EVERY {
            scene_at = Instant::now();
            let names = state::lock(&LOCAL_SCENE).clone();
            if !names.is_empty() && names.len() < buf.len() - 4 {
                send(&sock, &[&SCENE_MAGIC.to_le_bytes()[..], &names].concat(), peer);
            }
        }
        if version_at.elapsed() >= SCENE_EVERY {
            version_at = Instant::now();
            send(&sock, &[&VERSION_MAGIC.to_le_bytes()[..], VERSION.as_bytes()].concat(), peer);
        }
        match sock.recv_from(&mut buf) {
            // Only the configured peer's IP is trusted; anything else is dropped.
            // A new port is learned only between sessions, so another program on the peer's IP cannot take over one.
            Ok((n, from)) if from == peer || (from.ip() == cfg.peer.ip() && !REMOTE_ACTIVE.load(Ordering::Relaxed)) => {
                if from != peer {
                    tracing::info!("peer answers from {from}");
                    peer = from;
                }
                // Save data only inside a live session (input packets already flowing), never as a first packet.
                if let Some(s) = saves.as_mut().filter(|_| REMOTE_ACTIVE.load(Ordering::Relaxed)) {
                    match s.on_packet(&buf[..n], &sock, peer) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(e) => {
                            send_failed(peer, &e);
                            continue;
                        }
                    }
                }
                // Scene signals/events/names, like saves, only inside a live session (never as a first packet).
                let live = REMOTE_ACTIVE.load(Ordering::Relaxed);
                if let Some(v) = decode_version(&buf[..n]) {
                    if peer_version.as_deref() != Some(v) {
                        if v == VERSION {
                            tracing::info!("peer runs nier_coop v{v} (same as you)");
                        } else {
                            tracing::warn!("VERSION MISMATCH: peer runs nier_coop v{v}, you run v{VERSION}. Both install the same zip.");
                        }
                        peer_version = Some(v.to_string());
                    }
                } else if n >= 4 && buf[..4] == reliable::DATA_MAGIC.to_le_bytes() {
                    // Story events and signals only inside a live session; not acked otherwise, so they come again.
                    if live {
                        let (ack, bodies) = rel_rx.on_packet(&buf[..n]);
                        if let Some(a) = ack {
                            send(&sock, &a, peer);
                        }
                        for b in bodies {
                            deliver(&b, cfg.host);
                        }
                    }
                } else if n >= 4 && buf[..4] == reliable::ACK_MAGIC.to_le_bytes() {
                    rel_tx.on_ack(&buf[..n]);
                } else if n >= 4 && buf[..4] == SCENE_MAGIC.to_le_bytes() {
                    if live && !cfg.host {
                        *state::lock(&REMOTE_SCENE) = Some(buf[4..n].to_vec()); // validated by the game thread
                    }
                } else if let Some((h, s)) = decode_list::<EnemyState>(SNAPSHOT_MAGIC, &buf[..n]) {
                    let fresh = world_rx_seq.is_none_or(|r: u32| (h[0].wrapping_sub(r) as i32) > 0);
                    // One enemy with a bad float is left out, not the whole snapshot (that stalled every enemy).
                    let s: Vec<EnemyState> = s.into_iter().filter(|e| sane(&[e.yaw, e.pos.x, e.pos.y, e.pos.z, e.pos.w])).collect();
                    if !cfg.host && fresh {
                        world_rx_seq = Some(h[0]);
                        *state::lock(&REMOTE_WORLD) = Some((Instant::now(), WorldGen { host: h[1], client: h[2] }, s));
                    }
                } else if let Some((h, d)) = decode_list::<Damage>(DAMAGE_MAGIC, &buf[..n]) {
                    // Totals only grow within a generation, so order does not matter: the host applies what is new.
                    if cfg.host {
                        *state::lock(&REMOTE_DAMAGE) = Some((WorldGen { host: h[1], client: h[2] }, d));
                    }
                } else if let Some((h, mut c, mut frames)) = decode(&buf[..n]) {
                    let seq = h.seq;
                    REMOTE_READY.store(h.flags & READY != 0, Ordering::Relaxed);
                    REMOTE_EPOCH.store(h.epoch, Ordering::Relaxed);
                    if h.flags & IS_HOST != 0 && HOST.load(Ordering::Relaxed) && !mirror && !warned_hosts {
                        warned_hosts = true;
                        tracing::warn!("both players have host=true; set it on exactly one, or lockstep never lines up");
                    }
                    // A new lockstep session restarts the peer's frame numbers.
                    if rx_epoch != Some(h.epoch) {
                        rx_epoch = Some(h.epoch);
                        state::lock(&REMOTE_INPUTS).reset();
                    }
                    // Mirror test (peer == bind): stand the copy 2 m aside, or it collides with the player it copies.
                    if mirror {
                        c.pos.x += 2.0;
                        for f in &mut frames {
                            f.pos.x += 2.0;
                        }
                        if cfg.mirror_idle {
                            // Frozen where it first appeared, idle inputs, no animation replays.
                            let first = *idle.get_or_insert(c);
                            c = Correction { pos: first.pos, rot: first.rot, hp: c.hp, ..Default::default() };
                            for f in &mut frames {
                                *f = InputFrame { frame: f.frame, checksum: f.checksum, pos: first.pos, ..Default::default() };
                            }
                        }
                    }
                    state::lock(&REMOTE_INPUTS).push(&frames);
                    // Corrections only move forward (wrapping compare); inputs are ordered by the queue itself.
                    if h.flags & HAS_CORRECTION == 0 {
                        *state::lock(&REMOTE_CORRECTION) = None; // no player there (loading): nothing to correct toward
                    } else if rx_seq.is_none_or(|r| (seq.wrapping_sub(r) as i32) > 0) {
                        rx_seq = Some(seq);
                        *state::lock(&REMOTE_CORRECTION) = Some(c);
                    }
                    last_rx = Some(Instant::now());
                    if !REMOTE_ACTIVE.swap(true, Ordering::Relaxed) {
                        tracing::info!("connected to {from}");
                    }
                } else if !warned_layout && n >= 4 && buf[..3] == *b"NCO" && buf[..4] != MAGIC.to_le_bytes() {
                    // An input packet with another layout tag (NCO2, NCO4...): older builds send no NCVR.
                    warned_layout = true;
                    tracing::warn!("VERSION MISMATCH: peer's input packets are from another nier_coop build (you run v{VERSION}). Both install the same zip.");
                }
            }
            Ok(_) => {}
            // ConnectionReset: Windows reports ICMP port-unreachable from a previous send_to; harmless.
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::ConnectionReset) => {}
            Err(e) => {
                // Not fatal either: the socket stays bound, the network may come back.
                send_failed(peer, &e);
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if last_rx.is_some_and(|t| t.elapsed() > TIMEOUT) && REMOTE_ACTIVE.swap(false, Ordering::Relaxed) {
            tracing::warn!("peer timed out; buddy AI restored");
            rx_seq = None; // a restarted peer counts from 1 again
            rx_epoch = None;
            world_rx_seq = None;
            peer_version = None; // the peer may come back with another build
            if let Some(s) = saves.as_mut() {
                s.reset();
            }
            *state::lock(&REMOTE_WORLD) = None;
            *state::lock(&REMOTE_SCENE) = None;
            rel_tx.clear(); // the peer's next session starts after what we still had queued
            rel_rx.reset();
            state::lock(&REMOTE_SIGNALS).clear();
            *state::lock(&REMOTE_DAMAGE) = None;
            REMOTE_READY.store(false, Ordering::Relaxed);
            state::lock(&REMOTE_INPUTS).reset();
        }
    }
}

/// `frames` is oldest first; at most `REDUNDANCY` are sent, unused slots stay zero (frame 0 = empty).
pub fn encode(h: Header, c: &Correction, frames: &[InputFrame]) -> [u8; PACKET_LEN] {
    let mut p = [0u8; PACKET_LEN];
    p[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    p[4..8].copy_from_slice(&h.seq.to_le_bytes());
    p[8..12].copy_from_slice(&h.epoch.to_le_bytes());
    p[12..16].copy_from_slice(&h.flags.to_le_bytes());
    p[HEADER..FRAMES_AT].copy_from_slice(bytemuck::bytes_of(c));
    let start = frames.len().saturating_sub(REDUNDANCY);
    for (i, f) in frames[start..].iter().enumerate() {
        let at = FRAMES_AT + i * size_of::<InputFrame>();
        p[at..at + size_of::<InputFrame>()].copy_from_slice(bytemuck::bytes_of(f));
    }
    p
}

/// One message from the reliable channel (already in order and deduplicated). Trust boundary: exact lengths.
fn deliver(b: &[u8], host: bool) {
    let w = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
    if b.len() == 16 && w(0) == SIGNAL_MAGIC {
        if !host {
            state::lock(&REMOTE_SIGNALS).push([w(1), w(2), w(3)]);
        }
    } else if b.len() == 20 && w(0) == EVENT_MAGIC {
        // The host only takes objectives the client's player reached: kind 2, DONE.
        if !host || (w(2) == 2 && w(3) == 0x26b8_0d1f) {
            state::lock(&REMOTE_EVENTS).push([w(1), w(2), w(3), w(4)]);
        }
    }
}

/// Bytes before the items of a world sync list: magic, `LIST_HEAD` words, count.
const LIST_AT: usize = 4 * (LIST_HEAD + 2);

/// World sync packet: magic, the header words, count, then at most `MAX_ENEMIES` items.
fn encode_list<T: bytemuck::Pod>(magic: u32, head: [u32; LIST_HEAD], items: &[T]) -> Vec<u8> {
    let items = &items[..items.len().min(MAX_ENEMIES)];
    let mut p = Vec::with_capacity(LIST_AT + size_of_val(items));
    for w in [magic].iter().chain(&head).chain(&[items.len() as u32]) {
        p.extend_from_slice(&w.to_le_bytes());
    }
    p.extend_from_slice(bytemuck::cast_slice(items));
    p
}

/// Trust boundary: magic, count within `MAX_ENEMIES`, and exact length for that count.
fn decode_list<T: bytemuck::Pod>(magic: u32, p: &[u8]) -> Option<([u32; LIST_HEAD], Vec<T>)> {
    let word = |i: usize| p.get(i * 4..i * 4 + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let n = word(LIST_HEAD + 1)? as usize;
    if word(0)? != magic || n > MAX_ENEMIES || p.len() != LIST_AT + n * size_of::<T>() {
        return None;
    }
    let head = [word(1)?, word(2)?, word(3)?];
    Some((head, p[LIST_AT..].chunks_exact(size_of::<T>()).map(bytemuck::pod_read_unaligned).collect()))
}

/// Trust boundary: magic, then 1-32 printable ASCII characters (the text goes into the log).
fn decode_version(p: &[u8]) -> Option<&str> {
    let v = p.strip_prefix(&VERSION_MAGIC.to_le_bytes()[..])?;
    (!v.is_empty() && v.len() <= 32 && v.iter().all(u8::is_ascii_graphic)).then(|| std::str::from_utf8(v).ok())?
}

fn sane(v: &[f32]) -> bool {
    v.iter().all(|f| f.is_finite() && f.abs() < 1.0e6)
}

/// Trust boundary: exact length, magic, and finite floats only.
pub fn decode(p: &[u8]) -> Option<(Header, Correction, [InputFrame; REDUNDANCY])> {
    if p.len() != PACKET_LEN || p[0..4] != MAGIC.to_le_bytes() {
        return None;
    }
    let word = |i: usize| u32::from_le_bytes(p[i..i + 4].try_into().unwrap());
    let h = Header { seq: word(4), epoch: word(8), flags: word(12) };
    let c: Correction = bytemuck::pod_read_unaligned(&p[HEADER..FRAMES_AT]);
    let frames: [InputFrame; REDUNDANCY] = bytemuck::pod_read_unaligned(&p[FRAMES_AT..]);
    let floats_ok = sane(&[c.pos.x, c.pos.y, c.pos.z, c.pos.w, c.rot.x, c.rot.y, c.rot.z, c.rot.w, c.pod_cooldown])
        && frames.iter().all(|f| sane(&[f.speed, f.facing2, f.aim.x, f.aim.y, f.aim.z, f.aim.w, f.pos.x, f.pos.y, f.pos.z, f.pos.w]));
    floats_ok.then_some((h, c, frames))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::cVec4;

    #[test]
    fn version_packet() {
        let m = VERSION_MAGIC.to_le_bytes();
        assert_eq!(decode_version(&[&m[..], VERSION.as_bytes()].concat()), Some(VERSION));
        assert_eq!(decode_version(&[&m[..], b"0.0.9"].concat()), Some("0.0.9"));
        assert_eq!(decode_version(&m), None); // empty
        assert_eq!(decode_version(&[&m[..], b"1\n2"].concat()), None); // control char
        assert_eq!(decode_version(&[&m[..], &[b'1'; 33][..]].concat()), None); // too long
        assert_eq!(decode_version(b"NCO3\x01\x00"), None);
    }

    /// Plays the host for a game running as client (`bind=127.0.0.1:27015`, `peer=127.0.0.1:27016`,
    /// `save_sync=true`): heartbeats plus this PC's newest save, until the game acks it.
    /// `cargo test fake_host -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn fake_host_for_running_game() {
        let sock = UdpSocket::bind("127.0.0.1:27016").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
        let game: SocketAddr = "127.0.0.1:27015".parse().unwrap();
        let mut s = save::SaveSync::new(true).expect("save folder");
        let (start, mut beat, mut seq) = (Instant::now(), Instant::now() - HEARTBEAT, 0);
        let mut buf = [0u8; 2048];
        while start.elapsed() < Duration::from_secs(120) {
            if beat.elapsed() >= HEARTBEAT {
                beat = Instant::now();
                seq += 1;
                sock.send_to(&encode(Header { seq, epoch: 0, flags: IS_HOST }, &Correction::default(), &[]), game).unwrap();
            }
            s.poll(&sock, game).unwrap();
            if let Ok((n, _)) = sock.recv_from(&mut buf) {
                if n >= 4 && buf[..4] == save::ACK_MAGIC.to_le_bytes() {
                    println!("game acked the save after {:?}", start.elapsed());
                    return;
                }
            }
        }
        panic!("no ack from the game in 120 s");
    }

    /// Enemy sync without a second player, step 1. Plays the client for a game running as host
    /// (`peer=127.0.0.1:27016`, `host=true`): records 60 s of the game's enemy snapshots to a file.
    /// `cargo test record_enemies -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn record_enemies_from_running_host() {
        let sock = UdpSocket::bind("127.0.0.1:27016").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
        let game: SocketAddr = "127.0.0.1:27015".parse().unwrap();
        let (mut beat, mut seq, mut first) = (Instant::now() - HEARTBEAT, 0, None::<Instant>);
        let (mut out, mut count, mut buf) = (Vec::new(), 0, [0u8; 2048]);
        let deadline = Instant::now() + Duration::from_secs(600);
        while first.is_none_or(|t| t.elapsed() < Duration::from_secs(60)) {
            assert!(Instant::now() < deadline, "no enemy snapshots in 10 min (game running as host, near enemies?)");
            if beat.elapsed() >= HEARTBEAT {
                beat = Instant::now();
                seq += 1;
                sock.send_to(&encode(Header { seq, epoch: 0, flags: READY }, &Correction::default(), &[]), game).unwrap();
            }
            if let Ok((n, _)) = sock.recv_from(&mut buf) {
                if let Some((_, s)) = decode_list::<EnemyState>(SNAPSHOT_MAGIC, &buf[..n]) {
                    let t = *first.get_or_insert_with(|| {
                        println!("recording {} enemies, 60 s...", s.len());
                        Instant::now()
                    });
                    out.extend_from_slice(&(t.elapsed().as_millis() as u32).to_le_bytes());
                    out.extend_from_slice(&(n as u32).to_le_bytes());
                    out.extend_from_slice(&buf[..n]);
                    count += 1;
                }
            }
        }
        std::fs::write(enemy_recording(), out).unwrap();
        println!("saved {count} snapshots to {}", enemy_recording().display());
    }

    /// Step 2. Plays the host for a game running as client (`peer=127.0.0.1:27016`, `host=false`): replays the
    /// recording in a loop for 10 min and prints the damage the game reports. Its log shows "N/M host enemies matched".
    /// `cargo test replay_enemies -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn replay_enemies_to_running_client() {
        let rec = std::fs::read(enemy_recording()).expect("run record_enemies_from_running_host first");
        let mut packets = Vec::new();
        let mut at = 0;
        while at + 8 <= rec.len() {
            let word = |i: usize| u32::from_le_bytes(rec[i..i + 4].try_into().unwrap());
            let (ms, n) = (word(at), word(at + 4) as usize);
            packets.push((Duration::from_millis(ms as u64), &rec[at + 8..at + 8 + n]));
            at += 8 + n;
        }
        let total = packets.last().expect("empty recording").0 + Duration::from_millis(16);
        let sock = UdpSocket::bind("127.0.0.1:27016").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
        let game: SocketAddr = "127.0.0.1:27015".parse().unwrap();
        let (start, mut beat, mut seq, mut next, mut lap) = (Instant::now(), Instant::now() - HEARTBEAT, 0, 0, 0u32);
        let mut buf = [0u8; 2048];
        while start.elapsed() < Duration::from_secs(600) {
            if beat.elapsed() >= HEARTBEAT {
                beat = Instant::now();
                seq += 1;
                sock.send_to(&encode(Header { seq, epoch: 0, flags: IS_HOST }, &Correction::default(), &[]), game).unwrap();
            }
            // Snapshot seq numbers must keep growing across laps, or the game drops them as old.
            let t = start.elapsed().saturating_sub(total * lap);
            if t >= total {
                lap += 1;
                next = 0;
            } else if next < packets.len() && t >= packets[next].0 {
                let mut p = packets[next].1.to_vec();
                p[4..8].copy_from_slice(&(lap * packets.len() as u32 + next as u32 + 1).to_le_bytes());
                sock.send_to(&p, game).unwrap();
                next += 1;
            }
            if let Ok((n, _)) = sock.recv_from(&mut buf) {
                if let Some((_, d)) = decode_list::<Damage>(DAMAGE_MAGIC, &buf[..n]) {
                    if !d.is_empty() {
                        println!("game reports damage: {:?}", d.iter().map(|x| (x.handle, x.total)).collect::<Vec<_>>());
                    }
                }
            }
        }
    }

    /// Quest sync without a second player: run the game as client (`host=false`, `world_sync=true`,
    /// bind 127.0.0.1:27015, peer 127.0.0.1:27016), load the newest save, then run this. It plays the host and sends
    /// that save's SceneState list without "Desert_first" for 70 s, then the full list for 70 s: the log should
    /// show `quest sync: - Desert_first`, then `+ Desert_first`.
    /// `cargo test --release --target x86_64-pc-windows-msvc fake_host_scene_states -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn fake_host_scene_states() {
        let dir = save::save_dir().expect("save folder");
        let newest = (0..3)
            .filter_map(|n| {
                let p = dir.join(format!("SlotData_{n}.dat"));
                Some((std::fs::metadata(&p).ok()?.modified().ok()?, p))
            })
            .max()
            .expect("no save")
            .1;
        let text = String::from_utf8_lossy(&std::fs::read(&newest).unwrap()).into_owned();
        let names: Vec<&str> = text
            .lines()
            .skip_while(|l| *l != "SceneState")
            .skip(1)
            .take_while(|l| l.starts_with(' '))
            .map(str::trim)
            .collect();
        println!("{}: {} scene states", newest.display(), names.len());
        let sock = UdpSocket::bind("127.0.0.1:27016").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
        let game: SocketAddr = "127.0.0.1:27015".parse().unwrap();
        let (start, mut beat, mut list_at, mut seq) = (Instant::now(), Instant::now() - HEARTBEAT, Instant::now(), 0);
        let mut buf = [0u8; 2048];
        while start.elapsed() < Duration::from_secs(150) {
            if beat.elapsed() >= HEARTBEAT {
                beat = Instant::now();
                seq += 1;
                sock.send_to(&encode(Header { seq, epoch: 0, flags: IS_HOST }, &Correction::default(), &[]), game).unwrap();
            }
            if start.elapsed() > Duration::from_secs(10) && list_at.elapsed() >= SCENE_EVERY {
                list_at = Instant::now();
                let with = start.elapsed() < Duration::from_secs(80);
                // A real state: the game's set() refuses names it does not know.
                let l: Vec<&str> = names.iter().copied().filter(|n| !with || *n != "Desert_first").collect();
                sock.send_to(&[&SCENE_MAGIC.to_le_bytes()[..], l.join("\n").as_bytes()].concat(), game).unwrap();
                println!("{:>3} s: sent {} states{}", start.elapsed().as_secs(), l.len(), if with { " (without Desert_first)" } else { "" });
            }
            let _ = sock.recv_from(&mut buf);
        }
    }

    fn enemy_recording() -> std::path::PathBuf {
        std::env::temp_dir().join("nier_coop_enemies.rec")
    }
    #[test]
    fn packet_roundtrip_and_rejects() {
        let c = Correction { pos: cVec4 { x: 1.0, y: 2.0, z: 3.0, w: 1.0 }, anim: [7, 0, 0, 0], hp: 100, ..Default::default() };
        let frames: Vec<_> = (1..=10).map(|i| InputFrame { frame: i, speed: 0.5, ..Default::default() }).collect();
        let h = Header { seq: 5, epoch: 3, flags: READY | HAS_CORRECTION };
        let p = encode(h, &c, &frames);
        let (h2, c2, f2) = decode(&p).unwrap();
        assert_eq!((h2, c2), (h, c));
        assert_eq!(f2.map(|f| f.frame), [3, 4, 5, 6, 7, 8, 9, 10]); // newest REDUNDANCY frames, oldest first

        let short = encode(Header::default(), &c, &frames[..2]);
        assert_eq!(decode(&short).unwrap().2.map(|f| f.frame), [1, 2, 0, 0, 0, 0, 0, 0]);

        assert_eq!(decode(&p[..PACKET_LEN - 1]), None);
        let mut bad = p;
        bad[0] ^= 1;
        assert_eq!(decode(&bad), None);
        let mut nan = p;
        nan[HEADER..HEADER + 4].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(decode(&nan), None);
        let mut nan_input = p;
        let speed_at = FRAMES_AT + 4;
        nan_input[speed_at..speed_at + 4].copy_from_slice(&f32::INFINITY.to_le_bytes());
        assert_eq!(decode(&nan_input), None);
    }

    #[test]
    fn world_lists_roundtrip_and_reject() {
        let e = EnemyState { handle: 0x1234_5600, kind: 7, hp: 90, yaw: 1.5, pos: cVec4 { x: 1.0, ..Default::default() }, anim: [3, 0, 0, 1], anim_seq: 5, applied: 4 };
        let p = encode_list(SNAPSHOT_MAGIC, [9, 2, 3], &[e, e]);
        assert_eq!(decode_list::<EnemyState>(SNAPSHOT_MAGIC, &p), Some(([9, 2, 3], vec![e, e])));
        assert_eq!(decode_list::<EnemyState>(DAMAGE_MAGIC, &p), None); // wrong type
        assert_eq!(decode_list::<EnemyState>(SNAPSHOT_MAGIC, &p[..p.len() - 1]), None);
        assert_eq!(decode_list::<EnemyState>(SNAPSHOT_MAGIC, &p[..4]), None);
        let mut huge = p.clone();
        huge[16..20].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(decode_list::<EnemyState>(SNAPSHOT_MAGIC, &huge), None);
        assert_eq!(encode_list(SNAPSHOT_MAGIC, [0; 3], &vec![e; 40]).len(), LIST_AT + MAX_ENEMIES * size_of::<EnemyState>()); // capped
        assert!(LIST_AT + MAX_ENEMIES * size_of::<EnemyState>() <= 1472); // one UDP payload, no fragmentation
        let d = encode_list(DAMAGE_MAGIC, [1, 2, 3], &[Damage { handle: 5, total: 30 }]);
        assert_eq!(decode_list::<Damage>(DAMAGE_MAGIC, &d), Some(([1, 2, 3], vec![Damage { handle: 5, total: 30 }])));
        assert_eq!(decode_list::<Damage>(DAMAGE_MAGIC, &encode_list::<Damage>(DAMAGE_MAGIC, [2, 0, 0], &[])), Some(([2, 0, 0], vec![])));
    }

    #[test]
    fn reliable_bodies_parse() {
        state::lock(&REMOTE_SIGNALS).clear();
        state::lock(&REMOTE_EVENTS).clear();
        let sig: Vec<u8> = [SIGNAL_MAGIC, 1, 2, 3].iter().flat_map(|v| v.to_le_bytes()).collect();
        let ev: Vec<u8> = [EVENT_MAGIC, 7, 2, 0x26b8_0d1f, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        let other: Vec<u8> = [EVENT_MAGIC, 7, 0, 5, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        deliver(&sig, false);
        deliver(&ev, true); // host takes a reached objective
        deliver(&other, true); // but not a cutscene from the client
        deliver(&sig[..12], false); // short: ignored
        assert_eq!(*state::lock(&REMOTE_SIGNALS), vec![[1, 2, 3]]);
        assert_eq!(*state::lock(&REMOTE_EVENTS), vec![[7, 2, 0x26b8_0d1f, 0]]);
    }
}
