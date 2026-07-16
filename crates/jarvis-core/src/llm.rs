//! Stage-1/stage-2 LLM intent classification (DESIGN.md §2, §4).
//!
//! The SHIPPED app calls the Anthropic Messages API directly (`AnthropicBackend`,
//! ureq POST — no Claude Code session, no MCP). A `MockBackend` returns canned
//! classifications so the routing/escalation/validation logic is fully testable
//! offline before a key is present.
//!
//! Routing policy, enforced in `Router::classify`:
//!  - stage 1: claude-sonnet-5 classifies any transcript stage-0 grammar missed.
//!  - stage 2: claude-opus-4-8 re-classifies when the result is low-confidence,
//!    flagged ambiguous, resolves to a DENY-BY-DEFAULT action, or a destructive
//!    verb resolved to a non-destructive action (misclassification guard).
//!  - Opus only classifies better — it never authorizes. The registry tier and
//!    the J5 confirmation gates are unaffected by anything a model says.
//!  - Fail closed: any transport/parse/validation failure yields no action.
//!
//! Only TEXT ever leaves the machine (the transcript + the action catalog),
//! never audio. Stage-0 hits never reach this module at all.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::registry::{normalize, ParamType, Registry};

/// API model ids. NOTE: the Anthropic API may require a dated suffix
/// (e.g. `claude-sonnet-5-YYYYMMDD`) — override via config when wiring the key.
pub const MODEL_SONNET: &str = "claude-sonnet-5";
pub const MODEL_OPUS: &str = "claude-opus-4-8";

/// Confidence at/below which stage-1 is escalated to stage-2.
const ESCALATE_BELOW: f32 = 0.75;

/// Below this, decline outright rather than act on a near-guess — even behind a
/// confirmation prompt. Applied to the FINAL result (after any escalation), so
/// a stronger model gets its chance to raise confidence first. A clearer
/// re-phrasing from the user usually hits the instant grammar path anyway.
const CONFIDENCE_FLOOR: f32 = 0.3;

/// Verbs that must never be reached via a SAFE-AUTO action off an LLM guess —
/// their presence in the transcript with a non-destructive resolution triggers
/// an opus re-check.
const DESTRUCTIVE_VERBS: &[&str] =
    &["delete", "remove", "send", "buy", "purchase", "uninstall", "format", "erase", "wipe"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Model {
    Sonnet,
    Opus,
}

impl Model {
    pub fn id(&self) -> &'static str {
        match self {
            Model::Sonnet => MODEL_SONNET,
            Model::Opus => MODEL_OPUS,
        }
    }
    /// Audit-log tag (DESIGN.md §5 `model` field).
    pub fn tag(&self) -> &'static str {
        match self {
            Model::Sonnet => "sonnet",
            Model::Opus => "opus",
        }
    }
}

/// A completion backend: given (model, system, user) return the raw text the
/// model produced. `force_json` constrains the output to strict JSON (for
/// intent classification); false lets the model reply in prose (for chat).
pub trait Backend: Send + Sync {
    fn complete(&self, model: Model, system: &str, user: &str, force_json: bool) -> Result<String>;

    /// Whether stage-2 (opus tier) is a genuinely different/stronger model
    /// than stage-1. When it isn't (e.g. Ollama configured with one model for
    /// both tiers), escalation at temperature 0 just re-derives the identical
    /// answer, so the router skips it to save a redundant call. Cloud backends
    /// with distinct sonnet/opus models return true.
    fn has_distinct_escalation(&self) -> bool {
        true
    }
}

/// A validated stage-1/2 classification, ready to run through the same tier
/// gate as a grammar hit.
#[derive(Debug, Clone)]
pub struct Classification {
    pub action_id: String,
    /// Validated + canonicalized (enum synonyms resolved, aliases confirmed).
    pub params: HashMap<String, String>,
    pub confidence: f32,
    pub ambiguity_reason: Option<String>,
    /// Which model's answer this is (opus if escalation ran).
    pub model: Model,
}

/// Raw shape the model is asked to emit.
#[derive(serde::Deserialize)]
struct RawClassification {
    #[serde(default)]
    action_id: Option<String>,
    #[serde(default)]
    params: HashMap<String, String>,
    #[serde(default)]
    confidence: f32,
    #[serde(default)]
    ambiguity_reason: Option<String>,
}

