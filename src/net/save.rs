//! Save sync (`save_sync=true` on both): the host sends its most recently saved slot (SlotData_N.dat) whenever it
//! changes; the client writes it to the same slot, backing up its own file once. Load it from the title menu.
//! Chunks are re-sent in rounds until the client acks the whole file's CRC, so lost packets only cost time.

use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crate::hooks::crc32;

pub const CHUNK_MAGIC: u32 = u32::from_le_bytes(*b"NCSV");
pub const ACK_MAGIC: u32 = u32::from_le_bytes(*b"NCSA");
const CHUNK: usize = 1024;
const HEAD: usize = 20; // magic, slot, crc, total, offset
/// Every save slot is exactly this size (Steam build); anything else is not a save and is never written.
/// shortcut: no shared key, so a spoofed peer IP:port could still send a save-sized file; add an HMAC key if that matters.
const SAVE_SIZE: usize = 235_980;
const SLOTS: u32 = 3;
/// Gap between chunks: ~0.5 MB/s, a full save in ~0.5 s per round.
const GAP: Duration = Duration::from_millis(2);

struct Out {
    slot: u32,
    crc: u32,
    data: Vec<u8>,
    next: usize,
    acked: bool,
    mtime: SystemTime,
}

struct In {
    slot: u32,
    crc: u32,
    data: Vec<u8>,
    have: Vec<bool>,
    left: usize,
}

pub struct SaveSync {
    host: bool,
    dir: PathBuf,
    out: Option<Out>,
    inc: Option<In>,
    written: Option<u32>, // client: crc of the save already written
    checked: Instant,
    sent: Instant,
}

impl SaveSync {
    pub fn new(host: bool) -> Option<Self> {
        let dir = save_dir()?;
        tracing::info!("save sync ({}): {}", if host { "host sends" } else { "client receives" }, dir.display());
        let now = Instant::now();
        Some(Self { host, dir, out: None, inc: None, written: None, checked: now - Duration::from_secs(5), sent: now })
    }

    /// Host: re-reads the newest slot every second and sends the next chunk while the client has not acked it.
    pub fn poll(&mut self, sock: &UdpSocket, peer: SocketAddr) -> std::io::Result<()> {
        if !self.host {
            return Ok(());
        }
        if self.checked.elapsed() >= Duration::from_secs(1) {
            self.checked = Instant::now();
            self.refresh();
        }
        let Some(o) = self.out.as_mut().filter(|o| !o.acked) else { return Ok(()) };
        if self.sent.elapsed() < GAP {
            return Ok(());
        }
        self.sent = Instant::now();
        let end = (o.next + CHUNK).min(o.data.len());
        let mut p = Vec::with_capacity(HEAD + CHUNK);
        for v in [CHUNK_MAGIC, o.slot, o.crc, o.data.len() as u32, o.next as u32] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        p.extend_from_slice(&o.data[o.next..end]);
        o.next = if end == o.data.len() { 0 } else { end };
        sock.send_to(&p, peer).map(|_| ())
    }

    fn refresh(&mut self) {
        let newest = (0..SLOTS)
            .filter_map(|s| Some((s, std::fs::metadata(self.dir.join(slot_name(s))).ok()?.modified().ok()?)))
            .max_by_key(|&(_, t)| t);
        let Some((slot, mtime)) = newest else { return };
        // Unchanged, or written under a second ago (the game may still be writing it).
        if self.out.as_ref().is_some_and(|o| o.slot == slot && o.mtime == mtime)
            || mtime.elapsed().is_ok_and(|e| e < Duration::from_secs(1))
        {
            return;
        }
        let Ok(data) = std::fs::read(self.dir.join(slot_name(slot))) else { return };
        if data.len() != SAVE_SIZE {
            return; // not a save (checked every second, so no log)
        }
        let crc = crc32(&data);
        if self.out.as_ref().is_some_and(|o| o.crc == crc && o.slot == slot) {
            self.out.as_mut().unwrap().mtime = mtime;
            return;
        }
        tracing::info!("save sync: sending slot {} ({} bytes)", slot + 1, data.len());
        self.out = Some(Out { slot, crc, data, next: 0, acked: false, mtime });
    }

