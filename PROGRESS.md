# jarvis — progress

> Orchestration state file. Read this + `DESIGN.md` + `REGISTRY.md` before resuming work.

**Project:** Jarvis — voice-controlled personal automation agent on top of whispr's local STT.
**Repo root:** `C:\Users\varun\brain\raw\jarvis` (consumes `../whispr/crates/whispr-core` as path dep — not yet wired).

## Decisions

| Decision | Value | Status |
|---|---|---|
| Wake engine | openWakeWord (ONNX via `ort` 2.0.0-rc.12) | ✅ Confirmed by J0 spike. sherpa-rs `keyword_spot` noted as fallback, not needed. |
| Wake phrase | "hey jarvis" (pretrained model, no training) | ✅ User confirmed 2026-07-09 |
| Detect threshold | **0.85** (default 0.5 lets "hey travis"-type phrases through at 0.6–0.76; positives score 0.997+) | Pending live false-accept validation |
| Feedback channel | Toast always; SAPI TTS on NEEDS-CONFIRM prompts, DENY dialogs, kill/HALT | ✅ User confirmed 2026-07-09 |
| Kill hotkey | Ctrl+Shift+Alt+K (+ spoken "jarvis abort") | ✅ User confirmed 2026-07-09 |
| Shell | CLI first, tray later | ✅ User confirmed 2026-07-09 |
| Runtime LLM | **Local Ollama (default)** via `Backend` trait; Anthropic API opt-in; Fable 5 dev-only | Swapped from Anthropic-first 2026-07-09 per user (Ollama already installed) |
| Ollama model | **qwen2.5:3b for both tiers** (escalation auto-skips when same model) | Tuned to this 16GB box; see memory finding below |

## Phase status

- **Docs:** DESIGN.md + REGISTRY.md (16 actions) + `.claude/agents/` (action-planner, action-executor) — done, user-approved.
- **J0 — wake spike: DONE — live-validated by user 2026-07-09** (detected on
  real voice via `--mic`; real-voice recording scored 0.998 offline, identical
  to reference). Gotcha: first live test "failed" because the user spoke after
  the 5 s `--rec` window — always check the energy profile before blaming the
  pipeline.
  `crates/wake-spike`: `--wav` scorer, `--mic` live monitor, `--dump` debug.
  Results vs reference openwakeword (Python, `.venv-check/`): positives
  0.997–0.999 identical to 3 decimals; all 8 TTS clips (`testwavs/`) classify
  correctly at 0.85. Perf: ~4–5 ms per 80 ms chunk ⇒ ~5% of one core always-on.
- **J1 — listener: DONE — live-validated by user 2026-07-09** (5 wakes at
  0.966–0.999, accurate transcripts at ~160–260 ms, no-speech timeout path
  exercised). `jarvis-core` (wake.rs
  ported from spike, mic.rs continuous capture, vad.rs silero endpointer) +
  `jarvis-cli` (state machine IDLE→CAPTURE→transcript). Sim run: wake 0.982 →
  VAD endpoint → Parakeet → exact transcript. No whispr-core changes were
  needed after all — jarvis owns its capture (int16-range convention), whispr
  is consumed read-only for `asr::Engine` + `postproc::Dictionary`.
  Notes: cpal must match whispr's 0.18 (links=alsa conflict otherwise; 0.18
  API: `device.description()`, `sample_rate()` → u32, config by value).
  ASR models resolved from `../whispr/bench/models`; silero model at
  `models/silero_vad.onnx`. VAD: 0.8 s trailing silence, 15 s cap, 6 s
  no-speech timeout.
- **J2 — registry + stage-0 + SAFE-AUTO exec: DONE in sim/tests, live voice
  test pending.** `registry/actions.toml` (16 actions incl. review guards;
  shell.run `enabled=false`), `registry.rs` (loader with invariant validation
  at load time, grammar compiler, stage-0 matcher), `exec.rs` (open_url via
  ShellExecuteW, launch_app fixed-argv allowlist, media_key SendInput;
  ps_template/dialog_only return NotImplemented until J5). CLI: tier gate +
  3 s per-action cooldown + `--text`/`--dry-run` modes. 13 unit tests green;
  full gate matrix verified (SAFE-AUTO executes, NEEDS-CONFIRM/DENY blocked,
  shell.run disabled, no-match refused); E2E sim opened the real YouTube URL.
  Registry acceptance: action-planner review 2026-07-09 folded into
  REGISTRY.md; user waived per-entry sign-off ("not required").
  Gotcha fixed: transcript normalization must strip punctuation only at word
  boundaries or filenames lose their dots ("report.pdf" → "reportpdf").
