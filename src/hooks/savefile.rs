//! Save sync, client side: the game lists the save slots once, at startup, so a save that arrives later only shows
//! after a restart. This hooks CreateFileA/W (the game opens files with CreateFileA) and holds the game's first open
//! of a `SlotData_N.dat` until the host's save has been written (or `WAIT` passes), so it reads the host's save.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use retour::static_detour;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::core::{PCSTR, s, w};

/// Set by the net thread once the host's save is on disk (written or already identical).
pub static SAVE_READY: AtomicBool = AtomicBool::new(false);
/// Long enough to connect and receive a save (~1 s) when the host's game is already running.
const WAIT: Duration = Duration::from_secs(20);
static WAITED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The net thread reads and writes the save files itself: never hold it (it would wait on itself).
    pub static NO_WAIT: Cell<bool> = const { Cell::new(false) };
}

type OpenW = unsafe extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, *mut c_void) -> *mut c_void;
type OpenA = unsafe extern "system" fn(*const u8, u32, u32, *const c_void, u32, u32, *mut c_void) -> *mut c_void;

static_detour! {
    static CreateFileW: unsafe extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, *mut c_void) -> *mut c_void;
    static CreateFileA: unsafe extern "system" fn(*const u8, u32, u32, *const c_void, u32, u32, *mut c_void) -> *mut c_void;
}

pub fn install() -> Result<(), String> {
    // KernelBase holds the real functions; kernel32's exports jump there.
    unsafe {
        let m = GetModuleHandleW(w!("kernelbase.dll")).map_err(|e| format!("kernelbase: {e}"))?;
        let a = GetProcAddress(m, s!("CreateFileA")).ok_or("CreateFileA not found")?;
        let wf = GetProcAddress(m, s!("CreateFileW")).ok_or("CreateFileW not found")?;
        CreateFileA.initialize(std::mem::transmute::<_, OpenA>(a), open_a).and_then(|d| d.enable()).map_err(|e| e.to_string())?;
        CreateFileW.initialize(std::mem::transmute::<_, OpenW>(wf), open_w).and_then(|d| d.enable()).map_err(|e| e.to_string())?;
    }
    tracing::info!("save sync: holding the game's first save read until the host's save arrives (up to {}s)", WAIT.as_secs());
    Ok(())
}

fn open_a(name: *const u8, access: u32, share: u32, sa: *const c_void, disp: u32, flags: u32, tmpl: *mut c_void) -> *mut c_void {
    if !NO_WAIT.with(Cell::get) && !WAITED.load(Relaxed) && !name.is_null() {
        before_open(&unsafe { PCSTR(name).to_string() }.unwrap_or_default());
    }
    unsafe { CreateFileA.call(name, access, share, sa, disp, flags, tmpl) }
}

fn open_w(name: *const u16, access: u32, share: u32, sa: *const c_void, disp: u32, flags: u32, tmpl: *mut c_void) -> *mut c_void {
    if !NO_WAIT.with(Cell::get) && !WAITED.load(Relaxed) && !name.is_null() {
        before_open(&unsafe { windows::core::PCWSTR(name).to_string() }.unwrap_or_default());
    }
    unsafe { CreateFileW.call(name, access, share, sa, disp, flags, tmpl) }
}

/// Seen in-game (Steam build): SystemData.dat at ~0.6 s, then after the logos (~20 s) GameData.dat and SlotData_0..3.
fn before_open(path: &str) {
    if WAITED.load(Relaxed) || !slot_name(path) || WAITED.swap(true, Relaxed) {
        return;
    }
    let start = Instant::now();
    tracing::info!("save sync: game is reading saves, waiting for the host's save...");
    while !SAVE_READY.load(Relaxed) && start.elapsed() < WAIT {
        std::thread::sleep(Duration::from_millis(50));
    }
    match SAVE_READY.load(Relaxed) {
        true => tracing::info!("save sync: host's save is in place, the game reads it now"),
        false => tracing::warn!("save sync: no save from the host in {}s (host's game not open?); this save list stays as it was", WAIT.as_secs()),
    }
}

/// The path ends in `SlotData_<digit>.dat` (any case).
fn slot_name(path: &str) -> bool {
    let file = path.rsplit(['\\', '/']).next().unwrap_or("").to_ascii_lowercase();
    file.len() == 14 && file.starts_with("slotdata_") && file.ends_with(".dat") && file.as_bytes()[9].is_ascii_digit()
}

#[cfg(test)]
mod tests {
    #[test]
    fn slot_names() {
        assert!(super::slot_name(r"C:\Users\x\Documents\My Games\NieR_Automata\SlotData_1.dat"));
        assert!(super::slot_name("slotdata_0.DAT"));
        assert!(!super::slot_name(r"C:\x\SlotData_1.dat.nier_coop.bak"));
        assert!(!super::slot_name(r"C:\x\GameData.dat"));
        assert!(!super::slot_name(r"C:\x\SlotData_12.dat"));
    }
}
