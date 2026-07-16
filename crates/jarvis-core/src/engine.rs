//! The Jarvis engine: the always-on orchestration loop shared by the CLI and
//! the tray app. Owns the loaded models, registry, classifier, and audit log,
//! and drives: wake word -> VAD capture -> transcript -> intent (grammar or
//! LLM) -> tier gate -> execute / spoken-confirm / on-screen-deny.
//!
//! Console output goes through `note!`, which ignores write errors so a
//! windowed app with no stdout can't panic on it. History for the UI comes
//! from the audit log on disk; a short live `status` string feeds the tray.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use whispr_core::{asr, postproc};

use crate::audit::{Audit, Entry};
use crate::llm::{AnthropicBackend, Backend, MockBackend, OllamaBackend, Router};
use crate::registry::{normalize, strip_wake_prefix, IntentMatch, Registry, Tier};
use crate::clap_wake::ClapDetector;
use crate::{exec, kill, mic, tts, vad::Endpointer, wake::WakeDetector, CHUNK, SAMPLE_RATE};

/// Print a line, ignoring any write error (windowed apps have no console).
macro_rules! note {
    ($($a:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), $($a)*);
    }};
}

/// Anti-replay: the same action can't fire again within this window.
const ACTION_COOLDOWN: Duration = Duration::from_secs(3);
/// A spoken confirmation must arrive within this window or it lapses to "no".
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(12);
/// Resting status shown on the tray/orb when idle.
const IDLE_STATUS: &str = "Clap 3\u{00d7} or use your wake word\u{2026}";
/// How many times to prompt "try again" before giving up and going idle.
const MAX_RETRIES: u32 = 2;

/// Words that suggest the utterance is actually a command (not garbled audio).
/// If NONE appear, we skip the slow local LLM and just ask the user to retry —
/// no point waiting 20 s for the model to decline noise.
const COMMAND_HINTS: &[&str] = &[
    "open", "close", "quit", "exit", "launch", "start", "play", "pause", "resume",
    "search", "google", "look", "find", "show", "check", "weather", "volume", "mute",
    "next", "skip", "previous", "louder", "quieter", "delete", "remove", "move",
    "rename", "write", "draft", "compose", "send", "install", "sleep", "gmail",
    "youtube", "spotify", "netflix", "github", "music", "song", "video", "email",
    "tab", "browser", "chrome", "edge", "notepad", "calculator", "terminal", "file",
];

fn looks_like_command(stripped: &str) -> bool {
    stripped.split_whitespace().any(|w| COMMAND_HINTS.contains(&w))
}

/// Words that mark a spoken utterance as a real question/request worth a
/// conversational reply (vs. short garbled audio).
const QUESTION_WORDS: &[&str] = &[
    "what", "whats", "who", "whos", "why", "how", "hows", "when", "where", "which",
    "whose", "is", "are", "am", "can", "could", "would", "should", "do", "does",
    "did", "tell", "explain", "define", "describe", "say", "think", "know", "help",
    "hey", "hello", "hi", "thanks", "thank",
];

/// A non-command utterance that still looks like real speech (a question or a
/// sentence) — worth a conversational reply rather than a "try again".
fn looks_conversational(stripped: &str) -> bool {
    let words: Vec<&str> = stripped.split_whitespace().collect();
    words.len() >= 4 || words.iter().any(|w| QUESTION_WORDS.contains(w))
}
/// Give up if no speech starts within this long after the wake word.
const SPEECH_START_TIMEOUT: Duration = Duration::from_secs(6);
/// Absolute cap on one capture (wake -> forced endpoint).
const CAPTURE_CAP: Duration = Duration::from_secs(16);

/// A NEEDS-CONFIRM action awaiting a spoken yes/no.
struct Pending {
    action_id: String,
    params: HashMap<String, String>,
    label: String,
    expires: Instant,
}

fn is_yes(s: &str) -> bool {
    matches!(
        s,
        "yes" | "yeah" | "yep" | "yup" | "confirm" | "confirmed" | "do it" | "go ahead"
            | "sure" | "okay" | "ok" | "affirmative" | "close it" | "yes please"
    )
}

fn is_no(s: &str) -> bool {
    matches!(
        s,
        "no" | "nope" | "nah" | "cancel" | "stop" | "dont" | "do not" | "negative"
            | "never mind" | "no thanks" | "leave it"
    )
}