- **J3 — audit log + kill switch: DONE in sim/tests, live test pending.**
  `audit.rs`: append-only JSONL at `%APPDATA%\jarvis\audit.jsonl`, one line
  per utterance incl. refusals/halts/errors, `model:null` for stage-0 (chrono-
  free RFC-3339 local timestamps via Win32 tz offset + Hinnant civil-date).
  `kill.rs`: Ctrl+Shift+Alt+K edge-triggered watcher thread toggling a shared
  halted flag (manual resume only); spoken halt phrases (stop/abort/cancel/…)
  intercepted before registry matching. CLI drains audio while HALTED.
  Also fixed from J2 live findings: (1) leading wake phrase stripped before
  matching so one-breath "hey jarvis, open youtube..." works; (2) bare "hey
  jarvis" → re-listen ("yes?"); (3) added edge/chrome to app map + "close X
  browser" pattern. 15 tests green.
  DEFERRED: pure-KWS "jarvis abort" that skips STT (needs custom wake model
  or sherpa KWS) — current voice halt needs the STT round-trip; the hotkey is
  the hard, mid-action path.
- **J4 — LLM router: DONE in mock, real-API test pending a key.** `llm.rs`:
  `Backend` trait with `AnthropicBackend` (real, ureq POST to
  api.anthropic.com/v1/messages, model ids `claude-sonnet-5`/`claude-opus-4-8`,
  key from `ANTHROPIC_API_KEY`) and `MockBackend` (keyword→canned JSON, can
  force per-model failure). `Router::classify` = stage-1 sonnet → escalate to
  stage-2 opus when confidence ≤ 0.75 / ambiguity_reason set / resolved tier
  DENY / destructive-verb-vs-non-DENY mismatch; opus classifies only, never
  authorizes; opus error falls back to stage-1; any transport/parse/validation
  failure fails closed. LLM params re-validated against registry ParamTypes
  (same guards as grammar — LLM can't widen surface). Disabled actions (shell.run)
  excluded from the catalog. CLI: `--mock-llm` flag; grammar and LLM paths share
  `gate_and_exec` so tier ALWAYS comes from the registry. 25 tests green (9 new).
  Audit now records stage (grammar/llm/none) + model (null/sonnet/opus) — the
  §4 routing policy is empirically checkable from the log. Verified E2E in mock:
  grammar hit → model=null, "close edge" → llm/sonnet→app.close, "close youtube
  now"/"what time" → llm declined→unrecognized.
  TODO when key lands: confirm model ids don't need dated suffixes; smoke-test
  real latency (10 s timeout, 1 retry per DESIGN §4 — retry not yet added).
- **J4b — Ollama backend (local LLM): DONE, working.** Added `OllamaBackend`
  (sibling `Backend`, POST `127.0.0.1:11434/api/chat`, adapter for Ollama's
  `{messages,options}`→`{message.content}` shape). Router logic UNCHANGED —
  the trait seam did its job. New: `Backend::has_distinct_escalation()` — when
  stage-2 model == stage-1 (single-model config), escalation is skipped (temp-0
  re-ask just repeats), so each fallback is ONE call, no model swap. Default:
  qwen2.5:3b both tiers. Ladder (3b→8b) opt-in via
  JARVIS_OLLAMA_ESCALATION_MODEL. Backend priority: --mock-llm → ANTHROPIC_API_KEY
  → Ollama reachable → offline. 26 tests green. Live probe: 5 grammar-miss
  utterances classified single-call, ~13–17s each (31s first = model load),
  volume/youtube resolved correctly, no transport errors.
  HARD-WON TRANSPORT KNOWLEDGE (do not re-learn):
  - Use `127.0.0.1`, NOT `localhost` — Ollama binds IPv4, ureq has no
    happy-eyeballs so `localhost`→`::1` fails the probe.
  - Do NOT pool the ureq agent — Ollama closes idle keep-alive sockets, a
    reused dead socket write-then-hangs to the read timeout. Fresh agent +
    `Connection: close` per call (what curl does; 100% reliable).
  - Cap output with `options.num_predict` (128) — uncapped format:json on a
    small model can run away and blow the read timeout.
  - The intermittent `os error 10060` was ENVIRONMENTAL: the box hit **2.1 GB
    free RAM** mid-session (browser tabs jarvis opened + cargo + ASR), so a
    5.7s call became 19s and Ollama's listener starved → dropped connections.
    Retry-with-backoff (0/1.5/4s) + smaller model + single-call config fixed it.
  - **llama3.1:8b (4.7GB) is impractical on this 16GB box under session load**
    — it pages. Pulled but not the default. qwen2.5:1.5b/3b are the real tier.
  **LIVE-VALIDATED by user 2026-07-09:** Ollama fallback fired with zero
  transport errors ("hey johnny" → LLM declined correctly; grammar hits still
  instant). Test surfaced + fixed a kill-switch gap: a halt during a ~15s
  blocking LLM call wasn't checked before executing the resolved action —
  `gate_and_exec` now checks the halted flag immediately before the side
  effect (outcome "killed"). (Deeper cancellation — interrupting the in-flight
  HTTP call itself — deferred; the pre-exec check is the safety-critical part.)
- **J5a — spoken confirmation + app.close: DONE (real close verified).** User
  reported closing didn't work (app.close was NEEDS-CONFIRM → blocked, and its
  ps_template executor was a stub). Built the NEEDS-CONFIRM spoken yes/no flow:
  matched action → `⏸ close X? say yes or no` (print + best-effort SAPI TTS via
  `tts.rs`) → capture reopens WITHOUT a new wake word (relisten) → yes executes,
  no/other/12s-timeout cancels. Pending state in Ctx; kill-check honoured before
  the confirmed side effect. Echo guard: mic channel drained after the (blocking)
  TTS prompt so it isn't heard as the answer. `app.close` executor now real:
  `scripts/close_app.ps1` does graceful `CloseMainWindow()` (WM_CLOSE, never
  force-kill); process name resolved from a NEW reviewed `[close_process]`
  registry map (alias→proc; explorer excluded — closing it kills the shell);
  name passed as a discrete `-ProcessName` arg, never spliced. Verified live via
  text mode: "close notepad"→"yes" actually closed Notepad (count→0); "no"
  cancels; audit shows awaiting_confirm→confirmed_then_executed / →declined.
  Other NEEDS-CONFIRM actions (file.move/rename, system.sleep) now reach exec on
  "yes" but their scripts don't exist → safe "script missing"/"not wired" error;
  path-param ps_template deliberately refused until the jail/canonicalize guards
  are built. Win11 note: bare `notepad` hits the Store redirect; launch by full
  path for testing.
