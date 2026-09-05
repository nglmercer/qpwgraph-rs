//! Small platform bridge for support diagnostics.

/// Copy text to the native clipboard without adding a third-party clipboard
/// runtime dependency to the desktop application.
#[cfg(target_os = "windows")]
pub(crate) fn copy_text(text: &str) -> Result<(), String> {
    use std::ptr;
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();
    unsafe { OpenClipboard(None) }
        .map_err(|error| format!("could not open the Windows clipboard: {error}"))?;

    let result = (|| {
        unsafe { EmptyClipboard() }
            .map_err(|error| format!("could not clear the Windows clipboard: {error}"))?;
        let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }
            .map_err(|error| format!("could not allocate clipboard memory: {error}"))?;
        let locked = unsafe { GlobalLock(memory) };
        if locked.is_null() {
            let _ = unsafe { GlobalFree(Some(memory)) };
            return Err("could not lock clipboard memory".to_owned());
        }
        // SAFETY: `locked` points to the `bytes` bytes returned by GlobalAlloc
        // and `wide` is exactly that size, including its terminating NUL.
        unsafe {
            ptr::copy_nonoverlapping(wide.as_ptr().cast::<u8>(), locked.cast::<u8>(), bytes);
        }
        if let Err(error) = unsafe { GlobalUnlock(memory) } {
            let _ = unsafe { GlobalFree(Some(memory)) };
            return Err(format!("could not unlock clipboard memory: {error}"));
        }
        if let Err(error) =
            unsafe { SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(memory.0))) }
        {
            let _ = unsafe { GlobalFree(Some(memory)) };
            return Err(format!("could not set Windows clipboard data: {error}"));
        }
        // Windows owns the movable block after SetClipboardData succeeds.
        Ok(())
    })();
    let close = unsafe { CloseClipboard() };
    result.and(close.map_err(|error| format!("could not close the Windows clipboard: {error}")))
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn copy_text(_text: &str) -> Result<(), String> {
    Err("Windows audio diagnostics are available only on Windows".into())
}