/// Human label for a confirmation prompt.
fn confirm_label(id: &str, params: &HashMap<String, String>) -> String {
    let p = |k: &str| params.get(k).cloned().unwrap_or_default();
    match id {
        "app.close" => format!("close {}", p("app")),
        "web.close" => format!("close the {} tab", p("site")),
        "file.move" => format!("move {} to {}", p("src"), p("dst")),
        "file.rename" => format!("rename {} to {}", p("src"), p("dst")),
        "system.sleep" => "put the computer to sleep".into(),
        other => other.replace('.', " "),
    }
}

enum State {
    Idle,
    Capture { started: Instant, heard_speech: bool },
}

struct Pipeline {
    wake: WakeDetector,
    clap: ClapDetector,
    vad: Endpointer,
    engine: asr::Engine,
    dict: postproc::Dictionary,
    state: State,
}

impl Pipeline {
    fn is_capturing(&self) -> bool {
        matches!(self.state, State::Capture { .. })
    }

    /// Feed one 80 ms chunk (int16-range f32). Returns a transcript when an
    /// utterance completes.
    fn push(&mut self, chunk: &[f32]) -> Result<Option<String>> {
        match &mut self.state {
            State::Idle => {
                let clapped = self.clap.feed(chunk);
                let woke = self.wake.detect(chunk)?;
                if clapped || woke.is_some() {
                    if clapped {
                        note!("\x07● wake (3 claps) — listening…");
                    } else {
                        note!("\x07● wake ({:.3}) — listening…", woke.unwrap());
                    }
                    self.clap.reset();
                    self.vad.clear();
                    self.state = State::Capture { started: Instant::now(), heard_speech: false };
                }
                Ok(None)
            }
            State::Capture { started, heard_speech } => {
                self.vad.feed(chunk.iter().map(|&s| s / 32768.0).collect());
                *heard_speech |= self.vad.in_speech();

                let (started, heard_speech) = (*started, *heard_speech);
                let segment = if let Some(seg) = self.vad.segment() {
                    Some(seg)
                } else if started.elapsed() > CAPTURE_CAP {
                    note!("  (capture cap hit — endpointing now)");
                    self.vad.flush()
                } else if !heard_speech && started.elapsed() > SPEECH_START_TIMEOUT {
                    note!("  (heard nothing — going back to sleep)");
                    self.state = State::Idle;
                    return Ok(None);
                } else {
                    None
                };

                let Some(mut seg) = segment else { return Ok(None) };
                self.state = State::Idle;

                let t0 = Instant::now();
                mic::normalize_for_asr(&mut seg);
                let text = self.dict.apply(&self.engine.transcribe(&seg));
                note!(
                    "  [asr] {:.1}s speech in {} ms",
                    seg.len() as f32 / SAMPLE_RATE as f32,
                    t0.elapsed().as_millis()
                );
                Ok((!text.is_empty()).then_some(text))
            }
        }
    }

    /// Reopen the capture window without a new wake ("hey jarvis" alone).
    fn relisten(&mut self) {
        self.vad.clear();
        self.state = State::Capture { started: Instant::now(), heard_speech: false };
    }
}

enum Handled {
    Done,
    ReListen,
}

/// Per-utterance handling context — borrows the engine's fields (distinct, so
/// disjoint borrows are fine) for one transcript.
struct Ctx<'a> {
    reg: &'a Registry,
    cooldowns: &'a mut HashMap<String, Instant>,
    audit: &'a Audit,
    halted: &'a AtomicBool,
    dry_run: bool,
    router: &'a Option<Router>,
    pending: &'a mut Option<Pending>,
    status: &'a Mutex<String>,
    /// Consecutive "didn't catch that" count, for the retry prompt.
    retries: &'a mut u32,
}

