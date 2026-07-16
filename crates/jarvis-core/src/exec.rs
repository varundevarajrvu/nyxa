//! Executors for registry actions. Only the closed set of kinds from
//! REGISTRY.md exists here; params arrive pre-validated from stage-0 matching
//! and flow through per-type encoders — transcript text NEVER reaches shell
//! text (invariant #2).
//!
//! J2 scope: SAFE-AUTO kinds (open_url, launch_app, media_key) are real.
//! ps_template and dialog_only return NotImplemented until the J5
//! confirmation flows exist — the tier gate in the caller stops those
//! utterances earlier anyway.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::registry::{Exec, IntentMatch, Registry};

/// What executing (or declining to execute) an intent produced.
#[derive(Debug)]
pub enum Outcome {
    /// Ran; the string is a human-readable summary for console/toast/audit.
    Executed(String),
    /// Ran and produced a spoken response (news, updates) to read aloud.
    Spoken(String),
    /// Dry-run: what WOULD have run.
    DryRun(String),
    /// Executor for this kind isn't built yet (J5 tiers).
    NotImplemented(&'static str),
}

/// Execute a stage-0 match. The caller has already applied the tier gate and
/// cooldown; this function only knows how to run things.
pub fn execute(reg: &Registry, m: &IntentMatch, dry_run: bool) -> Result<Outcome> {
    match &m.action.exec {
        Exec::OpenUrl { template } => {
            let url = fill_url_template(reg, template, &m.params)?;
            if dry_run {
                return Ok(Outcome::DryRun(format!("open_url {url}")));
            }
            open_url(&url)?;
            Ok(Outcome::Executed(format!("opened {url}")))
        }
        Exec::LaunchApp => {
            let alias = m.params.get("app").context("missing app param")?;
            let argv = reg
                .apps
                .get(alias)
                .with_context(|| format!("app alias '{alias}' not in allowlist"))?;
            if dry_run {
                return Ok(Outcome::DryRun(format!("launch_app {argv:?}")));
            }
            // Fixed argv from the reviewed allowlist — nothing user-derived
            // is appended, ever (action-planner cross-cutting rule #3).
            std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .spawn()
                .with_context(|| format!("launch {argv:?}"))?;
            Ok(Outcome::Executed(format!("launched {alias}")))
        }
        Exec::MediaKey { key_template } => {
            let key = fill_plain_slot(key_template, &m.params)?;
            let (vk, taps) = match key.as_str() {
                "playpause" => (0xB3u16, 1),
                "next" => (0xB0, 1),
                "prev" => (0xB1, 1),
                "mute" => (0xAD, 1),
                // ±2 steps per utterance, bounded (registry guard)
                "volume_up" => (0xAF, 2),
                "volume_down" => (0xAE, 2),
                other => bail!("unknown media key '{other}'"),
            };
            if dry_run {
                return Ok(Outcome::DryRun(format!("media_key {key} x{taps}")));
            }
            for _ in 0..taps {
                send_vk(vk);
            }
            Ok(Outcome::Executed(format!("media key {key}")))
        }
        Exec::PsTemplate { script } => {
            let script_path = reg.scripts_dir.join(script);
            // app.close: `app` alias -> process name (graceful WM_CLOSE).
            if let Some(alias) = m.params.get("app") {
                let proc = match reg.close_process.get(alias) {
                    Some(p) => p,
                    None => return Ok(Outcome::NotImplemented("that app isn't closable")),
                };
                if !script_path.exists() {
                    bail!("close script missing: {}", script_path.display());
                }
                if dry_run {
                    return Ok(Outcome::DryRun(format!("ps {script} -ProcessName {proc}")));
                }
                return run_ps_close(
                    &script_path,
                    &["-ProcessName", proc],
                    alias,
                    format!("closed {alias}"),
                    format!("{alias} wasn't running"),
                );
            }
            // spotify.play: `query` -> UI-Automation invoke of the matching
            // "Play <song>" button in the Spotify app. Query passed as a
            // discrete -Query argument, never spliced.
            if let Some(query) = m.params.get("query") {
                if !script_path.exists() {
                    bail!("spotify script missing: {}", script_path.display());
                }
                if dry_run {
                    return Ok(Outcome::DryRun(format!("ps {script} -Query {query:?}")));
                }
                return run_ps_close(
                    &script_path,
                    &["-Query", query],
                    query,
                    format!("playing '{query}' on Spotify"),
                    format!("opened Spotify search for '{query}' (no matching play button)"),
                );
            }
            // web.close: `site` alias -> browser-tab title keyword (Ctrl+W).
            if let Some(alias) = m.params.get("site") {
                let title = match reg.web_title.get(alias) {
                    Some(t) => t,
                    None => return Ok(Outcome::NotImplemented("no window title mapped for that site")),
                };
                if !script_path.exists() {
                    bail!("close script missing: {}", script_path.display());
                }
                if dry_run {
                    return Ok(Outcome::DryRun(format!("ps {script} -TitleMatch {title}")));
                }
                return run_ps_close(
                    &script_path,
                    &["-TitleMatch", title],
                    alias,
                    format!("closed the {alias} tab"),
                    format!("no open {alias} tab found"),
                );
            }
            // Path-param scripts (file ops) need canonicalization/jail guards
            // from a later milestone and are deliberately refused here.
            Ok(Outcome::NotImplemented(
                "this ps_template action isn't wired yet (needs its guards)",
            ))
        }
        Exec::DialogOnly => {
            Ok(Outcome::NotImplemented("confirmation dialog lands in J5"))
        }
        Exec::YoutubePlay => {
            let query = m.params.get("query").context("missing query param")?;
            let results = format!(
                "https://www.youtube.com/results?search_query={}",
                urlencode(query)
            );
            if dry_run {
                return Ok(Outcome::DryRun(format!("youtube_play {query:?}")));
            }
            // Read the public results page for the top video id (no API key).
            match first_video_id(&results) {
                Some(id) => {
                    open_url(&format!("https://www.youtube.com/watch?v={id}"))?;
                    Ok(Outcome::Executed(format!("playing '{query}' on YouTube")))
                }
                None => {
                    // Graceful fallback: open the search page, user picks.
                    open_url(&results)?;
                    Ok(Outcome::Executed(format!(
                        "opened YouTube search for '{query}' (couldn't autoplay top result)"
                    )))
                }
            }
        }
        Exec::ReadNews => {
            if dry_run {
                return Ok(Outcome::DryRun("read_news".into()));
            }
            Ok(Outcome::Spoken(fetch_news()))
        }
        Exec::ReadWeather => {
            if dry_run {
                return Ok(Outcome::DryRun("read_weather".into()));
            }
            Ok(Outcome::Spoken(fetch_weather()))
        }
        Exec::AssistantUpdate => {
            let script = reg.scripts_dir.join("assistant_update.ps1");
            if !script.exists() {
                bail!("assistant_update.ps1 missing: {}", script.display());
            }
            if dry_run {
                return Ok(Outcome::DryRun("assistant_update".into()));
            }
            Ok(Outcome::Spoken(run_update_script(&script)))
        }
        Exec::RecycleFile => {
            let path = m.params.get("path").context("missing path param")?;
            // Defense in depth: the DENY flow already resolved + jailed this,
            // but never trust a bare param — re-check it's an absolute file
            // under the user profile before deleting anything.
            let p = Path::new(path);
            if !p.is_absolute() || !p.is_file() {
                bail!("delete target is not an existing absolute file: {path}");
            }
            if let Ok(profile) = std::env::var("USERPROFILE") {
                if !p.starts_with(&profile) {
                    bail!("refusing to delete outside the user profile: {path}");
                }
            }
            let script = reg.scripts_dir.join("file_delete.ps1");
            if !script.exists() {
                bail!("file_delete.ps1 missing: {}", script.display());
            }
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if dry_run {
                return Ok(Outcome::DryRun(format!("recycle {path}")));
            }
            run_ps_close(
                &script,
                &["-Path", path],
                &name,
                format!("moved '{name}' to the Recycle Bin"),
                format!("'{name}' was already gone"),
            )
        }
    }
}

/// Resolve a spoken file name to a concrete, safe absolute path for deletion.
/// Searches Desktop/Downloads/Documents (and the profile root) for an exact
/// case-insensitive name match, then canonicalizes and profile-jails it. Any
/// ambiguity or wildcard is refused — a DENY action must target ONE known file.
pub fn resolve_user_file(name: &str) -> std::result::Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("no file name was given".into());
    }
    if name.contains(['*', '?', '"', '<', '>', '|']) {
        return Err("wildcards aren't allowed — name one exact file".into());
    }
    let profile = std::env::var("USERPROFILE").map_err(|_| "USERPROFILE not set".to_string())?;
    let base = Path::new(&profile);
    let subdirs = ["Desktop", "Downloads", "Documents", ""];
    let target = name.to_lowercase();

    // Match on the full name ("test.txt") OR the stem ("test") — voice users
    // routinely drop the extension.
    let mut matches: Vec<PathBuf> = Vec::new();
    for d in subdirs {
        let dir = if d.is_empty() { base.to_path_buf() } else { base.join(d) };
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if !e.path().is_file() {
                    continue;
                }
                let fname = e.file_name().to_string_lossy().to_lowercase();
                let stem = Path::new(&fname)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if fname == target || stem == target {
                    matches.push(e.path());
                }
            }
        }
    }
    match matches.len() {
        0 => Err(format!(
            "couldn't find a file named '{name}' in Desktop, Downloads, or Documents"
        )),
        1 => {
            let p = &matches[0];
            let canon = p
                .canonicalize()
                .map_err(|e| format!("couldn't resolve '{name}': {e}"))?;
            let profile_canon = base
                .canonicalize()
                .map_err(|e| format!("couldn't resolve profile: {e}"))?;
            if !canon.starts_with(&profile_canon) {
                return Err("refusing: that file is outside your user profile".into());
            }
            // Strip the \\?\ extended-length prefix for clean display/PS use.
            let s = canon.to_string_lossy();
            Ok(PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()))
        }
        _ => Err(format!(
            "found more than one file named '{name}' — move or rename so it's unambiguous"
        )),
    }
}

