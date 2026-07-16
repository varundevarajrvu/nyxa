# Jarvis — Initial Tool Registry (v0)

Spec for `registry/actions.toml`. 16 actions across the three tiers.
Tier definitions and security invariants: see `DESIGN.md` §3.

## Entry schema

```toml
[[action]]
id          = "domain.verb"        # unique, stable — audit log keys on this
tier        = "safe-auto"          # safe-auto | needs-confirm | deny-by-default
description = "one line — this exact text is what the LLM sees in stage 1"
patterns    = ["...{slot}..."]     # stage-0 grammar; {slot} = typed capture
[[action.params]]
name = "query"; type = "string"    # types: string | path | app_alias | site_alias | enum(...)
required = true
[action.exec]
kind = "open_url"                  # open_url | launch_app | media_key | ps_template | dialog_only
template = "https://...{query|urlencode}"
```

Executor kinds (closed set — adding a kind is a code change + review):

| kind | What it does | Param handling |
|---|---|---|
| `open_url` | `ShellExecuteW(NULL, "open", url, ...)` on a built URL | slots pass through per-slot encoders (`urlencode`) |
| `launch_app` | Spawn from the app allowlist map | param must resolve in the map; arbitrary exe paths rejected |
| `media_key` | Synthesize a media virtual-key (`SendInput`, like whispr's `inject.rs`) | enum only |
| `ps_template` | Run a **fixed** PowerShell script file shipped with jarvis | params passed as PS *arguments* (never spliced into command text); paths canonicalized + profile-jailed |
| `dialog_only` | No autonomous executor — renders a confirm dialog that performs the action on click | DENY-BY-DEFAULT tier only |

---

## The registry

### SAFE-AUTO — read-only or trivially reversible; executes immediately

| # | id | Utterance example | Exec |
|---|---|---|---|
| 1 | `youtube.search` | "open youtube and search for lo-fi study beats" | `open_url` → YouTube results page |
| 2 | `web.search` | "search for rust job objects" | `open_url` → default engine query |
| 3 | `browser.open_site` | "open gmail" | `open_url` via **site alias map** (gmail→mail.google.com, github, calendar, …) — no freeform URLs by voice |
| 4 | `app.launch` | "open notepad" / "launch spotify" | `launch_app` via **app allowlist map** (notepad, calculator, vscode, spotify, terminal, …) |
| 5 | `media.control` | "pause the music" / "next track" | `media_key` (play-pause/next/prev enum) |
| 6 | `system.volume` | "volume up" / "mute" | `media_key` (vol-up/vol-down/mute; ±2 steps per utterance, bounded) |
| 7 | `weather.check` | "check the weather" | `open_url` → weather page for configured city |

### NEEDS-CONFIRM — state-changing but recoverable; spoken yes/no gate

| # | id | Utterance example | Exec | Why this tier |
|---|---|---|---|---|
| 8 | `app.close` | "close spotify" | `ps_template` (graceful `WM_CLOSE`, never `taskkill /f`) | may lose unsaved state |
| 9 | `file.move` | "move report.pdf from downloads to documents" | `ps_template`; paths canonicalized, jailed to user profile, **never overwrites** (fails if dest exists) | reversible by moving back, but mutates layout |
| 10 | `file.rename` | "rename draft.md to final.md" | `ps_template`; same guards as move | same |
| 11 | `system.sleep` | "put the computer to sleep" | `ps_template` | interrupts everything running |

### DENY-BY-DEFAULT — destructive / irreversible / outward-facing; non-voice dialog gate

| # | id | Utterance example | Why this tier |
|---|---|---|---|
| 12 | `pkg.install` | "install 7zip" | **Moved from NEEDS-CONFIRM per action-planner review:** an installer is vendor code execution + persistence, and a spoken "yes" is defeatable by the same ambient audio that issued the command. Dialog shows resolved `winget` id + source; source pinned to the official repo |
| 13 | `file.delete` | "delete old-notes.txt" | Deletion. Single canonicalized, profile-jailed path only — no wildcards/globs/recursive; Recycle Bin semantics enforced in the executor; dialog shows the resolved absolute path. Stays DENY — recoverability is not a reason to weaken the gate on a destructive verb |
| 14 | `message.send` | "send an email to…" | Outward-facing: once sent, unrecallable. Dialog must render the **resolved** recipient address(es) and full body verbatim — no alias may mask the send target |
| 15 | `system.settings` | "change the default browser" / power plan / network | System-wide blast radius. **Fixed enum of individually pre-scripted settings only** — no freeform setting/registry-path param; arbitrary registry writes are out of scope entirely |
| 16 | `shell.run` | "run a command…" | The escape hatch = arbitrary code execution. **Off by default** (explicit config-flag opt-in, not shipped enabled in v0); always Stage-2 opus classification; dialog shows the command character-for-character with an explicit warning; runs inside the kill-switch Job object |

**Registry meta-rule:** a new action lands in the *most restrictive plausible
tier* and can only be relaxed after `action-planner` review (see
`.claude/agents/`). Params can tighten an action's guard but never loosen a
tier.

## Review outcomes (action-planner, 2026-07-09)

All 16 entries reviewed before acceptance. 7 ACCEPT as-is (`youtube.search`,
`app.launch`, `media.control`, `system.volume`, `weather.check`, `file.move`,
`file.rename`), 1 tier correction (`pkg.install` → DENY-BY-DEFAULT, applied
above), 7 NEEDS-GUARDS (guards folded into the tables above). Cross-cutting
requirements now binding on implementation:

1. **Every entry ships an explicit param block** — URL slots: `max_len` +
   `|urlencode`; path slots: canonicalize + profile-jail + no-overwrite;
   alias slots: map membership.
2. **Symlinks/junctions are resolved BEFORE the profile-jail check**; a
   target that escapes the jail via a link is rejected (`file.move`/`rename`/
   `delete`).
3. **`launch_app` and `app.close` never accept user-derived arguments**; both
   resolve targets through the shared app allowlist map.
4. **Alias maps are security data.** Site aliases must be bare HTTPS landing
   origins (no state-changing GET endpoints, no deep links); editing either
   map is a reviewed change.
5. **Spoken yes/no is spoofable** by the same ambient audio that issued the
   command — that's why installer-class actions can't sit in NEEDS-CONFIRM.
   Harden the confirm prompt later (e.g. randomized echoed token) if abuse
   shows up in the audit log.
6. **`dialog_only` renders fully resolved, non-truncated params** — absolute
   paths, resolved addresses, literal commands. Never an alias or summary.
7. **Per-action cooldown** (anti-replay: a TV/loop can't burst-fire SAFE-AUTO
   actions).

---

## Reference case, end to end: `youtube.search` (SAFE-AUTO)

Registry entry:

```toml
[[action]]
id          = "youtube.search"
tier        = "safe-auto"
description = "Open the default browser to the YouTube search results page for a spoken query."
patterns    = [
  "open youtube (and )?(search|look) (for )?{query}",
  "search (on )?youtube for {query}",
  "youtube {query}",
]
[[action.params]]
name = "query"; type = "string"; required = true; max_len = 120
[action.exec]
kind     = "open_url"
template = "https://www.youtube.com/results?search_query={query|urlencode}"
```

Walkthrough of: *"Hey Jarvis — open YouTube and search for lo-fi study beats"*

| Step | What happens | Budget |
|---|---|---|
| 1. Wake | Spotter scores ring-buffer hops; "hey jarvis" crosses threshold. Chime. Nothing before this instant was transcribed or kept. | ~0 (continuous) |
| 2. Capture | VAD-endpointed take: ends ~800 ms after speech stops (hard cap 15 s) | ~800 ms tail |
| 3. STT | Parakeet + dictionary → `"open youtube and search for lo-fi study beats"` | ~300 ms |
| 4. Stage 0 | Pattern 1 matches; slot `query = "lo-fi study beats"`. **No LLM call — `model: null`.** | ~0 ms |
| 5. Tier gate | Registry says `safe-auto` → no confirmation | — |
| 6. Guards | length ≤ 120 ✓; encode: `lo-fi+study+beats` | ~0 ms |
| 7. Execute | `ShellExecuteW(NULL, "open", "https://www.youtube.com/results?search_query=lo-fi+study+beats", ...)` — the query is only ever a URL component; it never touches a shell string | ~100 ms |
| 8. Audit | line below | ~0 ms |
| 9. Feedback | Toast "▶ YouTube: lo-fi study beats"; state → IDLE | — |

```json
{"ts":"2026-07-09T14:31:07+05:30","transcript":"open youtube and search for lo-fi study beats",
 "stage":"grammar","model":null,"action":"youtube.search","tier":"SAFE-AUTO",
 "params":{"query":"lo-fi study beats"},"outcome":"executed","latency_ms":1140}
```

**Total: ~1.2 s from end of speech to browser opening**, fully offline.

Failure modes:
- Empty slot ("open youtube and search for…" trailing off) → required-param
  fail → toast "search for what?", `outcome:"unrecognized"`, nothing opens.
- Kill fired between steps 5–7 → cancellation token check before
  `ShellExecuteW` aborts, `outcome:"killed"`.
- Pattern miss (odd phrasing) → falls to stage 1 (sonnet) which should return
  `youtube.search` + query; tier still comes from the registry, not the model.