    /// Handles a save packet; false if `p` is not one.
    pub fn on_packet(&mut self, p: &[u8], sock: &UdpSocket, peer: SocketAddr) -> std::io::Result<bool> {
        let word = |i: usize| p.get(i * 4..i * 4 + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
        match word(0) {
            Some(ACK_MAGIC) => {
                if let (true, Some(crc), Some(o)) = (self.host, word(1), self.out.as_mut()) {
                    if crc == o.crc && !o.acked {
                        o.acked = true;
                        tracing::info!("save sync: friend has slot {}", o.slot + 1);
                    }
                }
                Ok(true)
            }
            Some(CHUNK_MAGIC) if !self.host => {
                if let Some((slot, data)) = self.receive(p) {
                    let crc = crc32(&data);
                    let mut data = data;
                    match own_steam_id(&self.dir) {
                        Some(id) => set_owner(&mut data, id),
                        None => tracing::warn!("save sync: own Steam ID not found (no GameData.dat?); the slot may show as empty"),
                    }
                    let same = std::fs::read(self.dir.join(slot_name(slot))).is_ok_and(|d| d == data);
                    let result = if same { Ok(()) } else { self.write(slot, &data) };
                    if result.is_ok() {
                        crate::hooks::savefile::SAVE_READY.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    match result {
                        // The game lists saves only at startup: a save written while it runs shows after a restart.
                        Ok(()) if same => {
                            self.written = Some(crc);
                            tracing::info!("save sync: you have the host's save: Load Game -> slot {} (not Continue)", slot + 1);
                        }
                        Ok(()) => {
                            self.written = Some(crc);
                            tracing::info!("save sync: host's save written to slot {0}: Load Game -> slot {0} (not Continue)", slot + 1);
                        }
                        Err(e) => tracing::error!("save sync: could not write slot {}: {e}", slot + 1),
                    }
                }
                // Ack every chunk of a save we already have, so the host stops sending even if an ack was lost.
                if let (Some(w), Some(crc)) = (self.written, word(2)) {
                    if w == crc {
                        let mut a = ACK_MAGIC.to_le_bytes().to_vec();
                        a.extend_from_slice(&crc.to_le_bytes());
                        sock.send_to(&a, peer)?;
                    }
                }
                Ok(true)
            }
            Some(CHUNK_MAGIC) => Ok(true), // host: own chunks in the mirror test
            _ => Ok(false),
        }
    }

    /// Adds one chunk; the whole file once every chunk is in and its CRC matches. Untrusted input: all checked.
    fn receive(&mut self, p: &[u8]) -> Option<(u32, Vec<u8>)> {
        let word = |i: usize| p.get(i * 4..i * 4 + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
        let (slot, crc, total, off) = (word(1)?, word(2)?, word(3)? as usize, word(4)? as usize);
        let body = &p[HEAD..];
        if slot >= SLOTS || total != SAVE_SIZE || off % CHUNK != 0 || off >= total || body.len() != CHUNK.min(total - off)
        {
            return None;
        }
        if self.written == Some(crc) {
            return None;
        }
        if !self.inc.as_ref().is_some_and(|i| i.crc == crc && i.slot == slot && i.data.len() == total) {
            let n = total.div_ceil(CHUNK);
            self.inc = Some(In { slot, crc, data: vec![0; total], have: vec![false; n], left: n });
        }
        let i = self.inc.as_mut()?;
        if !std::mem::replace(&mut i.have[off / CHUNK], true) {
            i.data[off..off + body.len()].copy_from_slice(body);
            i.left -= 1;
        }
        if i.left > 0 {
            return None;
        }
        let i = self.inc.take()?;
        (crc32(&i.data) == i.crc).then_some((i.slot, i.data))
    }

    /// Peer timed out: the host sends the save again on reconnect (the friend may have restarted the game).
    pub fn reset(&mut self) {
        if let Some(o) = self.out.as_mut() {
            o.acked = false;
        }
        self.inc = None;
    }

    /// Writes the slot; the player's own file is kept once as SlotData_N.dat.nier_coop.bak (never overwritten).
    fn write(&self, slot: u32, data: &[u8]) -> std::io::Result<()> {
        let path = self.dir.join(slot_name(slot));
        let bak = path.with_extension("dat.nier_coop.bak");
        if path.exists() && !bak.exists() {
            std::fs::copy(&path, &bak)?;
        }
        let tmp = path.with_extension("dat.nier_coop.tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &path)
    }
}

/// Steam IDs are 64-bit with this fixed high word for individual accounts (bytes 4..8 little-endian).
const STEAM_ID_HIGH: [u8; 4] = [0x01, 0x00, 0x10, 0x01];

/// This player's SteamID64, from the first 8 bytes of their own GameData.dat (the game writes it there).
fn own_steam_id(dir: &std::path::Path) -> Option<[u8; 8]> {
    let g = std::fs::read(dir.join("GameData.dat")).ok()?;
    let id: [u8; 8] = g.get(..8)?.try_into().ok()?;
    (id[4..] == STEAM_ID_HIGH).then_some(id)
}

/// Each slot stores its owner's SteamID64 at byte 4 (seen in the Steam saves); the game shows a slot with
/// another player's ID as empty, so the host's save gets this player's ID.
fn set_owner(data: &mut [u8], id: [u8; 8]) {
    if data.get(8..12) == Some(&STEAM_ID_HIGH[..]) {
        data[4..12].copy_from_slice(&id);
    }
}

fn slot_name(slot: u32) -> String {
    format!("SlotData_{slot}.dat")
}

/// Documents\My Games\NieR_Automata, through the shell so a OneDrive-moved Documents folder is found.
pub(super) fn save_dir() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Documents, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    let docs = unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Documents, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s?
    };
    let dir = PathBuf::from(docs).join(r"My Games\NieR_Automata");
    dir.is_dir().then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(slot: u32, data: &[u8]) -> Vec<Vec<u8>> {
        let crc = crc32(data);
        (0..data.len())
            .step_by(CHUNK)
            .map(|off| {
                let mut p = Vec::new();
                for v in [CHUNK_MAGIC, slot, crc, data.len() as u32, off as u32] {
                    p.extend_from_slice(&v.to_le_bytes());
                }
                p.extend_from_slice(&data[off..(off + CHUNK).min(data.len())]);
                p
            })
            .collect()
    }

    /// Host and client over localhost UDP with temp save folders: the file arrives, is acked, and a backup is kept.
    #[test]
    fn host_to_client_over_udp() {
        let root = std::env::temp_dir().join(format!("nier_coop_save_{}", std::process::id()));
        let (hd, cd) = (root.join("host"), root.join("client"));
        std::fs::create_dir_all(&hd).unwrap();
        std::fs::create_dir_all(&cd).unwrap();
        let data: Vec<u8> = (0..235_980u32).map(|i| (i * 13) as u8).collect();
        std::fs::write(hd.join("SlotData_1.dat"), &data).unwrap();
        std::fs::write(cd.join("SlotData_1.dat"), b"friend's own save").unwrap();
        std::thread::sleep(Duration::from_millis(1100)); // the host skips files written under a second ago

        let new = |host, dir| SaveSync { host, dir, out: None, inc: None, written: None, checked: Instant::now() - Duration::from_secs(5), sent: Instant::now() };
        let (mut h, mut c) = (new(true, hd), new(false, cd.clone()));
        let hs = UdpSocket::bind("127.0.0.1:0").unwrap();
        let cs = UdpSocket::bind("127.0.0.1:0").unwrap();
        for s in [&hs, &cs] {
            s.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
        }
        let (ha, ca) = (hs.local_addr().unwrap(), cs.local_addr().unwrap());
        let mut buf = [0u8; 2048];
        let start = Instant::now();
        while !h.out.as_ref().is_some_and(|o| o.acked) {
            assert!(start.elapsed() < Duration::from_secs(10), "transfer timed out");
            h.poll(&hs, ca).unwrap();
            if let Ok((n, _)) = cs.recv_from(&mut buf) {
                assert!(c.on_packet(&buf[..n], &cs, ha).unwrap());
            }
            if let Ok((n, _)) = hs.recv_from(&mut buf) {
                assert!(h.on_packet(&buf[..n], &hs, ca).unwrap());
            }
        }
        assert_eq!(std::fs::read(cd.join("SlotData_1.dat")).unwrap(), data);
        assert_eq!(std::fs::read(cd.join("SlotData_1.dat.nier_coop.bak")).unwrap(), b"friend's own save");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn owner_id_swapped() {
        let host = [0, 0, 0, 0, 0x68, 0x8F, 0x66, 0x5B, 1, 0, 0x10, 1, 0x54, 0x59];
        let mine = [0x11, 0x22, 0x33, 0x44, 1, 0, 0x10, 1];
        let mut d = host.to_vec();
        set_owner(&mut d, mine);
        assert_eq!(d, [0, 0, 0, 0, 0x11, 0x22, 0x33, 0x44, 1, 0, 0x10, 1, 0x54, 0x59]);
        let mut odd = vec![0u8; 14]; // no Steam ID where expected: left alone
        set_owner(&mut odd, mine);
        assert_eq!(odd, vec![0u8; 14]);
    }

    #[test]
    fn assembles_out_of_order_and_rejects_bad() {
        let data: Vec<u8> = (0..235_980u32).map(|i| (i * 7) as u8).collect();
        let mut s = SaveSync { host: false, dir: PathBuf::new(), out: None, inc: None, written: None, checked: Instant::now(), sent: Instant::now() };
        let mut c = chunks(1, &data);
        c.reverse();
        let dup = c[3].clone();
        let last = c.pop().unwrap();
        for p in &c {
            assert_eq!(s.receive(p), None);
        }
        assert_eq!(s.receive(&dup), None); // duplicate counts once
        assert_eq!(s.receive(&last), Some((1, data.clone())));

        // Bad slot, short body, oversize, misaligned offset.
        let mut bad = chunks(5, &data)[0].clone();
        assert_eq!(s.receive(&bad), None);
        bad = chunks(0, &data)[0][..100].to_vec();
        assert_eq!(s.receive(&bad), None);
        bad = chunks(0, &data)[0].clone();
        bad[12..16].copy_from_slice(&(SAVE_SIZE as u32 + 1).to_le_bytes());
        assert_eq!(s.receive(&bad), None);
        bad = chunks(0, &data)[1].clone();
        bad[16..20].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(s.receive(&bad), None);
        // Not save-sized: never assembled, even when complete and CRC-correct.
        assert_eq!(chunks(0, &data[..3000]).iter().filter_map(|p| s.receive(p)).count(), 0);

        // Corrupted data: every chunk arrives but the CRC fails.
        let mut c = chunks(0, &data);
        c[1][HEAD] ^= 1;
        assert_eq!(c.iter().filter_map(|p| s.receive(p)).count(), 0);
    }

    /// Quest sync groundwork, run with the game in-game: where does the running game keep the newest save in memory?
    /// Samples 32-byte windows of SlotData_N.dat, finds them in the game's memory, and prints the biggest runs as
    /// file offset range -> address (exe-relative when inside the exe). Quest flags live somewhere in that image.
    /// `cargo test --release --target x86_64-pc-windows-msvc map_save_in_running_game -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn map_save_in_running_game() {
        use std::collections::HashMap;
        use windows::Win32::Foundation::{CloseHandle, HMODULE};
        use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
        use windows::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQueryEx};
        use windows::Win32::System::ProcessStatus::EnumProcessModules;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};

        let dir = save_dir().expect("save folder");
        let (path, data) = (0..3)
            .filter_map(|n| {
                let p = dir.join(slot_name(n));
                Some((std::fs::metadata(&p).ok()?.modified().ok()?, p))
            })
            .max()
            .map(|(_, p)| (p.clone(), std::fs::read(&p).unwrap()))
            .expect("no SlotData_N.dat");
        println!("{} ({} bytes)", path.display(), data.len());
        // Windows with >= 12 distinct bytes, keyed by their first 8 bytes.
        const W: usize = 32;
        let mut keys: HashMap<u64, Vec<usize>> = HashMap::new();
        for off in (0..data.len().saturating_sub(W)).step_by(16) {
            let w = &data[off..off + W];
            if w.iter().collect::<std::collections::HashSet<_>>().len() >= 12 {
                keys.entry(u64::from_le_bytes(w[..8].try_into().unwrap())).or_default().push(off);
            }
        }
        println!("{} sample windows", keys.values().map(Vec::len).sum::<usize>());

        let out = std::process::Command::new("tasklist").args(["/FI", "IMAGENAME eq NieRAutomata.exe", "/FO", "CSV", "/NH"]).output().unwrap();
        let pid: u32 = String::from_utf8_lossy(&out.stdout).split(',').nth(1).expect("game not running").trim_matches('"').parse().unwrap();
        let h = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) }.unwrap();
        let mut m = [HMODULE::default(); 1];
        let mut need = 0;
        unsafe { EnumProcessModules(h, m.as_mut_ptr(), size_of_val(&m) as u32, &mut need) }.unwrap();
        let exe = m[0].0 as usize;

        // delta = address - file offset; one save image in memory = many hits with the same delta.
        let mut hits: HashMap<isize, Vec<usize>> = HashMap::new();
        let (mut addr, mut buf) = (0usize, Vec::new());
        loop {
            let mut mi = MEMORY_BASIC_INFORMATION::default();
            if unsafe { VirtualQueryEx(h, Some(addr as _), &mut mi, size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
                break;
            }
            let (base, size) = (mi.BaseAddress as usize, mi.RegionSize);
            addr = base + size;
            if mi.State != MEM_COMMIT || (mi.Protect & (PAGE_NOACCESS | PAGE_GUARD)).0 != 0 || size > 1 << 30 {
                continue;
            }
            buf.resize(size, 0);
            let mut got = 0;
            if unsafe { ReadProcessMemory(h, base as _, buf.as_mut_ptr().cast(), size, Some(&mut got)) }.is_err() {
                continue;
            }
            for i in (0..got.saturating_sub(W)).step_by(4) {
                let Some(offs) = keys.get(&u64::from_le_bytes(buf[i..i + 8].try_into().unwrap())) else { continue };
                for &off in offs {
                    if buf[i..i + W] == data[off..off + W] {
                        hits.entry((base + i) as isize - off as isize).or_default().push(off);
                    }
                }
            }
        }
        unsafe { CloseHandle(h) }.unwrap();
        let mut runs: Vec<_> = hits.into_iter().filter(|(_, v)| v.len() >= 8).collect();
        runs.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        for (delta, offs) in runs.iter().take(10) {
            let (lo, hi) = (*offs.iter().min().unwrap(), *offs.iter().max().unwrap() + W);
            let a = (*delta + lo as isize) as usize;
            let at = if a >= exe && a < exe + 0x1000_0000 { format!("exe+{:#x}", a - exe) } else { format!("{a:#x} (heap)") };
            println!("file {lo:#x}..{hi:#x} ({} windows) at {at}", offs.len());
        }
    }
}