- **J5a2 — web.close (close a website tab): DONE, verified.** User's real ask
  was "close youtube" — youtube is a SITE, not an app, so app.close never
  matched. Added `web.close` (NEEDS-CONFIRM): "close {site}" / "close the
  {site} tab" → `scripts/close_window.ps1` finds the visible window whose title
  contains the site's `[web_title]` keyword, foregrounds it, sends Ctrl+W →
  closes THAT tab only (not the whole browser). Grammar disambiguates by map:
  a word in [apps] → app.close, in [sites] → web.close (loader enforces the
  maps are disjoint). ps_template executor generalized to handle both `app`
  (→ -ProcessName) and `site` (→ -TitleMatch) params.
  HARD-WON: a background process CANNOT SetForegroundWindow — Windows ignores
  it, so the first Ctrl+W went nowhere ("closed-tab" printed but tab stayed).
  Fix = the focus-unlock dance (ALT tap + AttachThreadInput + SetForegroundWindow
  + BringWindowToTop). Verified on a disposable example.com window: close →
  "closed-tab", re-close → "not-found" = really gone.
  CAVEAT: matches the site only when it's the ACTIVE tab of its window (true
  right after opening); if the user tabbed away, the window title won't match.
  **LIVE-VALIDATED by user 2026-07-09:** by voice, "close youtube tab" +
  "close spotify" + "close gmail tab" all confirmed-and-closed successfully;
  app.close and web.close both work end to end with spoken yes/no.
