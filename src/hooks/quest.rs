//! Quest sync (`world_sync=true`, one player `host=true`): the host's scene states ("q120/DONE", "q410/REQ",
//! "Route/A", ...: the story and quest progress the save keeps under SceneState) are copied to the client through
//! the game's own SceneStateSystem.set / .reset, the functions its scripts call. Game thread only.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use super::spawn::script_fn;
use crate::scan;
use crate::sdk::{HashedStr, SceneStateSystem, patterns};
use crate::state::{self, HOST, LOCAL_SCENE, REMOTE_ACTIVE, REMOTE_SCENE, WORLD_SYNC};

/// Absolute addresses, set once by `install`.
#[derive(Debug)]
struct Addr {
    sys: usize,     // static SceneStateSystem
    pool: usize,    // global string pool
    hash: usize,    // u32 hash(const char*, len)
    intern: usize,  // HashedStr* intern(pool, hash, const char*, len), +1 ref
    release: usize, // release(HashedStr**)
    set: usize,     // bool set(sys, HashedStr**)
    reset: usize,   // bool reset(sys, HashedStr**)
}
static ADDR: OnceLock<Addr> = OnceLock::new();
static FRAMES: AtomicU32 = AtomicU32::new(0);

/// States that describe this machine or this player's character, not the shared story.
const LOCAL_ONLY: [&str; 5] = ["Buddy/", "PL/", "DLC_", "playgo", "auto/"]; // auto/blocked: autosave lock
const MAX_STATES: usize = 4096;
const MAX_NAME: usize = 64;

/// Module offsets around a `set(&sys, &intern(...))` site: (sys, pool, hash, intern, release).
fn resolve(m: &[u8], set: usize) -> Result<[usize; 5], String> {
    let p = scan::parse(patterns::SCENE_SET_SITE).ok_or("SCENE_SET_SITE: invalid pattern")?;
    let call = |at: usize| scan::rip_target(m, at, 1, 5);
    let lea = |at: usize| scan::rip_target(m, at, 3, 7);
    let at = (0..m.len())
        .find(|&i| scan::matches_at(m, i, &p) && call(i + 0x2F) == Some(set))
        .ok_or("SCENE_SET_SITE: no set call site")?;
    let bad = || "SCENE_SET_SITE: bad rip target".to_string();
    Ok([lea(at + 0x28).ok_or_else(bad)?, lea(at + 0x14).ok_or_else(bad)?, call(at).ok_or_else(bad)?, call(at + 0x1B).ok_or_else(bad)?, call(at + 0x39).ok_or_else(bad)?])
}

pub fn install(m: &[u8], base: usize) -> Result<(), String> {
    let (set, reset) = (script_fn("SceneStateSystem.set").ok_or("no set")?, script_fn("SceneStateSystem.reset").ok_or("no reset")?);
    let [sys, pool, hash, intern, release] = resolve(m, set - base)?;
    let a = Addr { sys: base + sys, pool: base + pool, hash: base + hash, intern: base + intern, release: base + release, set, reset };
    tracing::info!("quest sync: {a:x?}");
    let _ = ADDR.set(a);
    Ok(())
}

pub fn tick() {
    if !WORLD_SYNC.load(Relaxed) || !REMOTE_ACTIVE.load(Relaxed) || FRAMES.fetch_add(1, Relaxed) % 60 != 0 {
        return;
    }
    let Some(a) = ADDR.get() else { return };
    let ours = states(a.sys as *const SceneStateSystem);
    if HOST.load(Relaxed) {
        *state::lock(&LOCAL_SCENE) = ours.join("\n").into_bytes();
        return;
    }
    let Some(theirs) = state::lock(&REMOTE_SCENE).take() else { return };
    let theirs: Vec<&str> = std::str::from_utf8(&theirs).unwrap_or("").split('\n').filter(|n| valid(n)).collect();
    // An empty list is a host between saves (title screen, loading), never "erase all progress".
    if theirs.is_empty() || ours.is_empty() {
        return;
    }
    let ours: Vec<&str> = ours.iter().map(String::as_str).collect();
    let (add, del) = diff(&ours, &theirs);
    for n in add {
        if call(a, a.set, n) {
            tracing::info!("quest sync: + {n}");
        }
    }
    let mut missing = state::lock(&MISSING);
    let partial = theirs.len() * 2 < ours.len();
    if partial && !del.is_empty() {
        tracing::debug!("quest sync: host list has {} states, ours {}; not resetting from it", theirs.len(), ours.len());
    }
    for n in debounce(missing.get_or_insert_with(Default::default), &del, partial) {
        if call(a, a.reset, n) {
            tracing::info!("quest sync: - {n}");
        }
    }
}

/// Lists in a row a state must be missing on the host before it is reset here.
const RESET_AFTER: u32 = 3;
/// Per state we have and the host lacks: host lists in a row without it.
static MISSING: std::sync::Mutex<Option<HashMap<String, u32>>> = std::sync::Mutex::new(None);

