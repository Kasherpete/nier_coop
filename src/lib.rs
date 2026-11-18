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

fn worker() {
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

    if let Err(e) = hooks::install() {
        tracing::error!("hooks not installed: {e}");
        return;
    }
    match cfg {
        Some(cfg) => net::spawn(cfg),
        None => tracing::error!("nier_coop.cfg missing or invalid (needs bind=IP:PORT and peer=IP:PORT)"),
    }
}