const SYSTEM_PROMPT: &str = "You are the intent classifier for Jarvis, a \
voice automation agent. You are given a transcribed spoken utterance and a \
catalog of the ONLY actions Jarvis can perform. Map the utterance to exactly \
one catalog action, or to null if none genuinely fits. Respond with ONLY \
strict minified JSON, no prose, no markdown, of the form: \
{\"action_id\":<catalog id or null>,\"params\":{<name>:<string value>},\
\"confidence\":<0..1>,\"ambiguity_reason\":<short string or null>}. Rules: \
action_id MUST be a catalog id verbatim or null — never invent one. Provide \
every parameter the chosen action declares, as strings; for enum params use \
one of the listed allowed values. Set confidence to your genuine certainty. \
Set ambiguity_reason to a short phrase when the utterance is vague, \
underspecified, could match several actions, or is high-stakes/destructive; \
otherwise null. Never include commentary, explanation, or code fences.";

pub struct Router {
    backend: Box<dyn Backend>,
    catalog: String,
}

impl Router {
    pub fn new(backend: Box<dyn Backend>, reg: &Registry) -> Self {
        Self { backend, catalog: build_catalog(reg) }
    }

    /// Classify a transcript stage-0 missed. Returns:
    ///  - Ok(Some(c)) — a validated action to run through the tier gate
    ///  - Ok(None)    — the model declined (null) or its answer failed
    ///                  validation (fail closed → unrecognized)
    ///  - Err(_)      — transport/parse failure (fail closed → llm_error)
    pub fn classify(&self, reg: &Registry, transcript: &str) -> Result<Option<Classification>> {
        let user = format!("Utterance: \"{transcript}\"\n\nActions:\n{}", self.catalog);

        // Stage 1 — sonnet.
        let raw1 = self.backend.complete(Model::Sonnet, SYSTEM_PROMPT, &user, true)?;
        let c1 = parse_and_validate(reg, &raw1, Model::Sonnet);

        // Escalation (DESIGN.md §2 stage-2 triggers). Skipped when stage-2 isn't
        // a distinct model — re-asking the same model at temp 0 just repeats it.
        let result = if self.backend.has_distinct_escalation()
            && should_escalate(reg, transcript, c1.as_ref())
        {
            // Stage 2 — opus re-reads. If opus errors, fall back to stage-1.
            match self.backend.complete(Model::Opus, SYSTEM_PROMPT, &user, true) {
                Ok(raw2) => parse_and_validate(reg, &raw2, Model::Opus),
                Err(e) => {
                    eprintln!("[llm] opus escalation failed ({e:#}); using stage-1 result");
                    c1
                }
            }
        } else {
            c1
        };

        // Confidence floor on the final answer: don't act on a near-guess. This
        // is the last word — any stronger model has already had its turn.
        if let Some(c) = &result {
            if c.confidence < CONFIDENCE_FLOOR {
                eprintln!(
                    "[llm] declining low-confidence resolution: {} conf {:.2} (< {CONFIDENCE_FLOOR})",
                    c.action_id, c.confidence
                );
                return Ok(None);
            }
        }
        Ok(result)
    }

    /// Conversational reply — spoken back to the user. Prose, not JSON.
    /// Kept short so it's pleasant to hear aloud.
    pub fn chat(&self, transcript: &str) -> Result<String> {
        const CHAT_SYSTEM: &str = "You are Nyxa, a voice assistant with a calm, \
dry, slightly gothic wit. Answer the user's spoken message helpfully in ONE or \
TWO short sentences — this will be read aloud, so use plain conversational text: \
no markdown, no bullet points, no emoji, no stage directions. If asked for a \
joke, invent a fresh, uncommon one each time — never repeat a well-worn joke.";
        let reply = self
            .backend
            .complete(Model::Sonnet, CHAT_SYSTEM, transcript, false)?;
        let reply = reply.trim().to_string();
        if reply.is_empty() {
            anyhow::bail!("empty chat reply");
        }
        Ok(reply)
    }
}

