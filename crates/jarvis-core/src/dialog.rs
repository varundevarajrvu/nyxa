//! Non-voice confirmation dialog for DENY-BY-DEFAULT actions (DESIGN §3/§5).
//! A destructive/irreversible action must be approved with a mouse/keyboard
//! click on a real modal dialog — a spoken "yes" is deliberately NOT enough
//! (ambient audio can't click). The dialog renders the fully-resolved action
//! and times out to DENY.

/// Show a modal Yes/No dialog and return true only on an explicit Yes click.
/// Default button is No (Enter = deny); a 30 s timeout also denies. The dialog
/// is system-modal + foreground so it can't be missed. Blocks the caller until
/// answered or timed out.
#[cfg(windows)]
pub fn confirm(title: &str, body: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;

    // MessageBoxTimeoutW is a stable (if undocumented) user32 export — gives us
    // a real modal with a deny-on-timeout, without spawning a UI thread.
    extern "system" {
        fn MessageBoxTimeoutW(
            hwnd: *mut core::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            utype: u32,
            language_id: u16,
            timeout_ms: u32,
        ) -> i32;
    }

    let wide = |s: &str| {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let body_w = wide(body);
    let title_w = wide(title);

    const MB_YESNO: u32 = 0x0000_0004;
    const MB_ICONWARNING: u32 = 0x0000_0030;
    const MB_DEFBUTTON2: u32 = 0x0000_0100; // default = No
    const MB_SYSTEMMODAL: u32 = 0x0000_1000; // topmost, blocks other input
    const MB_SETFOREGROUND: u32 = 0x0001_0000;
    const IDYES: i32 = 6;

    let ret = unsafe {
        MessageBoxTimeoutW(
            std::ptr::null_mut(),
            body_w.as_ptr(),
            title_w.as_ptr(),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SYSTEMMODAL | MB_SETFOREGROUND,
            0,
            30_000,
        )
    };
    // Anything that isn't an explicit Yes (No, closed, or timeout=32000) denies.
    ret == IDYES
}

#[cfg(not(windows))]
pub fn confirm(_title: &str, _body: &str) -> bool {
    false
}