- **Polish — LLM confidence floor: DONE.** `Router::classify` now declines any
  final resolution below `CONFIDENCE_FLOOR` (0.3), applied AFTER escalation so a
  stronger model gets its shot first. Fixes the "YouTube tab → conf 0.00 still
  executed" looseness the user spotted; genuinely ambiguous fragments now
  decline (a clearer re-phrasing hits the instant grammar path). qwen calibrates
  well enough (0.9–1.0 clear, 0.0 ambiguous) that the floor separates cleanly.
  28 tests (2 new: below-floor declines, at-floor kept).
- **Playback + email (all keyless/free): DONE.** Registry now 20 actions.
  - `youtube.play` (SAFE-AUTO): "play X" (bare default) / "play X on youtube" —
    reads the PUBLIC results page for the top `"videoId"` (regex, browser UA, no
    API key — same idea as yt-dlp) and opens `watch?v=ID` (autoplays). Falls
    back to the search page if the scrape fails. New exec kind `youtube_play`.
    Verified in-process: "✓ playing '…' on YouTube".
  - `spotify.play` (SAFE-AUTO): "play X on spotify" — NOW ACTUALLY PLAYS, no
    API key. `scripts/spotify_play.ps1` opens `spotify:search:X`, waits for the
    results to render, then uses **UI Automation** to find and Invoke() the
    "Play <song>" button whose name contains all query words. InvokePattern
    needs no focus (no SendKeys/focus-steal). Verified via SMTC (the Windows
    media session): "play believer on spotify" → Playing | Imagine Dragons -
    Believer. Bare "play X" still routes to YouTube; spotify.play ordered BEFORE
    youtube.play so "…on spotify" wins over the bare catch-all.
    KEY FINDINGS (do not re-learn): `spotify:track:ID` navigates but does NOT
    autoplay on this build; media keys are a global toggle (unreliable to
    target a track); Spotify's OS window title stays "Spotify Premium" even when
    playing (use SMTC, not the title, to check playback); Spotify IS visible to
    UIAutomation (~567 elements) and names play buttons "Play <song> by
    <artist>". Caveat: matches the FIRST such button in UIA tree order, so it
    may pick a remix/version (e.g. "shape of you" → Stormzy Remix) — be specific
    for the exact cut. Takes ~6-8s (search render + retries). `uri` encoder now
    unused (kept as a harmless pub util).
  - `email.draft` (SAFE-AUTO): "write/draft/compose an email to {recipient}
    saying {body}" → prefilled Gmail compose URL (opens a draft, never sends).
    Keyless. Caveat: body comes from the normalized transcript (lowercased, no
    punctuation) — a rough draft the user edits before sending.
  30 tests green. All three need NO API keys.
