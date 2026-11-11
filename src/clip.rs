//! Write text to the Windows clipboard as CF_UNICODETEXT. Best effort: a
//! failure (another process holds the clipboard, say) is reported, not fatal.

use std::ptr::null_mut;

use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

const CF_UNICODETEXT: u32 = 13;

/// Copy `text` to the system clipboard. Returns true on success.
pub fn set(text: &str) -> bool {
    // UTF-16, NUL-terminated, as CF_UNICODETEXT wants.
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;
    unsafe {
        if OpenClipboard(null_mut()) == 0 {
            return false;
        }
        // One owner at a time: everything here runs between Open and Close.
        let ok = (|| {
            if EmptyClipboard() == 0 {
                return false;
            }
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if h.is_null() {
                return false;
            }
            let dst = GlobalLock(h);
            if dst.is_null() {
                return false; // GlobalAlloc block is reclaimed when the process ends
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst as *mut u16, wide.len());
            GlobalUnlock(h);
            // On success the clipboard owns `h`; we must not free it.
            !SetClipboardData(CF_UNICODETEXT, h as _).is_null()
        })();
        CloseClipboard();
        ok
    }
}