/// Couldn't understand the utterance: ask the user to retry (spoken + status)
/// and reopen the mic so they can repeat without the wake word — up to
/// MAX_RETRIES, then give up and go idle. Fast: no LLM involved.
fn unheard_retry(ctx: &mut Ctx, text: &str, t0: Instant) -> Handled {
    *ctx.retries += 1;
    audit(ctx.audit, text, "none", None, None, None, serde_json::Value::Null, "unrecognized", t0);
    if *ctx.retries <= MAX_RETRIES {
        note!("   \u{2026} didn't catch that — asking to retry");
        set_status(ctx.status, "Didn\u{2019}t catch that \u{2014} try again");
        if !ctx.dry_run {
            tts::speak("I didn't catch that. Please try again.");
        }
        Handled::ReListen
    } else {
        *ctx.retries = 0;
        note!("   \u{2026} giving up — say \u{201c}hey jarvis\u{201d} to start over");
        set_status(ctx.status, "Say \u{201c}hey jarvis\u{201d} to try again");
        Handled::Done
    }
}

/// Conversational reply: ask the local LLM and speak the answer. Falls back to
/// a retry prompt if there's no LLM backend or the model fails.
fn chat_reply(ctx: &mut Ctx, text: &str, t0: Instant) -> Handled {
    *ctx.retries = 0;
    if ctx.router.is_none() {
        return unheard_retry(ctx, text, t0);
    }
    set_status(ctx.status, "Thinking\u{2026}");
    let reply = ctx.router.as_ref().unwrap().chat(text);
    match reply {
        Ok(reply) => {
            note!("   \u{1f5e3} {reply}");
            set_status(ctx.status, format!("\u{201c}{}\u{201d}", truncate(&reply, 46)));
            if !ctx.dry_run {
                tts::speak(&reply);
            }
            audit(ctx.audit, text, "chat", Some("sonnet"), None, None, serde_json::Value::Null, "replied", t0);
            Handled::Done
        }
        Err(e) => {
            note!("   ✗ chat failed: {e:#}");
            unheard_retry(ctx, text, t0)
        }
    }
}

fn set_status(status: &Mutex<String>, s: impl Into<String>) {
    if let Ok(mut g) = status.lock() {
        *g = s.into();
    }
}

fn gate_and_exec(
    reg: &Registry,
    cooldowns: &mut HashMap<String, Instant>,
    dry_run: bool,
    halted: &AtomicBool,
    m: &IntentMatch,
) -> &'static str {
    let id = m.action.id.clone();
    if !m.action.enabled {
        note!("   ✗ '{id}' is disabled in the registry config");
        return "disabled";
    }
    match m.action.tier {
        Tier::NeedsConfirm => "needs_confirm_blocked",
        Tier::DenyByDefault => "deny_blocked",
        Tier::SafeAuto => {
            if cooldowns.get(&id).is_some_and(|last| last.elapsed() < ACTION_COOLDOWN) {
                note!("   ✗ '{id}' on cooldown (anti-replay)");
                return "cooldown";
            }
            if halted.load(Ordering::Relaxed) {
                note!("   ⛔ killed before execution");
                return "killed";
            }
            match exec::execute(reg, m, dry_run) {
                Ok(exec::Outcome::Executed(what)) => {
                    cooldowns.insert(id, Instant::now());
                    note!("   ✓ {what}");
                    "executed"
                }
                Ok(exec::Outcome::Spoken(reply)) => {
                    cooldowns.insert(id, Instant::now());
                    note!("   \u{1f5e3} {}", truncate(&reply, 100));
                    tts::speak(&reply);
                    "spoke"
                }
                Ok(exec::Outcome::DryRun(what)) => {
                    note!("   [dry-run] {what}");
                    "dry_run"
                }
                Ok(exec::Outcome::NotImplemented(why)) => {
                    note!("   ✗ {why}");
                    "not_implemented"
                }
                Err(e) => {
                    note!("   ✗ exec failed: {e:#}");
                    "exec_error"
                }
            }
        }
    }
}