/// The states to reset now: missing from the host's list `RESET_AFTER` times in a row. A state is set here a
/// moment before the host's next list shows it (a cutscene the client played), and a host list taken mid-load
/// can be short; resetting from one list made such states flap. A `partial` list counts toward nothing.
fn debounce<'a>(missing: &mut HashMap<String, u32>, del: &[&'a str], partial: bool) -> Vec<&'a str> {
    missing.retain(|n, _| del.contains(&n.as_str()));
    if partial {
        return Vec::new();
    }
    let mut out = Vec::new();
    for &n in del {
        let c = missing.entry(n.to_string()).or_insert(0);
        *c += 1;
        if *c >= RESET_AFTER {
            missing.remove(n);
            out.push(n);
        }
    }
    out
}

/// Shared states the host has and we lack (to set), and that we have and the host does not (to reset).
fn diff<'a>(ours: &[&'a str], theirs: &[&'a str]) -> (Vec<&'a str>, Vec<&'a str>) {
    let shared = |n: &&str| !LOCAL_ONLY.iter().any(|p| n.starts_with(p));
    let (o, t): (HashSet<&str>, HashSet<&str>) = (ours.iter().copied().filter(shared).collect(), theirs.iter().copied().filter(shared).collect());
    let mut add: Vec<_> = t.difference(&o).copied().collect();
    let mut del: Vec<_> = o.difference(&t).copied().collect();
    add.sort_unstable();
    del.sort_unstable();
    (add, del)
}

/// Peer data: short printable ASCII only.
fn valid(n: &str) -> bool {
    !n.is_empty() && n.len() <= MAX_NAME && n.bytes().all(|c| c.is_ascii_graphic())
}

fn states(sys: *const SceneStateSystem) -> Vec<String> {
    let Some(s) = (unsafe { sys.as_ref() }) else { return Vec::new() };
    if s.states.is_null() || s.count > MAX_STATES {
        return Vec::new();
    }
    (0..s.count)
        .filter_map(|i| {
            let e = unsafe { s.states.add(i).read() };
            let h = unsafe { e.name.as_ref() }?;
            text(h.text)
        })
        .collect()
}

pub(super) fn text(p: *const u8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut v = Vec::new();
    for i in 0..=MAX_NAME {
        match unsafe { p.add(i).read() } {
            0 => break,
            c => v.push(c),
        }
    }
    String::from_utf8(v).ok().filter(|n| valid(n))
}

/// Drops one reference to an engine string (no-op before `install`).
pub(super) fn release(s: &mut *mut HashedStr) {
    if let Some(a) = ADDR.get() {
        unsafe { std::mem::transmute::<usize, extern "system" fn(*mut *mut HashedStr)>(a.release)(s) };
    }
}

/// `f(&sys, &name)` with `name` interned the way the game's own call sites do it.
fn call(a: &Addr, f: usize, name: &str) -> bool {
    type Hash = extern "system" fn(*const u8, usize) -> u32;
    type Intern = extern "system" fn(usize, u32, *const u8, usize) -> *mut HashedStr;
    type Release = extern "system" fn(*mut *mut HashedStr);
    type SetFn = extern "system" fn(usize, *const *mut HashedStr) -> bool;
    let z: Vec<u8> = name.bytes().chain([0]).collect(); // intern copies len + 1 bytes
    unsafe {
        let (hash, intern) = (std::mem::transmute::<usize, Hash>(a.hash), std::mem::transmute::<usize, Intern>(a.intern));
        let (release, f) = (std::mem::transmute::<usize, Release>(a.release), std::mem::transmute::<usize, SetFn>(f));
        let mut s = intern(a.pool, hash(z.as_ptr(), name.len()), z.as_ptr(), name.len());
        if s.is_null() {
            return false;
        }
        let ok = f(a.sys, &s);
        release(&mut s);
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_skips_local_states() {
        let ours = ["q120/RUN_1", "Buddy/2B", "PL/2B", "Route/A", "auto/blocked"];
        let theirs = ["q120/DONE", "Buddy/Auto", "Route/A", "DLC_installed"];
        assert_eq!(diff(&ours, &theirs), (vec!["q120/DONE"], vec!["q120/RUN_1"]));
        assert!(valid("q770_FIN") && !valid("") && !valid("a b") && !valid(&"x".repeat(65)));
    }

    #[test]
    fn resets_only_after_repeated_absence() {
        let mut m = HashMap::new();
        assert!(debounce(&mut m, &["q1", "q2"], false).is_empty());
        assert!(debounce(&mut m, &["q1"], false).is_empty()); // q2 came back on the host: its count restarts
        assert!(debounce(&mut m, &["q1", "q2"], true).is_empty()); // short host list: counts nothing
        assert_eq!(debounce(&mut m, &["q1", "q2"], false), vec!["q1"]); // third full list in a row without q1
        assert!(debounce(&mut m, &["q2"], false).is_empty());
        assert_eq!(debounce(&mut m, &["q2"], false), vec!["q2"]);
    }

    /// `NIER_EXE=<exe> cargo test quest -- --ignored --nocapture`; offsets from the exe of 2026-11-19.
    #[test]
    #[ignore]
    fn resolves_against_installed_exe() {
        let img = super::super::tests::map_image(&std::fs::read(std::env::var("NIER_EXE").unwrap()).unwrap());
        assert_eq!(resolve(&img, 0x354b20), Ok([0xfc2680, 0xf8c4e0, 0x28ee30, 0x150e70, 0x150c60]));
    }
}