/// Fetch free BBC RSS feeds (no API key) and build a spoken news briefing:
/// world -> tech -> sport -> India, a few headlines each. Fail-open: whatever
/// feeds respond get read; if none do, say so.
fn fetch_news() -> String {
    let feeds: &[(&str, &str, usize)] = &[
        ("Top world headlines", "http://feeds.bbci.co.uk/news/world/rss.xml", 3),
        ("In technology", "http://feeds.bbci.co.uk/news/technology/rss.xml", 2),
        ("In sport", "http://feeds.bbci.co.uk/sport/rss.xml", 2),
        ("And from India", "http://feeds.bbci.co.uk/news/world/asia/india/rss.xml", 2),
    ];
    let mut out = String::from("Here's the news. ");
    let mut got_any = false;
    for (label, url, n) in feeds {
        let titles = rss_titles(url, *n);
        if titles.is_empty() {
            continue;
        }
        got_any = true;
        out.push_str(label);
        out.push_str(": ");
        out.push_str(&titles.join(". "));
        out.push_str(". ");
    }
    if got_any {
        out.push_str("That's the latest.");
        out
    } else {
        "Sorry, I couldn't reach the news right now. Please check your connection and try again.".into()
    }
}

/// Extract the first `n` `<item><title>` headlines from an RSS feed.
fn rss_titles(url: &str, n: usize) -> Vec<String> {
    let body = match ureq::get(url)
        .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .timeout(Duration::from_secs(8))
        .call()
        .and_then(|r| r.into_string().map_err(|e| e.into()))
    {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    // Titles inside <item>…</item>; skip the channel title (before the first item).
    let after_first_item = body.split_once("<item>").map(|(_, b)| b).unwrap_or(&body);
    let re = regex::Regex::new(r"(?s)<title>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</title>").unwrap();
    re.captures_iter(after_first_item)
        .filter_map(|c| c.get(1))
        .map(|m| clean_headline(m.as_str()))
        .filter(|t| !t.is_empty())
        .take(n)
        .collect()
}

/// Decode the few HTML entities BBC uses and tidy a headline for speech.
fn clean_headline(s: &str) -> String {
    s.replace("&amp;", "and")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&pound;", "£")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fetch the current weather from wttr.in (free, no API key; auto-detects
/// location by IP when no city is set via JARVIS_WEATHER_CITY) and phrase it
/// for speaking.
fn fetch_weather() -> String {
    let city = std::env::var("JARVIS_WEATHER_CITY").unwrap_or_default();
    let loc = city.trim().replace(' ', "+");
    let url = format!("https://wttr.in/{loc}?format=j1");
    let v: serde_json::Value = match ureq::get(&url)
        .set("User-Agent", "curl/8")
        .timeout(Duration::from_secs(12))
        .call()
        .and_then(|r| r.into_json().map_err(|e| e.into()))
    {
        Ok(v) => v,
        Err(_) => return "Sorry, I couldn't reach the weather service right now.".into(),
    };
    let cur = &v["current_condition"][0];
    let get = |k: &str| cur[k].as_str().unwrap_or("").to_string();
    let desc = cur["weatherDesc"][0]["value"].as_str().unwrap_or("").trim().to_lowercase();
    let temp = get("temp_C");
    let feels = get("FeelsLikeC");
    let hum = get("humidity");
    let area = v["nearest_area"][0]["areaName"][0]["value"]
        .as_str()
        .unwrap_or("your area")
        .to_string();
    if temp.is_empty() || desc.is_empty() {
        return "Sorry, I couldn't get the weather right now.".into();
    }
    format!(
        "In {area}, it's {desc}, {temp} degrees, feeling like {feels}, with {hum} percent humidity."
    )
}

/// Run assistant_update.ps1 and return its stdout as the spoken briefing.
#[cfg(windows)]
fn run_update_script(script: &Path) -> String {
    use std::process::Command;
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &script.to_string_lossy(),
        ])
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                "I couldn't gather a status update right now.".into()
            } else {
                s
            }
        }
        Err(_) => "I couldn't gather a status update right now.".into(),
    }
}