fn handle_transcript(ctx: &mut Ctx, text: &str) -> Handled {
    let t0 = Instant::now();
    let reg: &Registry = ctx.reg;
    let stripped = strip_wake_prefix(&normalize(text));

    if let Some(p) = ctx.pending.take() {
        let expired = p.expires < Instant::now();
        if !expired && is_yes(&stripped) {
            return execute_confirmed(ctx, reg, p, text, t0);
        }
        if !expired && is_no(&stripped) {
            note!("   ✗ cancelled — '{}'", p.label);
            audit_pending(ctx.audit, &p, text, "declined", t0);
            return Handled::Done;
        }
        if expired {
            note!("   (confirmation for '{}' timed out — cancelled)", p.label);
        } else {
            note!("   (no yes/no — cancelled '{}', treating as a new command)", p.label);
        }
        audit_pending(ctx.audit, &p, text, "declined", t0);
    }

    if stripped.is_empty() {
        note!("   (yes? — still listening)");
        *ctx.retries = 0;
        audit(ctx.audit, text, "none", None, None, None, serde_json::Value::Null, "relisten", t0);
        return Handled::ReListen;
    }
    if kill::is_halt_phrase(&stripped) {
        ctx.halted.store(true, Ordering::Relaxed);
        *ctx.retries = 0;
        note!("\x07⛔ HALTED by voice — Ctrl+Shift+Alt+K to resume");
        set_status(ctx.status, "Paused (say/press resume)");
        audit(ctx.audit, text, "none", None, None, None, serde_json::Value::Null, "halted", t0);
        return Handled::Done;
    }

    if let Some(m) = reg.match_transcript(text) {
        return act_on_intent(ctx, reg, &m, "grammar", None, None, text, t0);
    }
    // Robust bag-of-words fallback for garbled close/quit commands, so they
    // fire instantly (needs-confirm) instead of falling to the slow LLM.
    if let Some(m) = reg.match_close_fallback(text) {
        return act_on_intent(ctx, reg, &m, "grammar", None, None, text, t0);
    }

    // Grammar missed. Route by what the utterance looks like:
    //  - command-ish  -> LLM classify (a command the grammar didn't cover)
    //  - a question   -> conversational chat reply (spoken)
    //  - short/garble -> fast "try again" (no slow LLM)
    let conversational = looks_conversational(&stripped);
    if !looks_like_command(&stripped) {
        if conversational {
            return chat_reply(ctx, text, t0);
        }
        return unheard_retry(ctx, text, t0);
    }

    // Plausibly a command the grammar missed — worth the LLM classifier.
    set_status(ctx.status, "Thinking\u{2026}");
    let classified = ctx.router.as_ref().map(|r| r.classify(reg, text));
    match classified {
        Some(Ok(Some(c))) => {
            let action = reg
                .actions
                .iter()
                .find(|a| a.id == c.action_id)
                .expect("classifier validated action_id against the registry");
            let m = IntentMatch { action, params: c.params.clone() };
            act_on_intent(ctx, reg, &m, "llm", Some(c.model.tag()), Some(c.confidence), text, t0)
        }
        // Not a command after all — if it reads like a question, chat; else retry.
        Some(Ok(None)) if conversational => chat_reply(ctx, text, t0),
        Some(Ok(None)) => {
            note!("   no matching action (LLM declined)");
            unheard_retry(ctx, text, t0)
        }
        Some(Err(e)) => {
            note!("   ✗ LLM classification failed: {e:#}");
            unheard_retry(ctx, text, t0)
        }
        None => unheard_retry(ctx, text, t0),
    }
}

fn act_on_intent(
    ctx: &mut Ctx,
    reg: &Registry,
    m: &IntentMatch,
    stage: &str,
    model: Option<&str>,
    confidence: Option<f32>,
    text: &str,
    t0: Instant,
) -> Handled {
    *ctx.retries = 0; // a real command was recognized
    let id = m.action.id.clone();
    let tier = m.action.tier.as_str();
    let params = serde_json::to_value(&m.params).unwrap_or(serde_json::Value::Null);
    let head = model.map(|mm| format!("[{mm}] ")).unwrap_or_default();
    let conf = confidence.map(|c| format!(" conf {c:.2}")).unwrap_or_default();
    note!("   {head}intent: {id} [{tier}]{conf}  params: {:?}", m.params);

    if !m.action.enabled {
        note!("   ✗ '{id}' is disabled in the registry config");
        audit(ctx.audit, text, stage, model, Some(&id), Some(tier), params, "disabled", t0);
        return Handled::Done;
    }

    match m.action.tier {
        Tier::SafeAuto => {
            let outcome = gate_and_exec(reg, ctx.cooldowns, ctx.dry_run, ctx.halted, m);
            set_status(ctx.status, format!("{id}: {outcome}"));
            audit(ctx.audit, text, stage, model, Some(&id), Some(tier), params, outcome, t0);
            Handled::Done
        }
        Tier::NeedsConfirm => {
            let label = confirm_label(&id, &m.params);
            let prompt = format!("{label}? Say yes or no.");
            note!("   ⏸ {prompt}");
            set_status(ctx.status, prompt.clone());
            if !ctx.dry_run {
                tts::speak(&prompt);
            }
            *ctx.pending = Some(Pending {
                action_id: id.clone(),
                params: m.params.clone(),
                label,
                expires: Instant::now() + CONFIRM_TIMEOUT,
            });
            audit(ctx.audit, text, stage, model, Some(&id), Some(tier), params, "awaiting_confirm", t0);
            Handled::ReListen
        }
        Tier::DenyByDefault => deny_flow(ctx, reg, m, stage, model, text, t0),
    }
}