/// Should stage-1's result be re-checked by opus?
fn should_escalate(reg: &Registry, transcript: &str, c: Option<&Classification>) -> bool {
    match c {
        // No confident stage-1 action but the utterance clearly wanted
        // something — let opus have a look before giving up.
        None => true,
        Some(c) => {
            if c.confidence <= ESCALATE_BELOW || c.ambiguity_reason.is_some() {
                return true;
            }
            // Anything resolving to DENY gets a stronger second read before a
            // human is even prompted.
            if let Some(action) = reg.actions.iter().find(|a| a.id == c.action_id) {
                if action.tier == crate::registry::Tier::DenyByDefault {
                    return true;
                }
                // Destructive verb resolved to a non-DENY action: misclassification guard.
                let norm = normalize(transcript);
                let has_destructive = DESTRUCTIVE_VERBS
                    .iter()
                    .any(|v| norm.split_whitespace().any(|w| w == *v));
                if has_destructive && action.tier != crate::registry::Tier::DenyByDefault {
                    return true;
                }
            }
            false
        }
    }
}

/// Parse the model's JSON and validate against the registry. Any deviation
/// (unknown action, bad params, missing fields) → None (fail closed).
fn parse_and_validate(reg: &Registry, raw: &str, model: Model) -> Option<Classification> {
    let json = extract_json(raw)?;
    let rc: RawClassification = serde_json::from_str(&json).ok()?;
    let action_id = rc.action_id?;
    if action_id.eq_ignore_ascii_case("null") {
        return None;
    }
    let action = reg.actions.iter().find(|a| a.id == action_id)?;
    let params = validate_params(reg, action, &rc.params)?;
    Some(Classification {
        action_id,
        params,
        confidence: rc.confidence.clamp(0.0, 1.0),
        ambiguity_reason: rc.ambiguity_reason.filter(|s| !s.trim().is_empty()),
        model,
    })
}

/// Validate an LLM-proposed param map against an action's declared params.
/// Returns a canonicalized map, or None if anything is missing/invalid — the
/// same guards stage-0 enforces, so the LLM can never widen the attack surface.
pub fn validate_params(
    reg: &Registry,
    action: &crate::registry::Action,
    raw: &HashMap<String, String>,
) -> Option<HashMap<String, String>> {
    let mut out = HashMap::new();
    for (name, ptype) in &action.params {
        let val = raw.get(name)?.trim().to_string();
        let canonical = match ptype {
            ParamType::Text { max_len } => {
                if val.is_empty() || val.len() > *max_len {
                    return None;
                }
                val
            }
            ParamType::SiteAlias => {
                let key = normalize(&val);
                if !reg.sites.contains_key(&key) {
                    return None;
                }
                key
            }
            ParamType::AppAlias => {
                let key = normalize(&val);
                if !reg.apps.contains_key(&key) {
                    return None;
                }
                key
            }
            ParamType::Enum { synonym_to_value } => {
                let key = normalize(&val);
                if let Some(canon) = synonym_to_value.get(&key) {
                    canon.clone()
                } else if synonym_to_value.values().any(|v| *v == val) {
                    val // already a canonical value
                } else {
                    return None;
                }
            }
        };
        out.insert(name.clone(), canonical);
    }
    Some(out)
}

/// Pull the first balanced JSON object out of the model's text (tolerates an
/// accidental code fence or stray prose around it).
fn extract_json(raw: &str) -> Option<String> {
    let start = raw.find('{')?;
    let mut depth = 0;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in raw[start..].char_indices() {
        match c {
            '"' if !esc => in_str = !in_str,
            '\\' if in_str => esc = !esc,
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return Some(raw[start..start + i + 1].to_string());
                }
            }
            _ => {}
        }
        if c != '\\' {
            esc = false;
        }
    }
    None
}