/// Fetch the YouTube results page and extract the first video id. Uses a
/// browser User-Agent so YouTube serves the normal page. Best-effort — any
/// failure returns None and the caller falls back to opening the search.
fn first_video_id(results_url: &str) -> Option<String> {
    let body = ureq::get(results_url)
        .set(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0 Safari/537.36",
        )
        .timeout(Duration::from_secs(10))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    // The first `"videoId":"…"` in ytInitialData is the top result.
    let re = regex::Regex::new(r#""videoId":"([A-Za-z0-9_-]{11})""#).ok()?;
    re.captures(&body).map(|c| c[1].to_string())
}

/// Run a fixed close script (`close_app.ps1` / `close_window.ps1`) with args
/// passed as discrete `-Flag value` tokens — data, never command text. Exit
/// codes: 0 = closed, 2 = target absent, 3 = target didn't respond.
#[cfg(windows)]
fn run_ps_close(
    script_path: &std::path::Path,
    extra_args: &[&str],
    _alias: &str,
    ok_msg: String,
    absent_msg: String,
) -> Result<Outcome> {
    use std::process::Command;
    let path_str = script_path.to_string_lossy();
    let mut args = vec![
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        path_str.as_ref(),
    ];
    args.extend_from_slice(extra_args);
    let out = Command::new("powershell")
        .args(&args)
        .output()
        .context("spawn powershell for close")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    match out.status.code().unwrap_or(-1) {
        0 => Ok(Outcome::Executed(ok_msg)),
        2 => Ok(Outcome::Executed(absent_msg)),
        3 => Ok(Outcome::Executed("target didn't respond to close".into())),
        code => bail!("close script failed (code {code}): {}", stdout.trim()),
    }
}

/// Fill an open_url template: slots were validated at load time to carry the
/// right encoder for their param type.
fn fill_url_template(
    reg: &Registry,
    template: &str,
    params: &HashMap<String, String>,
) -> Result<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let end = start + rest[start..].find('}').context("unclosed { in template")?;
        out.push_str(&rest[..start]);
        let token = &rest[start + 1..end];
        let (name, encoder) = token.split_once('|').unwrap_or((token, ""));
        let value = params.get(name).with_context(|| format!("missing param {name}"))?;
        match encoder {
            "urlencode" => out.push_str(&urlencode(value)),
            "uri" => out.push_str(&uriencode(value)),
            "site_url" => out.push_str(
                reg.sites
                    .get(value)
                    .with_context(|| format!("site alias '{value}' not in map"))?,
            ),
            other => bail!("unknown encoder '{other}'"),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Fill a bare "{name}" template (media_key's key field).
fn fill_plain_slot(template: &str, params: &HashMap<String, String>) -> Result<String> {
    if let Some(name) = template.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
        return params.get(name).cloned().with_context(|| format!("missing param {name}"));
    }
    Ok(template.to_string())
}

/// RFC 3986-ish query-component encoding: unreserved chars pass, space -> '+',
/// everything else -> %XX. The query is only ever a URL component; it never
/// touches a shell string.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Percent-encode for non-http URI schemes (space -> %20, not '+'). Used by
/// the `spotify:` scheme, which rejects '+'.
pub fn uriencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Open a URL in the default browser via ShellExecuteW — the URL is passed as
/// a single wide string directly to the shell API, no command line involved.
#[cfg(windows)]
fn open_url(url: &str) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let op: Vec<u16> = std::ffi::OsStr::new("open").encode_wide().chain([0]).collect();
    let wide: Vec<u16> = std::ffi::OsStr::new(url).encode_wide().chain([0]).collect();
    let r = unsafe {
        winapi::um::shellapi::ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            winapi::um::winuser::SW_SHOWNORMAL,
        )
    };
    // Per docs: > 32 means success.
    if (r as usize) <= 32 {
        bail!("ShellExecuteW failed ({})", r as usize);
    }
    Ok(())
}