fn deny_flow(
    ctx: &mut Ctx,
    reg: &Registry,
    m: &IntentMatch,
    stage: &str,
    model: Option<&str>,
    text: &str,
    t0: Instant,
) -> Handled {
    let id = m.action.id.clone();
    let tier = m.action.tier.as_str();

    let (resolved, body) = match id.as_str() {
        "file.delete" => {
            let raw = m.params.get("path").map(String::as_str).unwrap_or("");
            match exec::resolve_user_file(raw) {
                Ok(abs) => {
                    let mut p = m.params.clone();
                    let abs_str = abs.to_string_lossy().into_owned();
                    p.insert("path".into(), abs_str.clone());
                    let body = format!(
                        "Delete this file?\n\n{abs_str}\n\nIt will go to the Recycle Bin (recoverable).\n\nYes = delete  ·  No = cancel",
                    );
                    (p, body)
                }
                Err(why) => {
                    note!("   ✗ {why}");
                    let params = serde_json::to_value(&m.params).unwrap_or(serde_json::Value::Null);
                    audit(ctx.audit, text, stage, model, Some(&id), Some(tier), params, "unrecognized", t0);
                    return Handled::Done;
                }
            }
        }
        _ => {
            note!("   ✗ '{id}' is a DENY action with no executor yet — not available");
            let params = serde_json::to_value(&m.params).unwrap_or(serde_json::Value::Null);
            audit(ctx.audit, text, stage, model, Some(&id), Some(tier), params, "deny_blocked", t0);
            return Handled::Done;
        }
    };

    let params = serde_json::to_value(&resolved).unwrap_or(serde_json::Value::Null);
    if ctx.dry_run {
        note!("   [dry-run] would prompt (on-screen): {}", body.replace('\n', " "));
        audit(ctx.audit, text, stage, model, Some(&id), Some(tier), params, "dry_run", t0);
        return Handled::Done;
    }
    note!("   ⏸ on-screen confirmation required (click Yes/No)…");
    set_status(ctx.status, "Waiting for on-screen confirmation…");
    let approved = crate::dialog::confirm("Jarvis — confirm action", &body);
    if !approved {
        note!("   ✗ denied (dialog: No / closed / timed out)");
        audit(ctx.audit, text, stage, model, Some(&id), Some(tier), params, "denied", t0);
        return Handled::Done;
    }
    if ctx.halted.load(Ordering::Relaxed) {
        note!("   ⛔ killed before execution");
        audit(ctx.audit, text, stage, model, Some(&id), Some(tier), params, "killed", t0);
        return Handled::Done;
    }

    let action = reg.actions.iter().find(|a| a.id == id).expect("action exists");
    let m2 = IntentMatch { action, params: resolved };
    let outcome = match exec::execute(reg, &m2, ctx.dry_run) {
        Ok(exec::Outcome::Executed(what)) => {
            note!("   ✓ {what}");
            "confirmed_then_executed"
        }
        Ok(exec::Outcome::Spoken(reply)) => {
            note!("   \u{1f5e3} {}", truncate(&reply, 100));
            tts::speak(&reply);
            "spoke"
        }
        Ok(exec::Outcome::DryRun(what)) => {
            note!("   [dry-run] {what}");
            "dry_run"
        }
        Ok(exec::Outcome::NotImplemented(why)) => {
            note!("   ✗ {why}");
            "not_implemented"
        }
        Err(e) => {
            note!("   ✗ exec failed: {e:#}");
            "exec_error"
        }
    };
    audit(ctx.audit, text, stage, model, Some(&id), Some(tier), params, outcome, t0);
    Handled::Done
}

