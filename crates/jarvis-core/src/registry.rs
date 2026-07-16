//! Tool registry: loads `registry/actions.toml`, validates it against the
//! REGISTRY.md invariants, and provides stage-0 grammar matching
//! (transcript -> action + typed params, no LLM, no network).
//!
//! Invariants enforced here (not convention — load fails otherwise):
//! - every template slot is a declared param, with the right encoder for its
//!   type (string -> urlencode, site_alias -> site_url)
//! - string params carry a max_len cap
//! - site/app alias keys are disjoint (both appear after "open ...")
//! - site values are bare https origins (no query strings / deep links)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    SafeAuto,
    NeedsConfirm,
    DenyByDefault,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::SafeAuto => "SAFE-AUTO",
            Tier::NeedsConfirm => "NEEDS-CONFIRM",
            Tier::DenyByDefault => "DENY-BY-DEFAULT",
        }
    }
}

impl std::str::FromStr for Tier {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "safe-auto" => Ok(Self::SafeAuto),
            "needs-confirm" => Ok(Self::NeedsConfirm),
            "deny-by-default" => Ok(Self::DenyByDefault),
            other => bail!("unknown tier '{other}'"),
        }
    }
}

// ---- raw TOML shapes ----

#[derive(Deserialize)]
struct RegistryFile {
    #[serde(default)]
    sites: HashMap<String, String>,
    #[serde(default)]
    apps: HashMap<String, Vec<String>>,
    /// App alias -> OS process base name, for graceful close (app.close).
    /// Only apps listed here are closable; deliberately excludes things like
    /// explorer (closing it kills the shell).
    #[serde(default)]
    close_process: HashMap<String, String>,
    /// Site alias -> browser-tab title keyword, for closing a website tab
    /// (web.close).
    #[serde(default)]
    web_title: HashMap<String, String>,
    action: Vec<ActionDef>,
}

#[derive(Deserialize)]
struct ActionDef {
    id: String,
    tier: String,
    description: String,
    patterns: Vec<String>,
    #[serde(default)]
    params: Vec<ParamDef>,
    exec: ExecDef,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct ParamDef {
    name: String,
    #[serde(rename = "type")]
    ptype: String,
    #[serde(default)]
    max_len: Option<usize>,
    /// enum type only: canonical value -> spoken synonyms
    #[serde(default)]
    values: Option<HashMap<String, Vec<String>>>,
}

#[derive(Deserialize)]
struct ExecDef {
    kind: String,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    script: Option<String>,
}

// ---- compiled registry ----

#[derive(Clone, Debug)]
pub enum ParamType {
    /// freeform text with a hard length cap
    Text { max_len: usize },
    /// key into the [sites] map
    SiteAlias,
    /// key into the [apps] map
    AppAlias,
    /// canonical value chosen via spoken synonyms
    Enum { synonym_to_value: HashMap<String, String> },
}

#[derive(Clone, Debug)]
pub enum Exec {
    OpenUrl { template: String },
    LaunchApp,
    MediaKey { key_template: String },
    PsTemplate { script: String },
    DialogOnly,
    /// Search YouTube for the `query` param and open the top result's watch
    /// page (autoplays). Falls back to the results page if the ID can't be
    /// scraped. No API key — reads the public results page.
    YoutubePlay,
    /// Move the resolved `path` param to the Recycle Bin (DENY-BY-DEFAULT).
    /// Expects a pre-resolved absolute, profile-jailed path; re-validates.
    RecycleFile,
    /// Fetch RSS headlines and speak a news briefing.
    ReadNews,
    /// Gather system + project status and speak it.
    AssistantUpdate,
    /// Fetch the weather (wttr.in, no key) and speak it.
    ReadWeather,
}

pub struct Action {
    pub id: String,
    pub tier: Tier,
    pub description: String,
    pub enabled: bool,
    pub exec: Exec,
    pub params: HashMap<String, ParamType>,
    regexes: Vec<Regex>,
}

pub struct Registry {
    pub actions: Vec<Action>,
    pub sites: HashMap<String, String>,
    pub apps: HashMap<String, Vec<String>>,
    pub close_process: HashMap<String, String>,
    pub web_title: HashMap<String, String>,
    /// Directory holding ps_template scripts (`<workspace>/scripts`).
    pub scripts_dir: PathBuf,
}

/// A stage-0 grammar hit: which action, with which extracted params.
/// `params` values are canonical (enum values resolved, aliases still keys —
/// resolution to URL/argv happens in exec, where the maps live).
pub struct IntentMatch<'r> {
    pub action: &'r Action,
    pub params: HashMap<String, String>,
}

