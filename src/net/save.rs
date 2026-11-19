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
/// Saves are 235,980 bytes (Steam build); anything far bigger is not a save.
const MAX_SIZE: usize = 1 << 20;
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
        if data.is_empty() || data.len() > MAX_SIZE {
            return;
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
                    match self.write(slot, &data) {
                        Ok(()) => {
                            self.written = Some(crc32(&data));
                            tracing::info!("save sync: host's save written to slot {}; load it from the title menu", slot + 1);
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
        if slot >= SLOTS || total == 0 || total > MAX_SIZE || off % CHUNK != 0 || off >= total || body.len() != CHUNK.min(total - off)
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

fn slot_name(slot: u32) -> String {
    format!("SlotData_{slot}.dat")
}

/// Documents\My Games\NieR_Automata, through the shell so a OneDrive-moved Documents folder is found.
fn save_dir() -> Option<PathBuf> {
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
        bad[12..16].copy_from_slice(&(MAX_SIZE as u32 + 1).to_le_bytes());
        assert_eq!(s.receive(&bad), None);
        bad = chunks(0, &data)[1].clone();
        bad[16..20].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(s.receive(&bad), None);

        // Corrupted data: every chunk arrives but the CRC fails.
        let mut c = chunks(0, &data[..3000]);
        c[1][HEAD] ^= 1;
        assert_eq!(c.iter().filter_map(|p| s.receive(p)).count(), 0);
    }
}