fn execute_confirmed(ctx: &mut Ctx, reg: &Registry, p: Pending, text: &str, t0: Instant) -> Handled {
    let Some(action) = reg.actions.iter().find(|a| a.id == p.action_id) else {
        note!("   ✗ action '{}' no longer in registry", p.action_id);
        return Handled::Done;
    };
    let tier = action.tier.as_str();
    let params = serde_json::to_value(&p.params).unwrap_or(serde_json::Value::Null);
    note!("   ✓ confirmed — {}", p.label);

    if ctx.halted.load(Ordering::Relaxed) {
        note!("   ⛔ killed before execution");
        audit(ctx.audit, text, "confirm", None, Some(&p.action_id), Some(tier), params, "killed", t0);
        return Handled::Done;
    }
    let m = IntentMatch { action, params: p.params.clone() };
    let outcome = match exec::execute(reg, &m, ctx.dry_run) {
        Ok(exec::Outcome::Executed(what)) => {
            note!("   ✓ {what}");
            "confirmed_then_executed"
        }
        Ok(exec::Outcome::Spoken(reply)) => {
            note!("   \u{1f5e3} {}", truncate(&reply, 100));
            tts::speak(&reply);
            "spoke"
        }
        Ok(exec::Outcome::DryRun(what)) => {
            note!("   [dry-run] {what}");
            "dry_run"
        }
        Ok(exec::Outcome::NotImplemented(why)) => {
            note!("   ✗ {why}");
            "not_implemented"
        }
        Err(e) => {
            note!("   ✗ exec failed: {e:#}");
            "exec_error"
        }
    };
    set_status(ctx.status, format!("{}: {outcome}", p.action_id));
    audit(ctx.audit, text, "confirm", None, Some(&p.action_id), Some(tier), params, outcome, t0);
    Handled::Done
}

fn audit_pending(a: &Audit, p: &Pending, text: &str, outcome: &str, t0: Instant) {
    let params = serde_json::to_value(&p.params).unwrap_or(serde_json::Value::Null);
    audit(a, text, "confirm", None, Some(&p.action_id), None, params, outcome, t0);
}

#[allow(clippy::too_many_arguments)]
fn audit(
    audit: &Audit,
    transcript: &str,
    stage: &str,
    model: Option<&str>,
    action: Option<&str>,
    tier: Option<&str>,
    params: serde_json::Value,
    outcome: &str,
    t0: Instant,
) {
    audit.log(&Entry {
        ts: crate::audit::now_rfc3339(),
        transcript,
        stage,
        model,
        action,
        tier,
        params,
        outcome,
        latency_ms: t0.elapsed().as_millis() as u64,
    });
}

// ─── Engine facade ───────────────────────────────────────────────────────────

pub struct EngineConfig {
    pub wake_threshold: f32,
    pub dry_run: bool,
    pub mock_llm: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self { wake_threshold: crate::wake::DEFAULT_THRESHOLD, dry_run: false, mock_llm: false }
    }
}

pub struct Engine {
    reg: Registry,
    audit: Audit,
    router: Option<Router>,
    cooldowns: HashMap<String, Instant>,
    pending: Option<Pending>,
    dry_run: bool,
    status: Arc<Mutex<String>>,
    retries: u32,
    /// Loaded lazily on first audio use (ASR is heavy; text mode skips it).
    pipeline: Option<Pipeline>,
    models: PathBuf,
    wake_threshold: f32,
}

impl Engine {
    /// Load the registry, audit log, and classifier backend. ASR/wake/VAD are
    /// loaded lazily when audio actually starts (so `--text` is instant).
    pub fn load(cfg: EngineConfig) -> Result<Self> {
        let models = crate::models_root()?;
        let reg_path = models
            .parent()
            .context("models root has no parent")?
            .join("registry/actions.toml");
        let reg = Registry::load(&reg_path)?;
        note!("[registry] {} actions loaded from {}", reg.actions.len(), reg_path.display());
        let audit = Audit::open_default()?;
        note!("[audit] logging to {}", audit.path().display());

        let router = build_router(cfg.mock_llm, &reg);

        Ok(Self {
            reg,
            audit,
            router,
            cooldowns: HashMap::new(),
            pending: None,
            dry_run: cfg.dry_run,
            status: Arc::new(Mutex::new("Starting…".into())),
            retries: 0,
            pipeline: None,
            models,
            wake_threshold: cfg.wake_threshold,
        })
    }

