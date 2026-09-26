//! Files on the clipboard the way Explorer puts them there.

use std::io;

/// Places `paths` on the clipboard as `CF_HDROP`, plus a
/// `Preferred DropEffect` of move (`cut`) or copy, so pasting into Explorer
/// or another app copies or moves the real files rather than pasting their
/// path as text.
#[cfg(windows)]
pub fn put_files(paths: &[String], cut: bool) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, POINT};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::CF_HDROP;
    use windows_sys::Win32::UI::Shell::DROPFILES;

    const DROPEFFECT_COPY: u32 = 1;
    const DROPEFFECT_MOVE: u32 = 2;
    const DROPEFFECT_LINK: u32 = 4;

    // DROPFILES is followed by the paths as NUL-terminated UTF-16 strings,
    // the list itself terminated by one more NUL.
    let mut list: Vec<u16> = Vec::new();
    for path in paths {
        list.extend(std::ffi::OsStr::new(path).encode_wide());
        list.push(0);
    }
    list.push(0);

    let header = std::mem::size_of::<DROPFILES>();
    let list_bytes = list.len() * std::mem::size_of::<u16>();

    /// Allocates a movable global block and fills it via `write`. Ownership
    /// passes to the clipboard only once `SetClipboardData` succeeds.
    unsafe fn global_block(size: usize, write: impl FnOnce(*mut u8)) -> io::Result<HANDLE> {
        let block = GlobalAlloc(GMEM_MOVEABLE, size);
        if block.is_null() {
            return Err(io::Error::last_os_error());
        }
        let ptr = GlobalLock(block) as *mut u8;
        if ptr.is_null() {
            let err = io::Error::last_os_error();
            GlobalFree(block);
            return Err(err);
        }
        std::ptr::write_bytes(ptr, 0, size);
        write(ptr);
        GlobalUnlock(block);
        Ok(block)
    }

    // SAFETY: every pointer written through comes from a block of exactly
    // `size` bytes that this function just allocated and locked.
    unsafe {
        let drop_block = global_block(header + list_bytes, |ptr| {
            let files = ptr as *mut DROPFILES;
            (*files).pFiles = header as u32;
            (*files).pt = POINT { x: 0, y: 0 };
            (*files).fNC = 0;
            (*files).fWide = 1;
            std::ptr::copy_nonoverlapping(list.as_ptr() as *const u8, ptr.add(header), list_bytes);
        })?;

        let effect = if cut {
            DROPEFFECT_MOVE
        } else {
            DROPEFFECT_COPY | DROPEFFECT_LINK
        };
        let effect_block = match global_block(std::mem::size_of::<u32>(), |ptr| {
            (ptr as *mut u32).write_unaligned(effect);
        }) {
            Ok(block) => block,
            Err(e) => {
                GlobalFree(drop_block);
                return Err(e);
            }
        };

        // Another app can hold the clipboard for a moment (clipboard
        // managers, RDP); retry briefly instead of failing the first time.
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !opened {
            let err = io::Error::last_os_error();
            GlobalFree(drop_block);
            GlobalFree(effect_block);
            return Err(err);
        }

        EmptyClipboard();
        let result = if SetClipboardData(CF_HDROP as u32, drop_block).is_null() {
            let err = io::Error::last_os_error();
            GlobalFree(drop_block);
            GlobalFree(effect_block);
            Err(err)
        } else {
            let name: Vec<u16> = "Preferred DropEffect\0".encode_utf16().collect();
            let format = RegisterClipboardFormatW(name.as_ptr());
            if format == 0 || SetClipboardData(format, effect_block).is_null() {
                // The files are on the clipboard; only the copy/move hint
                // is missing, which paste targets treat as a copy.
                GlobalFree(effect_block);
            }
            Ok(())
        };
        CloseClipboard();
        result
    }
}

#[cfg(not(windows))]
pub fn put_files(_paths: &[String], _cut: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file clipboard is only implemented on Windows",
    ))
}