impl Registry {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read registry {}", path.display()))?;
        let raw: RegistryFile = toml::from_str(&text).context("parse actions.toml")?;

        // Site values: bare https origins only (action-planner rule #4).
        for (k, v) in &raw.sites {
            if !v.starts_with("https://") || v.contains('?') || v.contains('=') {
                bail!("site alias '{k}' is not a bare https origin: {v}");
            }
        }
        // "open {site}" and "open {app}" must be unambiguous.
        if let Some(dup) = raw.sites.keys().find(|k| raw.apps.contains_key(*k)) {
            bail!("alias '{dup}' appears in both [sites] and [apps]");
        }

        // close_process keys must be real app aliases (target resolves through
        // the reviewed allowlist — action-planner rule #3).
        for k in raw.close_process.keys() {
            if !raw.apps.contains_key(k) {
                bail!("close_process alias '{k}' is not in [apps]");
            }
        }
        // web_title keys must be real site aliases.
        for k in raw.web_title.keys() {
            if !raw.sites.contains_key(k) {
                bail!("web_title alias '{k}' is not in [sites]");
            }
        }

        let mut actions = Vec::new();
        for def in &raw.action {
            actions.push(compile_action(def, &raw)?);
        }
        // registry/actions.toml -> <workspace>/scripts
        let scripts_dir = path
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("scripts"))
            .unwrap_or_else(|| PathBuf::from("scripts"));
        Ok(Self {
            actions,
            sites: raw.sites,
            apps: raw.apps,
            close_process: raw.close_process,
            web_title: raw.web_title,
            scripts_dir,
        })
    }

    /// Stage-0 grammar match. Actions are tried in file order; the first
    /// pattern that consumes the whole normalized transcript wins.
    /// A leading wake phrase is stripped first — one-breath utterances
    /// ("hey jarvis open youtube...") carry it into the transcript.
    pub fn match_transcript(&self, transcript: &str) -> Option<IntentMatch<'_>> {
        let text = strip_wake_prefix(&normalize(transcript));
        for action in &self.actions {
            for re in &action.regexes {
                if let Some(caps) = re.captures(&text) {
                    let mut params = HashMap::new();
                    let mut ok = true;
                    for (name, ptype) in &action.params {
                        let Some(m) = caps.name(name) else {
                            ok = false;
                            break;
                        };
                        let raw_val = m.as_str().trim().to_string();
                        let val = match ptype {
                            ParamType::Text { max_len } => {
                                if raw_val.is_empty() || raw_val.len() > *max_len {
                                    ok = false;
                                    break;
                                }
                                raw_val
                            }
                            // alias membership was enforced by the alternation regex
                            ParamType::SiteAlias | ParamType::AppAlias => raw_val,
                            ParamType::Enum { synonym_to_value } => {
                                match synonym_to_value.get(&raw_val) {
                                    Some(v) => v.clone(),
                                    None => {
                                        ok = false;
                                        break;
                                    }
                                }
                            }
                        };
                        params.insert(name.clone(), val);
                    }
                    if ok {
                        return Some(IntentMatch { action, params });
                    }
                }
            }
        }
        None
    }

    /// Bag-of-words fallback for close/quit commands the strict grammar missed
    /// because the ASR garbled word order ("spotify closed spotify") or dropped
    /// glue words. If the utterance contains a close verb AND a known app or
    /// site name anywhere, match the corresponding close action. Both close
    /// actions are needs-confirm, so a false hit only ever asks "Close X?".
    pub fn match_close_fallback(&self, transcript: &str) -> Option<IntentMatch<'_>> {
        let text = strip_wake_prefix(&normalize(transcript));
        let words: Vec<&str> = text.split_whitespace().collect();
        let has_close = words
            .iter()
            .any(|w| matches!(*w, "close" | "quit" | "exit" | "shut" | "closed"));
        if !has_close {
            return None;
        }
        // `name` (possibly multi-word) appears as a contiguous run in the text.
        let contains = |name: &str| -> bool {
            let want: Vec<&str> = name.split_whitespace().collect();
            words.windows(want.len().max(1)).any(|win| win == want.as_slice())
        };
        // Prefer a real app (Spotify, Edge, …); fall back to a website tab.
        // Longest alias first so "microsoft edge" beats "edge".
        let mut app_aliases: Vec<&String> = self.close_process.keys().collect();
        app_aliases.sort_by_key(|a| std::cmp::Reverse(a.len()));
        for alias in app_aliases {
            if contains(alias) {
                let action = self.actions.iter().find(|a| a.id == "app.close")?;
                let mut params = HashMap::new();
                params.insert("app".to_string(), alias.clone());
                return Some(IntentMatch { action, params });
            }
        }
        let mut site_aliases: Vec<&String> = self.web_title.keys().collect();
        site_aliases.sort_by_key(|a| std::cmp::Reverse(a.len()));
        for alias in site_aliases {
            if contains(alias) {
                let action = self.actions.iter().find(|a| a.id == "web.close")?;
                let mut params = HashMap::new();
                params.insert("site".to_string(), alias.clone());
                return Some(IntentMatch { action, params });
            }
        }
        None
    }
}