    /// A shared handle to the live status string (for a tray tooltip / window).
    pub fn status_handle(&self) -> Arc<Mutex<String>> {
        self.status.clone()
    }

    pub fn audit_path(&self) -> PathBuf {
        self.audit.path().clone()
    }

    fn ensure_pipeline(&mut self) -> Result<()> {
        if self.pipeline.is_some() {
            return Ok(());
        }
        // Prefer a custom "hey nyxa" model if the user has trained + dropped one
        // in; otherwise fall back to the pretrained "hey jarvis".
        let wake_dir = self.models.join("wake");
        let model_file = if wake_dir.join("hey_nyxa.onnx").exists() {
            note!("[wake] using custom model: hey_nyxa.onnx (say \"hey nyxa\")");
            "hey_nyxa.onnx"
        } else {
            note!("[wake] using pretrained model: hey_jarvis (say \"hey jarvis\")");
            "hey_jarvis_v0.1.onnx"
        };
        let wake = WakeDetector::load(&wake_dir, model_file, self.wake_threshold)?;
        let vad = Endpointer::new(&self.models.join("silero_vad.onnx"))?;
        let asr_root = self
            .models
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("whispr/bench/models"))
            .filter(|p| p.exists())
            .map_or_else(asr::default_models_root, Ok)?;
        let engine = asr::Engine::load(asr::EngineKind::Parakeet, &asr_root, 4)?;
        let dict_path = asr_root.join("../../whispr.dict.txt");
        let dict = if dict_path.exists() {
            postproc::Dictionary::load(&dict_path)?
        } else {
            postproc::Dictionary::empty()
        };
        self.pipeline = Some(Pipeline {
            wake,
            clap: ClapDetector::new(),
            vad,
            engine,
            dict,
            state: State::Idle,
        });
        Ok(())
    }

    /// Process a single transcript through the full pipeline (no audio). Used
    /// by `--text` and tests. Returns true if it asked to keep listening.
    pub fn feed_text(&mut self, text: &str, halted: &AtomicBool) -> bool {
        let mut ctx = Ctx {
            reg: &self.reg,
            cooldowns: &mut self.cooldowns,
            audit: &self.audit,
            halted,
            dry_run: self.dry_run,
            router: &self.router,
            pending: &mut self.pending,
            status: &self.status,
            retries: &mut self.retries,
        };
        matches!(handle_transcript(&mut ctx, text), Handled::ReListen)
    }

    /// Run the always-on microphone loop until the stream ends. `halted` is the
    /// shared kill flag (hotkey + voice); while set, audio is drained and
    /// nothing runs.
    pub fn run_mic(&mut self, halted: Arc<AtomicBool>) -> Result<()> {
        self.ensure_pipeline()?;
        let m = mic::Mic::open()?;
        note!("jarvis listening — say \"hey jarvis\", then speak.");
        set_status(&self.status, "Listening for \u{201c}hey jarvis\u{201d}\u{2026}");

        let mut was_halted = false;
        let mut was_capturing = false;
        let mut last_activity = Instant::now();
        let mut resting = false;
        for chunk in m.chunks() {
            let is_halted = halted.load(Ordering::Relaxed);
            if is_halted != was_halted {
                was_halted = is_halted;
                resting = false;
                set_status(
                    &self.status,
                    if is_halted { "Paused (halted)" } else { IDLE_STATUS },
                );
                self.audit.log(&Entry {
                    ts: crate::audit::now_rfc3339(),
                    transcript: "",
                    stage: "none",
                    model: None,
                    action: None,
                    tier: None,
                    params: serde_json::Value::Null,
                    outcome: if is_halted { "halted" } else { "resumed" },
                    latency_ms: 0,
                });
            }
            if is_halted {
                continue;
            }

            // Return the orb to calm idle a few seconds after any activity, so
            // no transient state (e.g. "Thinking…") ever sticks.
            if !resting && last_activity.elapsed() > Duration::from_secs(4) {
                if self.pipeline.as_ref().is_some_and(|p| !p.is_capturing()) {
                    set_status(&self.status, IDLE_STATUS);
                    resting = true;
                }
            }

            let text = {
                let pipe = self.pipeline.as_mut().expect("pipeline loaded");
                let out = pipe.push(&chunk)?;
                let capturing = pipe.is_capturing();
                if capturing != was_capturing {
                    was_capturing = capturing;
                    if capturing {
                        resting = false;
                        last_activity = Instant::now();
                        set_status(&self.status, "Listening\u{2026}");
                    }
                }
                out
            };

            if let Some(text) = text {
                note!("»» \"{text}\"");
                resting = false;
                set_status(&self.status, format!("Heard: {}", truncate(&text, 40)));
                let relisten = {
                    let mut ctx = Ctx {
                        reg: &self.reg,
                        cooldowns: &mut self.cooldowns,
                        audit: &self.audit,
                        halted: &halted,
                        dry_run: self.dry_run,
                        router: &self.router,
                        pending: &mut self.pending,
                        status: &self.status,
                        retries: &mut self.retries,
                    };
                    matches!(handle_transcript(&mut ctx, &text), Handled::ReListen)
                };
                last_activity = Instant::now();
                if relisten {
                    // Drop audio buffered during a (blocking) TTS/dialog prompt
                    // so it isn't captured as the answer.
                    while m.chunks().try_recv().is_ok() {}
                    self.pipeline.as_mut().expect("pipeline").relisten();
                }
            }
        }
        Ok(())
    }

    /// Feed a wav file as if it were the mic (dev/test). 3 s of trailing
    /// silence is appended so the VAD can endpoint the last utterance.
    pub fn feed_wav(&mut self, path: &Path, halted: &AtomicBool) -> Result<()> {
        self.ensure_pipeline()?;
        let mut reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        anyhow::ensure!(
            spec.sample_rate == SAMPLE_RATE && spec.channels == 1,
            "--sim expects 16 kHz mono wav"
        );
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32))
            .collect::<Result<_, _>>()?;
        let silence = vec![0f32; SAMPLE_RATE as usize * 3];
        for chunk in samples.chunks_exact(CHUNK).chain(silence.chunks_exact(CHUNK)) {
            let text = self.pipeline.as_mut().expect("pipeline").push(chunk)?;
            if let Some(text) = text {
                note!("»» \"{text}\"");
                if self.feed_text(&text, halted) {
                    self.pipeline.as_mut().expect("pipeline").relisten();
                }
            }
        }
        Ok(())
    }
}

