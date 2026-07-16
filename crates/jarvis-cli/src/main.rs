//! jarvis-cli — headless always-on listener. Wraps `jarvis_core::engine`:
//! wake word -> VAD capture -> transcript -> intent (grammar / local LLM) ->
//! tier gate -> execute, spoken-confirm, or on-screen-deny.
//!
//!   jarvis-cli               live (mic)
//!   jarvis-cli --sim FILE    feed a 16 kHz mono wav as if it were the mic
//!   jarvis-cli --text "..."  parse + gate + execute a transcript directly
//!   jarvis-cli --dry-run     print what would execute instead of executing
//!   jarvis-cli --mock-llm    use canned LLM classifications (offline test)
//!   jarvis-cli --threshold F wake threshold (default 0.85)

use std::path::PathBuf;

use anyhow::{Context, Result};
use jarvis_core::engine::{Engine, EngineConfig};
use jarvis_core::kill;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut sim: Option<PathBuf> = None;
    let mut text_cmds: Vec<String> = Vec::new();
    let mut cfg = EngineConfig::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sim" => sim = it.next().map(PathBuf::from),
            "--text" => text_cmds.extend(it.next().cloned()),
            "--dry-run" => cfg.dry_run = true,
            "--mock-llm" => cfg.mock_llm = true,
            "--threshold" => {
                cfg.wake_threshold = it.next().context("--threshold needs a value")?.parse()?;
            }
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }

    let mut engine = Engine::load(cfg)?;
    let halted = kill::spawn_hotkey_watcher();

    // Text mode: no audio stack — parse, gate, execute, exit.
    if !text_cmds.is_empty() {
        for t in &text_cmds {
            println!("»» \"{t}\"");
            engine.feed_text(t, &halted);
        }
        return Ok(());
    }

    if let Some(path) = sim {
        return engine.feed_wav(&path, &halted);
    }

    println!("kill switch: Ctrl+Shift+Alt+K (or say \"hey jarvis, stop\")");
    engine.run_mic(halted)
}