/// Render the registry as a compact action catalog for the prompt.
fn build_catalog(reg: &Registry) -> String {
    let mut lines = Vec::new();
    for a in &reg.actions {
        if !a.enabled {
            continue; // disabled actions (e.g. shell.run) aren't offered to the LLM
        }
        // Compact one-liner per action. Enum values ARE listed (the model must
        // pick one); site/app aliases are NOT enumerated inline — the prompt
        // just names the type and the validator enforces membership, which
        // keeps the catalog small enough for fast small-model inference.
        let mut parts = vec![format!("{}: {}", a.id, a.description)];
        if !a.params.is_empty() {
            let ps: Vec<String> = a
                .params
                .iter()
                .map(|(n, t)| match t {
                    ParamType::Text { .. } => format!("{n}=<text>"),
                    ParamType::SiteAlias => format!("{n}=<website name>"),
                    ParamType::AppAlias => format!("{n}=<app name>"),
                    ParamType::Enum { synonym_to_value } => {
                        let vals: HashSet<&String> = synonym_to_value.values().collect();
                        format!("{n}=<{}>", keys(vals.into_iter()).replace(", ", "|"))
                    }
                })
                .collect();
            parts.push(format!("({})", ps.join(", ")));
        }
        lines.push(format!("- {}", parts.join(" ")));
    }
    lines.join("\n")
}

fn keys<'a>(it: impl Iterator<Item = &'a String>) -> String {
    let mut v: Vec<&str> = it.map(|s| s.as_str()).collect();
    v.sort_unstable();
    v.join(", ")
}

// ─── Real backend: Anthropic Messages API ────────────────────────────────────

pub struct AnthropicBackend {
    api_key: String,
    version: String,
    timeout: Duration,
}

impl AnthropicBackend {
    /// From `ANTHROPIC_API_KEY`. Returns None (→ offline-degraded, stage-0
    /// only) when unset.
    pub fn from_env() -> Option<Self> {
        std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()).map(|api_key| Self {
            api_key,
            version: "2023-06-01".into(),
            timeout: Duration::from_secs(10),
        })
    }
}

impl Backend for AnthropicBackend {
    fn complete(&self, model: Model, system: &str, user: &str, _force_json: bool) -> Result<String> {
        let body = serde_json::json!({
            "model": model.id(),
            "max_tokens": 256,
            "temperature": 0.0,
            "system": system,
            "messages": [{ "role": "user", "content": user }],
        });
        let resp: serde_json::Value = ureq::post("https://api.anthropic.com/v1/messages")
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", &self.version)
            .set("content-type", "application/json")
            .timeout(self.timeout)
            .send_json(body)
            .context("anthropic request")?
            .into_json()
            .context("parse anthropic response")?;
        resp["content"][0]["text"]
            .as_str()
            .map(|s| s.to_string())
            .context("no text in anthropic response")
    }
}

// ─── Local backend: Ollama (OpenAI-ish /api/chat on localhost) ───────────────

/// Talks to a local Ollama instance. Fully offline, zero cost. The two-tier
/// routing survives the move: `Model::Sonnet`/`Opus` map to a default and an
/// (optional) larger escalation model — so nothing downstream knows or cares
/// that the model changed. The Anthropic path stays available; this is a
/// sibling `Backend`, not a replacement of the trait.
///
/// Ollama's request/response JSON differs from the Anthropic Messages API
/// (`messages`+`options` in, `message.content` out vs. `content[].text`), so
/// `complete` is a real adapter, not an endpoint rename.
pub struct OllamaBackend {
    url: String,
    /// Stage-1 (default) model, e.g. "qwen2.5:3b".
    model: String,
    /// Stage-2 (escalation) model — defaults to `model` when only one is pulled.
    escalation_model: String,
}

impl OllamaBackend {
    /// Default endpoint, same model for both tiers.
    pub fn new(model: impl Into<String>) -> Self {
        let model = model.into();
        Self {
            // 127.0.0.1, not "localhost": Ollama binds IPv4, and ureq does not
            // do curl-style happy-eyeballs, so "localhost" resolving to ::1
            // first makes the probe spuriously fail.
            url: "http://127.0.0.1:11434".into(),
            escalation_model: model.clone(),
            model,
        }
    }

