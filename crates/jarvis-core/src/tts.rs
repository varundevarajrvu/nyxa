//! Best-effort SAPI text-to-speech for confirmation prompts and kill/HALT
//! announcements (DESIGN §7). Blocking (the caller drains mic echo afterward);
//! any failure is swallowed — a missing voice must never break the pipeline.

use std::process::Command;

/// Speak `text` and block until done. Text is app-controlled (registry aliases
/// + fixed strings); single quotes are still escaped for the PS string.
pub fn speak(text: &str) {
    let safe = text.replace('\'', "''");
    // Zira (female) voice; fall back silently if it isn't installed.
    let script = format!(
        "Add-Type -AssemblyName System.Speech; \
         $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; \
         try {{ $s.SelectVoice('Microsoft Zira Desktop') }} catch {{}}; \
         $s.Rate = 1; $s.Speak('{safe}')"
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output();
}
