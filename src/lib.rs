use std::ffi::c_void;

use tracing_subscriber::fmt::writer::MakeWriterExt;

use windows::Win32::Foundation::{BOOL, HMODULE, TRUE};
use windows::Win32::System::Console::{AllocConsole, FreeConsole};
use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};

mod hooks;
mod net;
mod scan;
mod sdk;
mod state;

#[unsafe(no_mangle)]
extern "system" fn DllMain(module: HMODULE, reason: u32, reserved: *mut c_void) -> BOOL {
    match reason {
        DLL_PROCESS_ATTACH => {
            unsafe {
                let _ = DisableThreadLibraryCalls(module);
            }
            // Loader lock is held here: do no real work, just hand off to a thread.
            std::thread::spawn(worker);
        }
        // Non-null `reserved` means the process is exiting; other threads are already dead, so touch nothing.
        // shortcut: detours stay installed, so FreeLibrary while the game runs will crash; unload is unsupported.
        DLL_PROCESS_DETACH if reserved.is_null() => unsafe {
            let _ = FreeConsole();
        },
        _ => {}
    }
    TRUE
}

/// Proxy entry: shipped as `dinput8.dll` in the game folder, the game loads this DLL at startup (no injector).
/// Forwards to the real System32 dinput8.dll, loaded on first call (never inside DllMain).
#[unsafe(no_mangle)]
extern "system" fn DirectInput8Create(inst: HMODULE, ver: u32, riid: *const c_void, out: *mut *mut c_void, outer: *mut c_void) -> i32 {
    type F = extern "system" fn(HMODULE, u32, *const c_void, *mut *mut c_void, *mut c_void) -> i32;
    static REAL: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let real = REAL.get_or_init(|| unsafe {
        use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        // Full path: by bare name the loader returns this DLL (already loaded as "dinput8.dll") and we call ourselves.
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let h = LoadLibraryW(&windows::core::HSTRING::from(format!(r"{root}\System32\dinput8.dll"))).ok()?;
        GetProcAddress(h, windows::core::s!("DirectInput8Create")).map(|f| f as usize)
    });
    match real {
        Some(f) => unsafe { std::mem::transmute::<usize, F>(*f)(inst, ver, riid, out, outer) },
        None => 0x8000_4005u32 as i32, // E_FAIL
    }
}

fn worker() {
    // Loaded twice (dinput8.dll proxy plus an injector)? Only the first copy runs, two would hook everything twice.
    let name = windows::core::HSTRING::from(format!("nier_coop_{}", std::process::id()));
    match unsafe { windows::Win32::System::Threading::CreateMutexW(None, true, &name) } {
        Ok(_) if unsafe { windows::Win32::Foundation::GetLastError() } != windows::Win32::Foundation::ERROR_ALREADY_EXISTS => {}
        _ => return,
    }
    unsafe {
        let _ = AllocConsole();
    }
    let cfg = net::load_config();
    let level = if cfg.as_ref().is_some_and(|c| c.log_anims) { tracing::Level::DEBUG } else { tracing::Level::INFO };
    // Console plus nier_coop.log next to the exe (truncated each run), so a session can be read back in full.
    let file = std::env::current_exe().ok().and_then(|p| std::fs::File::create(p.with_file_name("nier_coop.log")).ok());
    let sub = tracing_subscriber::fmt().with_ansi(false).with_max_level(level);
    match file {
        Some(f) => sub.with_writer(std::io::stdout.and(std::sync::Mutex::new(f))).init(),
        None => sub.init(),
    }
    tracing::info!("nier_coop loaded");
    hooks::SPAWN_2B.store(cfg.as_ref().is_some_and(|c| c.spawn_2b), std::sync::atomic::Ordering::Relaxed);
    hooks::ANIM_REPLAY.store(cfg.as_ref().is_some_and(|c| c.anim_replay), std::sync::atomic::Ordering::Relaxed);
    state::LOCKSTEP.store(cfg.as_ref().is_some_and(|c| c.lockstep), std::sync::atomic::Ordering::Relaxed);
    state::WORLD_SYNC.store(cfg.as_ref().is_some_and(|c| c.world_sync), std::sync::atomic::Ordering::Relaxed);
    state::HOST.store(cfg.as_ref().is_some_and(|c| c.host), std::sync::atomic::Ordering::Relaxed);

    if let Err(e) = hooks::install() {
        tracing::error!("hooks not installed: {e}");
        return;
    }
    match cfg {
        Some(cfg) => net::spawn(cfg),
        None => tracing::error!("nier_coop.cfg missing or invalid (needs bind=IP:PORT and peer=IP:PORT)"),
    }
}
