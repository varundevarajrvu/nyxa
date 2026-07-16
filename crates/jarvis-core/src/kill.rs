//! Kill switch (DESIGN.md §5). Two always-armed triggers share one halted
//! flag:
//!  - hotkey Ctrl+Shift+Alt+K (this module, polling watcher thread)
//!  - spoken halt phrases ("stop", "abort", ...) intercepted by the CLI
//!    before registry matching (soft kill — needs the pipeline alive; the
//!    hotkey is the hard path that works even mid-capture/mid-exec)
//!
//! HALTED semantics: wake listening stops, nothing executes; resume is
//! MANUAL ONLY — the same hotkey toggles back. A kill is never silently
//! self-healed. (A pure-KWS "jarvis abort" that skips the STT round-trip
//! needs a custom-trained wake model or sherpa KWS — deferred; tracked in
//! PROGRESS.md.)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const VK_CONTROL: i32 = 0x11;
const VK_SHIFT: i32 = 0x10;
const VK_MENU: i32 = 0x12; // Alt
const VK_K: i32 = 0x4B;

#[cfg(windows)]
fn key_down(vk: i32) -> bool {
    unsafe { (winapi::um::winuser::GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}

/// Spoken phrases that soft-kill (checked against wake-prefix-stripped,
/// normalized transcripts). Deliberately NOT registry data — the kill switch
/// is infrastructure, not a reviewable action.
pub const HALT_PHRASES: &[&str] =
    &["stop", "abort", "halt", "cancel", "never mind", "stop listening"];

pub fn is_halt_phrase(normalized: &str) -> bool {
    HALT_PHRASES.contains(&normalized)
}

/// Spawn the hotkey watcher: Ctrl+Shift+Alt+K toggles `halted` (edge-
/// triggered — hold does not retrigger). Returns the shared flag.
pub fn spawn_hotkey_watcher() -> Arc<AtomicBool> {
    let halted = Arc::new(AtomicBool::new(false));
    let flag = halted.clone();
    std::thread::spawn(move || {
        let mut was_down = false;
        loop {
            let down =
                key_down(VK_CONTROL) && key_down(VK_SHIFT) && key_down(VK_MENU) && key_down(VK_K);
            if down && !was_down {
                let now = !flag.load(Ordering::Relaxed);
                flag.store(now, Ordering::Relaxed);
                // \x07 = terminal bell: audible confirmation either way.
                if now {
                    println!("\x07⛔ HALTED (Ctrl+Shift+Alt+K) — press again to resume");
                } else {
                    println!("\x07▶ resumed — listening again");
                }
            }
            was_down = down;
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    });
    halted
}