    /// A fresh single-use agent. We deliberately do NOT pool connections:
    /// Ollama closes idle keep-alive sockets server-side, and a reused-but-
    /// dead socket makes the next request write-then-hang until the read
    /// timeout. A fresh connection per call + `Connection: close` is exactly
    /// what curl does here, and it is 100% reliable in testing.
    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            // With keep_alive the model stays warm, so calls are a few seconds.
            // 60s still covers a one-time cold load of the small model without
            // letting a genuinely stuck call hang the assistant for minutes.
            .timeout_read(Duration::from_secs(60))
            .max_idle_connections(0)
            .build()
    }

    /// Use a larger model for stage-2 escalation (e.g. sonnet-tier work on the
    /// 3B, opus-tier on an 8B). No-op benefit if it's the same string.
    pub fn with_escalation(mut self, model: impl Into<String>) -> Self {
        self.escalation_model = model.into();
        self
    }

    /// Fire a tiny generation in a detached thread to load the model into RAM
    /// and start its keep_alive clock, so the first real command doesn't pay
    /// the cold-load cost. Failures are silent — it's best-effort warming.
    pub fn warm(&self) {
        let url = format!("{}/api/chat", self.url);
        let model = self.model.clone();
        std::thread::spawn(move || {
            let body = serde_json::json!({
                "model": model,
                "messages": [{ "role": "user", "content": "hi" }],
                "stream": false,
                "keep_alive": "30m",
                "options": { "num_predict": 1 },
            });
            let _ = Self::agent().post(&url).set("Connection", "close").send_json(body);
        });
    }

    /// True if the server answers — used to auto-select this backend.
    pub fn is_up(&self) -> bool {
        Self::agent()
            .get(&format!("{}/api/tags", self.url))
            .set("Connection", "close")
            .call()
            .is_ok()
    }

    fn model_for(&self, m: Model) -> &str {
        match m {
            Model::Sonnet => &self.model,
            Model::Opus => &self.escalation_model,
        }
    }
}

impl OllamaBackend {
    fn distinct(&self) -> bool {
        self.model != self.escalation_model
    }
}

impl Backend for OllamaBackend {
    fn complete(&self, model: Model, system: &str, user: &str, force_json: bool) -> Result<String> {
        // Adapter: Anthropic's (system, single user turn) -> Ollama's chat
        // message array with a system role. For classification we set
        // `format:"json"` to constrain output; for chat we leave it off so the
        // model replies in natural prose. `stream:false` returns one complete
        // message; temperature 0 for classification, a touch higher for chat.
        let mut body = serde_json::json!({
            "model": self.model_for(model),
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "stream": false,
            // Pin the model in RAM for 30 min. Without this Ollama unloads it
            // after ~5 min idle, so the next command pays a full cold reload
            // (~2 GB off disk) — that was the 3-4 minute "thinking" latency.
            "keep_alive": "30m",
            "options": {
                // Classification wants determinism; chat wants variety (so it
                // doesn't tell the same joke twice).
                "temperature": if force_json { 0.0 } else { 0.95 },
                "top_p": if force_json { 1.0 } else { 0.95 },
                // Cap output. A small model can otherwise run away until the
                // context fills — on CPU that blows past the read timeout.
                "num_predict": if force_json { 128 } else { 200 },
            },
        });
        if force_json {
            body["format"] = serde_json::Value::String("json".into());
        }
        let url = format!("{}/api/chat", self.url);

        // Retry transport errors with growing backoff. Under memory pressure a
        // busy Ollama can drop a new connection (os error 10060); giving it a
        // second or two to finish its current work and free its listener
        // clears the transient failure. Non-transport errors aren't retried.
        const BACKOFF_MS: &[u64] = &[0, 1500];
        let mut last_err = None;
        for (attempt, delay) in BACKOFF_MS.iter().enumerate() {
            if *delay > 0 {
                std::thread::sleep(Duration::from_millis(*delay));
            }
            match Self::agent().post(&url).set("Connection", "close").send_json(body.clone()) {
                Ok(resp) => {
                    let json: serde_json::Value =
                        resp.into_json().context("parse ollama response")?;
                    // Shape: {"message": {"role":"assistant","content":"..."}, ...}
                    return json["message"]["content"]
                        .as_str()
                        .map(|s| s.to_string())
                        .context("no message.content in ollama response");
                }
                Err(e) => {
                    if attempt + 1 < BACKOFF_MS.len() {
                        eprintln!("[llm] ollama transport error (attempt {}/{}, retrying): {e}", attempt + 1, BACKOFF_MS.len());
                    }
                    last_err = Some(e);
                }
            }
        }
        Err(anyhow::Error::new(last_err.unwrap()).context("ollama request"))
    }

    fn has_distinct_escalation(&self) -> bool {
        self.distinct()
    }
}

// ─── Mock backend: canned classifications for offline testing ────────────────