/// Tap a virtual key once (down+up) via SendInput — same pattern as
/// whispr-core's inject.rs.
#[cfg(windows)]
fn send_vk(vk: u16) {
    use winapi::um::winuser::{SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP};

    unsafe fn key(vk: u16, up: bool) -> INPUT {
        let mut input: INPUT = unsafe { std::mem::zeroed() };
        input.type_ = INPUT_KEYBOARD;
        let ki = unsafe { input.u.ki_mut() };
        *ki = KEYBDINPUT {
            wVk: vk,
            wScan: 0,
            dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
            time: 0,
            dwExtraInfo: 0,
        };
        input
    }

    unsafe {
        let mut seq = [key(vk, false), key(vk, true)];
        SendInput(seq.len() as u32, seq.as_mut_ptr(), std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_reference_case() {
        assert_eq!(urlencode("lo-fi study beats"), "lo-fi+study+beats");
    }

    #[test]
    fn urlencode_hostile_input_stays_inert() {
        // Shell metacharacters become percent escapes — data, not syntax.
        assert_eq!(urlencode("a&b|c;d\"e"), "a%26b%7Cc%3Bd%22e");
    }

    #[test]
    fn resolve_user_file_rejects_wildcards() {
        assert!(resolve_user_file("*.txt").is_err());
        assert!(resolve_user_file("notes?.md").is_err());
        assert!(resolve_user_file("").is_err());
    }

    #[test]
    fn resolve_user_file_missing_is_error() {
        // A name that (almost certainly) doesn't exist resolves to an error,
        // never a silent wrong target.
        assert!(resolve_user_file("jarvis_nonexistent_zzq_9f3.txt").is_err());
    }

    #[test]
    fn uriencode_uses_percent_twenty_for_space() {
        assert_eq!(uriencode("blinding lights"), "blinding%20lights");
    }
}