- **J5b — DENY-BY-DEFAULT dialog + file.delete: DONE (recycle verified; live
  click test pending).** Phase 2. `dialog.rs`: non-voice modal via
  `MessageBoxTimeoutW` (user32) — Yes/No, warning icon, system-modal +
  foreground, default No, 30 s timeout→deny. A spoken "yes" can't approve DENY
  (that's the point — ambient audio can't click). `file.delete` wired end to
  end: new `recycle_file` exec kind; `exec::resolve_user_file` finds the named
  file in Desktop/Downloads/Documents (exact case-insensitive match),
  canonicalizes, profile-jails, strips \\?\, refuses wildcards / not-found /
  ambiguous; `scripts/file_delete.ps1` sends to Recycle Bin (VisualBasic
  FileSystem.DeleteFile, SendToRecycleBin — recoverable, never hard-delete);
  executor re-validates absolute+file+jailed (defense in depth). CLI `deny_flow`:
  resolve → build body with the RESOLVED absolute path → modal → execute only on
  click; No/timeout/kill all deny; audit denied/confirmed_then_executed/killed.
  `--dry-run` skips the modal (prints what it'd prompt) for testing.
  Verified: dry-run resolves the path, refuses missing + wildcard; file_delete.ps1
  actually recycled a throwaway (confirmed present in Recycle Bin, extension
  hidden in its display name). Other DENY actions (pkg.install/message.send/
  system.settings/shell.run) stay blocked — no dead-end dialog until they have
  guarded executors. 31 tests (3 new: wildcard/empty reject, missing-file error,
  uriencode %20).
- **J6 — tray app: IN PROGRESS (Phase 3).**
  - **Engine extraction DONE + verified.** Moved all orchestration (Pipeline,
    handle_transcript, act_on_intent, deny_flow, execute_confirmed, gate_and_exec,
    confirm flow, audit, is_yes/no, confirm_label) from jarvis-cli/main.rs into
    `jarvis-core::engine`. `Engine` owns registry/audit/router/pipeline + runtime
    state; `Ctx` borrows distinct fields per-utterance (disjoint borrows).
    `println!`→`note!` macro (ignores write errors → panic-safe with no console).
    Added `status: Arc<Mutex<String>>` for the tray. Engine API: load / run_mic /
    feed_text / feed_wav / status_handle / audit_path. jarvis-core now depends on
    whispr-core + hound. jarvis-cli is now a thin ~60-line wrapper. CLI behavior
    verified unchanged (dry-run flows identical); 31 tests still green.
  - **jarvis-app (Tauri v2) scaffolded**, mirroring whispr-app: Cargo.toml,
    build.rs, tauri.conf.json (identifier dev.varun.jarvis), capabilities, icons
    (copied from whispr), ui/index.html. main.rs: loads engine, runs run_mic on a
    bg thread, system tray (Open/Pause-Resume/Quit), window on launch, prevents
    exit on window close. Commands: get_status, get_history (reads audit.jsonl,
    newest-first), toggle_pause (shares the kill flag). UI: paperback-themed
    status pill + pause button + live history list (polls 1.2/1.6s, color-coded
    outcomes). First build running.
    BUILT + smoke-tested: jarvis-app.exe (33MB) launches, loads engine, opens
    the "Jarvis" window (confirmed via EnumWindows) + tray icon, runs without
    crashing. Note: WebView2 window reports empty Get-Process MainWindowTitle
    (same quirk as browsers/Spotify) — use EnumWindows to confirm. First Tauri
    build ~10 min.
  - **Gothic orb UI + fixes (2026-07-10).** UI redesigned as a central animated
    "divine energy cluster" (canvas 2D, additive glow, ~640 particles, drifting
    embers, spectral filaments, dark-sun eclipse core, blood aura, vignette),
    reacting to engine status. User feedback fixes:
    (1) STUCK "THINKING": status set to "Thinking…" before the LLM call was
        never reset on decline/error → orb froze. Fixed: decline/error paths set
        a clear status, + run_mic decays to IDLE_STATUS 4 s after any activity.
    (2) "no listening feedback": added a distinct `listen` mode — bright pulsing
        double-ring + hard intensity surge while capturing a command; status
        poll 900→280 ms so it reacts promptly. moodFrom now returns a `mode`.
    (3) "can't leave history": drawer covered its own toggle button. Added a ×
        close button in the drawer header + Esc + click-orb-to-close.
    (4) app icon was Xenon's: generated a distinct gothic crimson-eclipse-orb
        icon set (scripts via System.Drawing → PNGs 16/32/48/128/256 + a valid
        25KB multi-size icon.ico; note: PS BinaryWriter.Write(Byte[]) needed
        explicit $fs.Write to embed the PNGs — first attempt gave a 74-byte ICO).
    Background-audio caveat: leftover YouTube/Spotify playback garbles the mic
    → mis-transcription → LLM decline. Not a bug; stop media before speaking.
  - **History → How-to guide (2026-07-10):** removed the activity/history drawer,
    replaced with a new-user "How to use Jarvis" guide (example commands by
    category + orb color legend), auto-opens first launch only (localStorage).
    get_history command left registered but unused.
  - **Auto-launch on login:** Startup-folder shortcut `Jarvis.lnk` →
    target jarvis-app.exe, WorkingDirectory = workspace root (so models_root
    resolves), icon.ico. Remove via shell:startup.
  - **Renamed to "Nyxa" (2026-07-16, previously undocumented):** productName/
    identifier (dev.varun.nyxa), window/tray/menu strings, guide text. Wake
    phrase still "hey jarvis" (pretrained); engine prefers a custom
    `models/wake/hey_nyxa.onnx` if one is ever trained. Guide also mentions
    clap-to-wake. `strip_wake_prefix` accepts hey nyxa/nyxa/hey jarvis/jarvis.
    Crates/paths/audit dir keep the jarvis name.
  - **Hand-gesture orb control (MediaPipe HandLandmarker, all local):**
    `ui/index.html` has a complete gesture module — Gesture button (top-right)
    starts the webcam (WebView2 flags in main.rs auto-grant the REAL camera:
    `--use-fake-ui-for-media-stream`), camera HUD bottom-left; 1 hand = pan the
    orb (palm = wrist+knuckle-9 midpoint, mirrored x), 2 hands = zoom from
    hand spread (spread 0.12–0.6 → scale 0.55–2.4), no hands 400 ms → ease
    home. Orb consumes eased gx/gy/gs each frame (CX/CY/UNIT from base values;
    starfield stays fixed). Session stalled because `ui/vendor/` was empty:
    fetched @mediapipe/tasks-vision 0.10.14 (`tasks-vision.mjs`, wasm simd +
    nosimd pairs) + `hand_landmarker.task` (float16 v1, 7.8 MB) — verified
    magic bytes + HandLandmarker/FilesetResolver exports. Assets embed in the
    exe (~27 MB added). Rebuild + live test in progress.
  - **GitHub prep (2026-07-16):** git repo initialized (main, 63 files,
    ~30 MB). skryver-core is now a GIT dependency (resolves to the same
    commit as the local checkout); local dev keeps building the sibling
    checkout via a gitignored `.cargo/config.toml` [patch] — both paths
    verified (`cargo metadata` unpatched + `cargo check` patched).
    Excluded from the repo: target/, .venv-check/, testwavs/ (real voice
    recordings), bench/ (ASR models — cloners run scripts/fetch_models.ps1,
    URL verified 200). Added README/LICENSE(MIT)/.gitattributes;
    assistant_update.ps1 projects path genericized (JARVIS_PROJECTS env or
    ~\brain\raw). Remote repo NOT created yet — awaiting name + go-ahead.
  - **Ops gotchas learned the hard way:**
    · NEVER fire multiple `cargo build` in parallel — they block on the target
      lock, appear to hang, and killing them mid-build corrupts incremental
      state → 7-min rebuilds. One build at a time.
    · Tauri COMPRESSES embedded UI assets, so grepping the exe for HTML text
      to "verify the build" is a false negative. Trust a clean BUILD_EXIT=0.
    · Mic wedged at the OS level (audit dead since 03:13, wake-spike also got
      silence, right device + unmuted) after the memory-starved machine
      thrashed during a long build. Fix = reboot / re-enable device /
      admin `Restart-Service Audiosrv`. NOT a Jarvis bug. Recording-to-verify
      has a timing trap: --rec starts before the user sees "speak now", so low
      peaks can just mean silence. Use the app's continuous listen + audit log.
    · CoreAudio IAudioEndpointVolume vtable order matters — a mis-declared
      interface set garbage/muted the mic. Correct order documented in
      /tmp/micfix2.ps1 pattern (Set/GetMasterVolumeLevelScalar at slots 5/7,
      SetMute/GetMute at 12/13).

## Hard-won knowledge (do not re-learn)

- **openWakeWord's classifier expects STREAMING features, not batch.** Mel
  must be computed per 80 ms chunk over `chunk + 480` samples (edge effects
  included), one embedding per chunk on the last 76 mel frames, feature
  buffer seeded with random-noise embeddings (never zeros). Whole-clip batch
  features score ~0.0 on true positives. `Streamer` in wake-spike is the
  faithful replica; keep it as the single feature path.
- Models feed on **int16-range f32** samples (no ±1 normalization) — opposite
  of whispr's normalized pipeline. Conversion happens at the capture boundary.
- `ort` rc.12 uses **ndarray 0.17** (0.16 fails trait bounds); cpal 0.16
  `sample_rate()` returns `SampleRate` (`.0` for the u32).
- Model files: `models/wake/*.onnx` from openWakeWord v0.5.1 GitHub release.

## Next

1. **Varun: live mic test** — `target\release\wake-spike.exe --mic`; speak
   "hey jarvis" (+ near-misses), then leave it running during normal
   work/music for a false-accept count. Tune threshold if needed.
2. **J1** — streaming tap in whispr-core, VAD endpointing (sherpa-rs
   `silero_vad`), wake → transcript in console.