/// A rule: if all `keywords` appear in the (lowercased) transcript, return
/// `json`. First matching rule wins; falls back to a null classification.
pub struct MockRule {
    pub keywords: Vec<&'static str>,
    pub json: &'static str,
}

pub struct MockBackend {
    rules: Vec<MockRule>,
    /// Models that should error (to exercise fail-closed paths).
    fail: HashSet<Model>,
}

impl MockBackend {
    pub fn new(rules: Vec<MockRule>) -> Self {
        Self { rules, fail: HashSet::new() }
    }

    /// A small built-in rule set covering the demo utterances from live logs.
    pub fn demo() -> Self {
        Self::new(vec![
            MockRule {
                keywords: vec!["close", "edge"],
                json: r#"{"action_id":"app.close","params":{"app":"microsoft edge"},"confidence":0.9,"ambiguity_reason":null}"#,
            },
            MockRule {
                keywords: vec!["close", "youtube"],
                // No app alias for a browser tab -> model should decline.
                json: r#"{"action_id":null,"params":{},"confidence":0.3,"ambiguity_reason":"no action closes a single browser tab"}"#,
            },
            MockRule {
                keywords: vec!["delete", "everything"],
                json: r#"{"action_id":null,"params":{},"confidence":0.4,"ambiguity_reason":"destructive and unspecified target"}"#,
            },
            MockRule {
                keywords: vec!["what", "time"],
                json: r#"{"action_id":null,"params":{},"confidence":0.8,"ambiguity_reason":null}"#,
            },
        ])
    }

    pub fn with_failing(mut self, m: Model) -> Self {
        self.fail.insert(m);
        self
    }
}