/// Lowercase and strip the sentence punctuation the ASR adds — but only at
/// word boundaries, so filenames survive: "beats." -> "beats" while
/// "report.pdf" keeps its dot. Apostrophes/quotes are stripped everywhere
/// ("what's" -> "whats").
pub fn normalize(transcript: &str) -> String {
    let lower = transcript.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut s = String::with_capacity(lower.len());
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '"' | '\'' => {}
            '.' | ',' | '!' | '?' | ';' | ':' => {
                let at_word_end = chars.get(i + 1).is_none_or(|n| n.is_whitespace());
                if !at_word_end {
                    s.push(c);
                }
            }
            _ => s.push(c),
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Drop a leading wake phrase ("hey nyxa" / "nyxa" / "hey jarvis" / "jarvis")
/// from normalized text. Returns "" for a bare wake phrase — callers treat that
/// as "still listening". Nyxa is the product name; jarvis remains because the
/// pretrained wake-word model still triggers on it.
pub fn strip_wake_prefix(text: &str) -> String {
    for prefix in ["hey nyxa", "nyxa", "hey jarvis", "jarvis"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            if rest.is_empty() || rest.starts_with(' ') {
                return rest.trim_start().to_string();
            }
        }
    }
    text.to_string()
}

fn compile_action(def: &ActionDef, raw: &RegistryFile) -> Result<Action> {
    let tier: Tier = def.tier.parse().with_context(|| format!("action {}", def.id))?;

    // Params
    let mut params: HashMap<String, ParamType> = HashMap::new();
    for p in &def.params {
        let ptype = match p.ptype.as_str() {
            "string" | "path" => ParamType::Text {
                // REGISTRY.md cross-cutting rule #1: caps are mandatory.
                max_len: p
                    .max_len
                    .with_context(|| format!("{}: param {} needs max_len", def.id, p.name))?,
            },
            "site_alias" => ParamType::SiteAlias,
            "app_alias" => ParamType::AppAlias,
            "enum" => {
                let values = p
                    .values
                    .as_ref()
                    .with_context(|| format!("{}: enum param {} needs values", def.id, p.name))?;
                let mut synonym_to_value = HashMap::new();
                for (canonical, synonyms) in values {
                    for syn in synonyms {
                        synonym_to_value.insert(normalize(syn), canonical.clone());
                    }
                }
                ParamType::Enum { synonym_to_value }
            }
            other => bail!("{}: unknown param type '{other}'", def.id),
        };
        params.insert(p.name.clone(), ptype);
    }

    // Exec
    let exec = match def.exec.kind.as_str() {
        "open_url" => {
            let template = def
                .exec
                .template
                .clone()
                .with_context(|| format!("{}: open_url needs template", def.id))?;
            validate_template(&def.id, &template, &params)?;
            Exec::OpenUrl { template }
        }
        "launch_app" => {
            if !matches!(params.get("app"), Some(ParamType::AppAlias)) {
                bail!("{}: launch_app needs an app_alias param named 'app'", def.id);
            }
            Exec::LaunchApp
        }
        "media_key" => Exec::MediaKey {
            key_template: def
                .exec
                .key
                .clone()
                .with_context(|| format!("{}: media_key needs key", def.id))?,
        },
        "ps_template" => Exec::PsTemplate {
            script: def
                .exec
                .script
                .clone()
                .with_context(|| format!("{}: ps_template needs script", def.id))?,
        },
        "dialog_only" => Exec::DialogOnly,
        "youtube_play" => {
            if !matches!(params.get("query"), Some(ParamType::Text { .. })) {
                bail!("{}: youtube_play needs a text param named 'query'", def.id);
            }
            Exec::YoutubePlay
        }
        "recycle_file" => {
            if !matches!(params.get("path"), Some(ParamType::Text { .. })) {
                bail!("{}: recycle_file needs a path param named 'path'", def.id);
            }
            Exec::RecycleFile
        }
        "read_news" => Exec::ReadNews,
        "assistant_update" => Exec::AssistantUpdate,
        "read_weather" => Exec::ReadWeather,
        other => bail!("{}: unknown exec kind '{other}'", def.id),
    };

    // Patterns -> anchored case-sensitive regexes over normalized text.
    let mut regexes = Vec::new();
    for pat in &def.patterns {
        let re_src = expand_pattern(&def.id, pat, &params, raw)?;
        regexes.push(
            Regex::new(&format!("^{re_src}$"))
                .with_context(|| format!("{}: bad pattern '{pat}'", def.id))?,
        );
    }

    Ok(Action {
        id: def.id.clone(),
        tier,
        description: def.description.clone(),
        enabled: def.enabled,
        exec,
        params,
        regexes,
    })
}

