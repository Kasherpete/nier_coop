//! AoB pattern scanning over the game module (ASLR-safe address resolution).

use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::{GetModuleInformation, MODULEINFO};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::PCWSTR;

/// The main executable's mapped image.
pub fn module() -> Option<&'static [u8]> {
    unsafe {
        let h = GetModuleHandleW(PCWSTR::null()).ok()?;
        let mut info = MODULEINFO::default();
        GetModuleInformation(GetCurrentProcess(), h, &mut info, size_of::<MODULEINFO>() as u32).ok()?;
        let base = info.lpBaseOfDll as *const u8;
        // shortcut: assumes every page of the image is readable; switch to per-section scanning if a page faults.
        (!base.is_null()).then(|| std::slice::from_raw_parts(base, info.SizeOfImage as usize))
    }
}

pub fn parse(pat: &str) -> Option<Vec<Option<u8>>> {
    let v = pat
        .split_whitespace()
        .map(|t| match t {
            "?" | "??" => Some(None),
            _ => u8::from_str_radix(t, 16).ok().map(Some),
        })
        .collect::<Option<Vec<_>>>()?;
    (!v.is_empty()).then_some(v)
}

/// Offset of the only match. `None` on zero or multiple matches: a non-unique pattern is a wrong address.
pub fn find_unique(hay: &[u8], pat: &[Option<u8>]) -> Option<usize> {
    let mut hits = hay
        .windows(pat.len())
        .enumerate()
        .filter(|(_, w)| w.iter().zip(pat).all(|(b, p)| p.is_none_or(|p| p == *b)))
        .map(|(i, _)| i);
    let first = hits.next()?;
    hits.next().is_none().then_some(first)
}

/// Target offset of a RIP-relative operand: `at + len + disp32`, checked to stay inside `hay`.
pub fn rip_target(hay: &[u8], at: usize, disp_off: usize, len: usize) -> Option<usize> {
    let d = hay.get(at + disp_off..at + disp_off + 4)?;
    let disp = i32::from_le_bytes(d.try_into().ok()?) as isize;
    let t = (at + len).checked_add_signed(disp)?;
    (t < hay.len()).then_some(t)
}

/// Offset of the first rel32 operand (at `i`, instruction ending at `i + 4`) that points at `target`.
pub fn find_ref(hay: &[u8], target: usize) -> Option<usize> {
    (0..hay.len().saturating_sub(4)).find(|&i| rip_target(hay, i, 0, 4) == Some(target))
}

/// Offset of the closest match of `pat` that starts in `[end - max, end)`.
pub fn rfind(hay: &[u8], end: usize, max: usize, pat: &[Option<u8>]) -> Option<usize> {
    (end.saturating_sub(max)..end).rev().find(|&i| {
        hay.get(i..i + pat.len()).is_some_and(|w| w.iter().zip(pat).all(|(b, p)| p.is_none_or(|p| p == *b)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scan_and_rip() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("48 zz"), None);
        let pat = parse("48 8B 05 ?? ?? ?? ??").unwrap();
        // mov rax,[rip+0x10] at offset 2
        let hay = [0x90, 0x90, 0x48, 0x8B, 0x05, 0x10, 0, 0, 0, 0x90];
        let at = find_unique(&hay, &pat).unwrap();
        assert_eq!(at, 2);
        assert_eq!(rip_target(&[0u8; 0x40], 2, 3, 7), Some(9)); // disp 0
        let mut big = [0u8; 0x40];
        big[2..9].copy_from_slice(&hay[2..9]);
        assert_eq!(rip_target(&big, 2, 3, 7), Some(0x19));
        assert_eq!(rip_target(&hay, 2, 3, 7), None); // target past end
        assert_eq!(find_unique(&[0x48, 0x48], &parse("48").unwrap()), None); // not unique
        assert_eq!(find_ref(&big, 0x19), Some(5));
        let cc = [0xCC, 0xCC, 0xCC, 0x48, 0xCC, 0xCC, 0xCC, 0x90, 0x90];
        assert_eq!(rfind(&cc, 8, 0x100, &parse("CC CC CC").unwrap()), Some(4));
        assert_eq!(rfind(&cc, 8, 2, &parse("CC CC CC").unwrap()), None);
    }
}