impl Backend for MockBackend {
    fn complete(&self, model: Model, _system: &str, user: &str, _force_json: bool) -> Result<String> {
        if self.fail.contains(&model) {
            bail!("mock backend: forced failure for {:?}", model);
        }
        let transcript = extract_utterance(user).unwrap_or_default();
        let hay = transcript.to_lowercase();
        for rule in &self.rules {
            if rule.keywords.iter().all(|k| hay.contains(k)) {
                return Ok(rule.json.to_string());
            }
        }
        Ok(r#"{"action_id":null,"params":{},"confidence":0.5,"ambiguity_reason":"no matching mock rule"}"#.to_string())
    }
}

/// Recover the transcript from a user prompt built by `Router::classify`.
fn extract_utterance(user: &str) -> Option<String> {
    let line = user.lines().next()?;
    let start = line.find('"')? + 1;
    let end = line.rfind('"')?;
    (end > start).then(|| line[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn registry() -> Registry {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../registry/actions.toml");
        Registry::load(&root).unwrap()
    }

    fn router(backend: MockBackend) -> (Router, Registry) {
        let reg = registry();
        let r = Router::new(Box::new(backend), &reg);
        (r, reg)
    }

    #[test]
    fn resolves_close_edge_to_app_close() {
        let (r, reg) = router(MockBackend::demo());
        let c = r.classify(&reg, "close all the tabs on Microsoft Edge").unwrap().unwrap();
        assert_eq!(c.action_id, "app.close");
        assert_eq!(c.params["app"], "microsoft edge");
    }

    #[test]
    fn null_classification_is_none() {
        let (r, reg) = router(MockBackend::demo());
        assert!(r.classify(&reg, "close youtube now").unwrap().is_none());
    }

    #[test]
    fn unknown_action_id_fails_closed() {
        let backend = MockBackend::new(vec![MockRule {
            keywords: vec!["frobnicate"],
            json: r#"{"action_id":"does.not.exist","params":{},"confidence":0.99,"ambiguity_reason":null}"#,
        }]);
        let (r, reg) = router(backend);
        assert!(r.classify(&reg, "frobnicate the widget").unwrap().is_none());
    }

    #[test]
    fn bad_params_fail_closed() {
        // Claims youtube.search but omits the required query param.
        let backend = MockBackend::new(vec![MockRule {
            keywords: vec!["play"],
            json: r#"{"action_id":"youtube.search","params":{},"confidence":0.95,"ambiguity_reason":null}"#,
        }]);
        let (r, reg) = router(backend);
        assert!(r.classify(&reg, "play something").unwrap().is_none());
    }

    #[test]
    fn low_confidence_escalates_to_opus() {
        // Sonnet: low-confidence youtube.search. Opus: confident app.launch.
        let backend = MockBackend::new(vec![
            MockRule {
                keywords: vec!["notepad"],
                json: r#"{"action_id":"youtube.search","params":{"query":"notepad"},"confidence":0.4,"ambiguity_reason":null}"#,
            },
        ]);
        // Both models hit the same single rule here, so confidence stays 0.4
        // -> escalation runs and returns an opus-tagged result.
        let (r, reg) = router(backend);
        let c = r.classify(&reg, "open notepad").unwrap().unwrap();
        assert_eq!(c.model, Model::Opus, "low confidence must escalate");
    }

    #[test]
    fn same_model_skips_escalation() {
        // A backend with no distinct escalation must never make a 2nd call,
        // even on a low-confidence stage-1 result.
        struct CountingBackend {
            calls: std::sync::atomic::AtomicUsize,
        }
        impl Backend for CountingBackend {
            fn complete(&self, _m: Model, _s: &str, _u: &str, _j: bool) -> Result<String> {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(r#"{"action_id":"app.launch","params":{"app":"notepad"},"confidence":0.3,"ambiguity_reason":"vague"}"#.into())
            }
            fn has_distinct_escalation(&self) -> bool {
                false
            }
        }
        let reg = registry();
        let backend = CountingBackend { calls: std::sync::atomic::AtomicUsize::new(0) };
        let router = Router::new(Box::new(backend), &reg);
        let c = router.classify(&reg, "open notepad").unwrap().unwrap();
        assert_eq!(c.model, Model::Sonnet, "must not escalate");
        // Can't read the count through the boxed trait object, but the model
        // tag proves stage-2 didn't run.
    }

    #[test]
    fn opus_failure_falls_back_to_sonnet() {
        let backend = MockBackend::new(vec![MockRule {
            keywords: vec!["notepad"],
            json: r#"{"action_id":"app.launch","params":{"app":"notepad"},"confidence":0.4,"ambiguity_reason":null}"#,
        }])
        .with_failing(Model::Opus);
        let (r, reg) = router(backend);
        // Escalation fires (0.4) but opus errors -> stage-1 result survives.
        let c = r.classify(&reg, "open notepad").unwrap().unwrap();
        assert_eq!(c.action_id, "app.launch");
        assert_eq!(c.model, Model::Sonnet);
    }

    #[test]
    fn below_confidence_floor_declines() {
        // conf 0.10 resolves to a valid action but is below the floor -> None.
        let backend = MockBackend::new(vec![MockRule {
            keywords: vec!["tab"],
            json: r#"{"action_id":"web.close","params":{"site":"youtube"},"confidence":0.1,"ambiguity_reason":null}"#,
        }]);
        let (r, reg) = router(backend);
        assert!(r.classify(&reg, "youtube tab").unwrap().is_none());
    }

    #[test]
    fn at_floor_is_kept() {
        // conf 0.30 is exactly the floor (strict <) -> kept.
        let backend = MockBackend::new(vec![MockRule {
            keywords: vec!["notepad"],
            json: r#"{"action_id":"app.launch","params":{"app":"notepad"},"confidence":0.3,"ambiguity_reason":null}"#,
        }]);
        let (r, reg) = router(backend);
        assert!(r.classify(&reg, "open notepad").unwrap().is_some());
    }

    #[test]
    fn transport_error_is_err_not_none() {
        let backend = MockBackend::demo().with_failing(Model::Sonnet);
        let (r, reg) = router(backend);
        assert!(r.classify(&reg, "close edge").is_err(), "sonnet failure must surface as llm_error");
    }

    #[test]
    fn catalog_excludes_disabled_actions() {
        let reg = registry();
        let cat = build_catalog(&reg);
        assert!(!cat.contains("shell.run"), "disabled actions must not be offered to the LLM");
        assert!(cat.contains("youtube.search"));
    }

    #[test]
    fn extract_json_tolerates_code_fence() {
        let raw = "```json\n{\"action_id\":null,\"params\":{},\"confidence\":0.5}\n```";
        assert!(extract_json(raw).unwrap().starts_with('{'));
    }
}