/// Replace `{slot}` tokens with named capture groups. Alias/enum slots become
/// an alternation over their known keys/synonyms (longest first, so "next
/// track" wins over "next"); text slots become a lazy catch-all.
fn expand_pattern(
    id: &str,
    pattern: &str,
    params: &HashMap<String, ParamType>,
    raw: &RegistryFile,
) -> Result<String> {
    let mut out = String::new();
    let mut rest = pattern;
    while let Some(start) = rest.find('{') {
        let end = rest[start..]
            .find('}')
            .map(|e| start + e)
            .with_context(|| format!("{id}: unclosed {{ in pattern"))?;
        out.push_str(&rest[..start]);
        let name = &rest[start + 1..end];
        let ptype = params
            .get(name)
            .with_context(|| format!("{id}: pattern slot {{{name}}} has no param"))?;
        let group = match ptype {
            ParamType::Text { .. } => "(?s:.+?)".to_string(),
            ParamType::SiteAlias => alternation(raw.sites.keys()),
            ParamType::AppAlias => alternation(raw.apps.keys()),
            ParamType::Enum { synonym_to_value } => alternation(synonym_to_value.keys()),
        };
        out.push_str(&format!("(?P<{name}>{group})"));
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn alternation<'a>(keys: impl Iterator<Item = &'a String>) -> String {
    let mut keys: Vec<&String> = keys.collect();
    keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
    keys.iter()
        .map(|k| regex::escape(&normalize(k)))
        .collect::<Vec<_>>()
        .join("|")
}

/// Template slots must be declared params with the right encoder for their
/// type — a string slot without |urlencode in a URL is a load error.
fn validate_template(id: &str, template: &str, params: &HashMap<String, ParamType>) -> Result<()> {
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let end = rest[start..]
            .find('}')
            .map(|e| start + e)
            .with_context(|| format!("{id}: unclosed {{ in template"))?;
        let token = &rest[start + 1..end];
        let (name, encoder) = match token.split_once('|') {
            Some((n, e)) => (n, Some(e)),
            None => (token, None),
        };
        let ptype = params
            .get(name)
            .with_context(|| format!("{id}: template slot {{{name}}} has no param"))?;
        match (ptype, encoder) {
            (ParamType::Text { .. }, Some("urlencode")) => {}
            // `uri`: percent-encode with space -> %20, for non-http schemes
            // like spotify: that reject '+'.
            (ParamType::Text { .. }, Some("uri")) => {}
            (ParamType::SiteAlias, Some("site_url")) => {}
            (pt, enc) => bail!("{id}: slot {{{token}}} has type {pt:?} but encoder {enc:?}"),
        }
        rest = &rest[end + 1..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn registry() -> Registry {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../registry/actions.toml");
        Registry::load(&root).expect("registry loads")
    }

    #[test]
    fn registry_loads_and_validates() {
        let r = registry();
        assert!(r.actions.len() >= 16);
    }

    #[test]
    fn youtube_search_reference_case() {
        let r = registry();
        let m = r
            .match_transcript("Open YouTube and search for lo-fi study beats.")
            .expect("matches");
        assert_eq!(m.action.id, "youtube.search");
        assert_eq!(m.params["query"], "lo-fi study beats");
        assert_eq!(m.action.tier, Tier::SafeAuto);
    }

    #[test]
    fn youtube_beats_generic_web_search() {
        let r = registry();
        let m = r.match_transcript("search youtube for rust tutorials").unwrap();
        assert_eq!(m.action.id, "youtube.search");
    }

    #[test]
    fn open_site_vs_open_app_disambiguates() {
        let r = registry();
        assert_eq!(r.match_transcript("open gmail").unwrap().action.id, "browser.open_site");
        assert_eq!(r.match_transcript("Open Notepad.").unwrap().action.id, "app.launch");
    }

    #[test]
    fn enum_longest_synonym_wins() {
        let r = registry();
        let m = r.match_transcript("next track").unwrap();
        assert_eq!(m.action.id, "media.control");
        assert_eq!(m.params["control"], "next");
    }

    #[test]
    fn delete_is_deny_tier() {
        let r = registry();
        let m = r.match_transcript("delete old-notes.txt").unwrap();
        assert_eq!(m.action.id, "file.delete");
        assert_eq!(m.action.tier, Tier::DenyByDefault);
    }

    #[test]
    fn normalize_keeps_filename_dots_strips_sentence_punct() {
        assert_eq!(
            normalize("Delete old-notes.txt, please!"),
            "delete old-notes.txt please"
        );
        assert_eq!(normalize("What's the weather?"), "whats the weather");
    }

    #[test]
    fn one_breath_wake_prefix_is_stripped() {
        let r = registry();
        let m = r
            .match_transcript("Hey Jarvis, open YouTube and search for Kariminati.")
            .expect("matches after prefix strip");
        assert_eq!(m.action.id, "youtube.search");
        assert_eq!(m.params["query"], "kariminati");
        assert_eq!(strip_wake_prefix(&normalize("Hey Jarvis.")), "");
    }

    #[test]
    fn path_param_preserves_extension() {
        let r = registry();
        let m = r.match_transcript("Move report.pdf to documents.").unwrap();
        assert_eq!(m.params["src"], "report.pdf");
        assert_eq!(m.params["dst"], "documents");
    }

    #[test]
    fn shell_run_is_disabled() {
        let r = registry();
        let m = r.match_transcript("run command format c").unwrap();
        assert_eq!(m.action.id, "shell.run");
        assert!(!m.action.enabled);
    }

    #[test]
    fn oversized_query_is_rejected() {
        let r = registry();
        let long = format!("open youtube and search for {}", "a".repeat(200));
        assert!(r.match_transcript(&long).is_none());
    }

    #[test]
    fn unknown_utterance_no_match() {
        let r = registry();
        assert!(r.match_transcript("tell me a joke about rust").is_none());
    }
}