/// Build the classifier backend (mock / Anthropic / Ollama / none), matching
/// the CLI's priority order.
fn build_router(mock_llm: bool, reg: &Registry) -> Option<Router> {
    if mock_llm {
        note!("[llm] MOCK backend (canned classifications)");
        return Some(Router::new(Box::new(MockBackend::demo()), reg));
    }
    if let Some(backend) = AnthropicBackend::from_env() {
        note!("[llm] Anthropic API ({} / {})", crate::llm::MODEL_SONNET, crate::llm::MODEL_OPUS);
        return Some(Router::new(Box::new(backend) as Box<dyn Backend>, reg));
    }
    // Default to the 1.5B model: roughly 2x faster on CPU than the 3B for
    // classification/short chat, and small enough to stay comfortably resident.
    let model = std::env::var("JARVIS_OLLAMA_MODEL").unwrap_or_else(|_| "qwen2.5:1.5b".into());
    let escalation =
        std::env::var("JARVIS_OLLAMA_ESCALATION_MODEL").unwrap_or_else(|_| model.clone());
    let backend = OllamaBackend::new(model.clone()).with_escalation(escalation.clone());
    if backend.is_up() {
        note!("[llm] local Ollama (stage-1: {model}, stage-2: {escalation})");
        // Pre-warm in the background so the model is resident (and keep_alive'd)
        // before the first real command — otherwise that command eats the cold
        // load. Detached: startup never blocks on it.
        backend.warm();
        Some(Router::new(Box::new(backend) as Box<dyn Backend>, reg))
    } else {
        note!("[llm] no ANTHROPIC_API_KEY and no Ollama at localhost:11434 — offline stage-0-only mode");
        None
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}
