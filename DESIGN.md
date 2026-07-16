# Jarvis — Design Doc (v0)

Voice-controlled personal automation agent, layered on top of whispr's local STT
pipeline (`brain\raw\whispr`). This doc covers the five scope items: wake-word
front-end, intent parser, tool registry, runtime model architecture, and
audit log + kill switch. No implementation exists yet — this is the spec.

---

## 0. Relationship to whispr

- **Jarvis is its own Rust workspace** at `brain\raw\jarvis`, consuming
  `whispr-core` as a path dependency (`../whispr/crates/whispr-core`). whispr
  stays a dictation product; jarvis is an automation product. Neither ships
  the other.
- **Reused from whispr-core:**
  - `asr::Engine` — Parakeet TDT 0.6B int8 (the accuracy pick; 130–330 ms live)
  - `postproc::Dictionary` — proper-noun fixup on transcripts
  - `hotkey` — the `GetAsyncKeyState` polling pattern, for the kill hotkey
- **Not reused:** the llama-server sidecar (`llm.rs`). Jarvis's LLM calls go to
  the Anthropic API (§4); the local Qwen stays a whispr dictation feature.
- **One proposed change to whispr code** (flagged now, done only on go-ahead):
  `whispr-core::audio::Recorder` buffers only while gated by the hotkey. The
  wake-word listener needs a continuous frame stream. Plan: add a small,
  backward-compatible streaming tap (frame callback at 16 kHz mono) to
  `audio.rs`, reusing its existing downmix/resample/anti-alias code rather
  than duplicating it.

Proposed layout:

```
jarvis/
├── Cargo.toml                 # workspace
├── crates/
│   ├── jarvis-core/           # wake, capture, intent, registry, exec, audit, kill
│   └── jarvis-cli/            # headless first (mirrors whispr's M1 pattern); tray app later
├── registry/
│   └── actions.toml           # the tool registry (data, not code)
├── DESIGN.md
└── REGISTRY.md
```

---

## 1. Wake-word front-end

**Decision: openWakeWord** — Apache-2.0, fully offline with no license key or
phone-home (Porcupine's free tier requires a Picovoice access key and online
license validation), its models are plain ONNX which slots into the
onnxruntime stack we already ship via sherpa, and it has a **pretrained
`hey_jarvis` model**, so the obvious wake phrase costs zero training.

| | openWakeWord | Porcupine |
|---|---|---|
| License | Apache-2.0, no account | Proprietary; free tier needs AccessKey + periodic online license check |
| Custom phrase | Free (synthetic training) | Console-trained, tied to key |
| Runtime | ONNX models → `ort` crate | Native lib + Rust SDK |
| Accuracy | Good; tunable threshold + VAD gate | Best-in-class |
| Fit | Matches "fully local, open" ethos | License friction contradicts it |

*Fallback flagged:* sherpa-onnx has a zipformer **KeywordSpotter** API. If our
`sherpa-rs` version exposes it, that's a zero-new-dependency alternative worth
a J0 benchmark alongside openWakeWord before committing to `ort`.

### Listener design

- One continuous cpal input stream → 16 kHz mono frames (via the whispr-core
  streaming tap, §0).
- Frames land in a **~2 s ring buffer**. The wake model scores each ~80 ms
  hop. Below threshold, frames are simply overwritten — **nothing is
  persisted, transcribed, parsed, or sent anywhere pre-wake**. The ASR engine
  is not even invoked. This is the privacy invariant, enforced structurally:
  the only consumer of pre-wake audio is the wake-word scorer.
- **On trigger:** audible chime + tray state change (user must always know
  when jarvis is actually listening) → capture the next utterance with VAD
  endpointing (Silero VAD via sherpa; end at ~800 ms trailing silence, hard
  cap 15 s) → transcript via `asr::Engine::transcribe` + dictionary → intent
  parser (§2) → back to idle listening.
- A **second keyword ("jarvis abort")** is loaded in the same spotter and is
  active at all times — it is the voice half of the kill switch (§5).

### State machine

```
IDLE ──wake──► CAPTURE ──VAD end──► PARSE ──tier gate──► EXECUTE ──► IDLE
  ▲                                                                    │
  └────────────────────── HALTED ◄── kill (hotkey/phrase, any state) ──┘
                            │  manual resume only
                            └──────────────► IDLE
```

---

## 2. Intent parser

Three stages, cheapest first. The parser's only job is to map a transcript to
`(action_id, params)` — **the permission tier is never decided here; it is
looked up from the registry** (§3 invariants).

**Stage 0 — local grammar (no network).** Each registry entry carries
utterance patterns with typed slots (e.g. `"search youtube for {query}"`).
A transcript that matches a pattern unambiguously yields action + params with
zero LLM involvement. This covers scope case (a): "open YouTube, search X" is
a pattern match plus slot extraction — the LLM is not needed to *execute*
anything, and with a pattern hit it isn't needed to extract the query either.

**Stage 1 — claude-sonnet-5.** For transcripts with no confident pattern hit
(scope case (b) phrasings the grammar missed, and case (c) candidates). One
Messages API call: the registry's action list (id, description, param schema)
plus the transcript; **strict JSON out** — `{action_id | null, params,
confidence, ambiguity_reason}`. Response is schema-validated: `action_id` must
exist in the registry, params must type-check, else the response is discarded
and treated as unrecognized. Fail-closed.

**Stage 2 — claude-opus-4-8, escalation only.** Triggered when any of:
- Sonnet reports `confidence < 0.75` or a non-null `ambiguity_reason`;
- the resolved action's registry tier is **DENY-BY-DEFAULT** (anything
  plausibly destructive gets a second, stronger read *before* the human is
  even prompted);
- mismatch heuristic: transcript contains destructive verbs
  (delete/send/buy/remove/uninstall/…) but resolved to a SAFE-AUTO action —
  opus re-checks the classification.

Opus never *authorizes* anything — it only classifies better. The
DENY-BY-DEFAULT non-voice confirmation (§3) still stands regardless of what
any model says.

**Unrecognized/ambiguous terminal state:** jarvis responds "I can't do that
yet" (toast + optional TTS), logs it, executes nothing.

**No API key configured →** stages 1–2 are disabled and jarvis runs stage-0
only: pattern-matched SAFE-AUTO/NEEDS-CONFIRM actions still work fully
offline; everything else is politely refused. (Same fail-open-to-degraded
philosophy as whispr's LLM stage, but here we degrade by *refusing*, never by
guessing.)

---

## 3. Tool registry (summary — full spec in REGISTRY.md)

Data file `registry/actions.toml`, loaded at startup, hot-reloadable. Each
entry: `id`, `tier`, `description`, utterance `patterns`, typed `params`, and
an `exec` spec drawn from a small set of executor kinds (`open_url`,
`launch_app`, `media_key`, `ps_template`, …).

Tiers:

| Tier | Meaning | Gate |
|---|---|---|
| `SAFE-AUTO` | Read-only or trivially reversible | None — executes immediately |
| `NEEDS-CONFIRM` | State-changing but recoverable | Spoken yes/no (strict grammar, 10 s timeout = no) |
| `DENY-BY-DEFAULT` | Destructive / irreversible / outward-facing | **Non-voice** confirmation: on-screen dialog, mouse/keyboard click required, timeout = deny |

Security invariants (enforced in code, not convention):

1. **Tier comes from the registry, never from model output.** No LLM response
   can name, change, or downgrade a tier.
2. **Transcript text is data.** It is never interpolated into a shell string.
   Typed params flow into exec templates through per-type encoders
   (urlencode, path-canonicalize, …); `ps_template` params are passed as
   PowerShell *arguments*, never spliced into command text.
3. **LLM output is schema-validated** against the registry before use;
   anything unparseable or referencing an unknown action is dropped.
4. **Param guards can only tighten:** path params are canonicalized and must
   stay under the user profile; app params must hit the allowlist map; etc.
5. **Fail closed.** Any error at any stage → nothing executes, audit line
   written.

---

## 4. Runtime model architecture

Intent classification runs through a swappable **`Backend` trait** so the model
provider is a config choice, not a code change. The router (stage-1 → stage-2
escalation, param re-validation, fail-closed) is provider-agnostic.

**Default: local Ollama** (`ureq` POST to `127.0.0.1:11434/api/chat`) — fully
local, zero cost, no key, no data leaving the machine at all. This is the
primary runtime path. `claude-*` via the Anthropic Messages API stays available
as an opt-in sibling backend (set `ANTHROPIC_API_KEY`). Neither depends on a
running Claude Code session, CLI shelling, or MCP.

The two-tier routing is preserved across providers by mapping the tier to a
concrete model in the backend:

| Concern | Tier | Ollama (default) | Anthropic (opt-in) |
|---|---|---|---|
| Stage-0 grammar match | — none — | fully local, majority of traffic | same |
| Intent parsing (stage 1) | default | `qwen2.5:3b` (`JARVIS_OLLAMA_MODEL`) | `claude-sonnet-5` |
| Ambiguous / high-stakes (stage 2) | escalation | `llama3.1:8b` (`JARVIS_OLLAMA_ESCALATION_MODEL`) | `claude-opus-4-8` |

- **Fable 5 is not in the runtime loop** — dev-time only (the model powering
  this Claude Code session while we build).
- **Text only ever leaves the process** — the transcript string, never audio.
  With Ollama nothing leaves the machine; stage-0 hits never touch any model.
- Backend selection order (CLI): `--mock-llm` → `ANTHROPIC_API_KEY` set →
  local Ollama reachable → offline stage-0-only.
- Endpoint is `127.0.0.1` not `localhost` (Ollama binds IPv4; ureq has no
  happy-eyeballs, so `localhost`→`::1` would spuriously fail).
- Timeout 60 s (local CPU inference; cold model load is slower than an API).
  On failure the utterance is treated as unrecognized (fail closed),
  audit-logged `outcome: "llm_error"`. Adapter differences are isolated in each
  `Backend::complete` — Ollama's `{messages,options}`→`{message.content}` shape
  vs. Anthropic's `{system,messages}`→`{content[].text}`.

---

## 5. Audit log and kill switch

### Audit log

Append-only JSONL at `%APPDATA%\jarvis\audit.jsonl` (size-rotated, kept
local). One line per handled utterance — **including refusals, kills, and
errors**:

```json
{"ts":"2026-07-09T14:31:07+05:30","transcript":"open youtube and search for lo-fi study beats",
 "stage":"grammar","model":null,"action":"youtube.search","tier":"SAFE-AUTO",
 "params":{"query":"lo-fi study beats"},"outcome":"executed","latency_ms":1140}
```

`outcome ∈ executed | confirmed_then_executed | declined | denied_dialog |
unrecognized | killed | llm_error | exec_error`. `model ∈ null | sonnet |
opus` records exactly which model (if any) was consulted — this is the field
that lets us verify the §4 routing policy empirically.

### Kill switch

Two triggers, both always armed:

1. **Hotkey — Ctrl+Shift+Alt+K** (reuses whispr's `hotkey.rs` polling
   pattern, checking all four VKs; user-configurable later).
2. **Spoken — "jarvis abort"**, the second keyword in the wake spotter (§1),
   detected locally with no STT round-trip, so it works even mid-capture.

Semantics on fire:

- A cancellation token is checked **immediately before every side-effectful
  step**; executors are built as small steps so the window between check and
  effect is minimal.
- Child processes spawned by the current action run inside a Windows **Job
  object** → killed as a group, no orphans.
- In-flight Anthropic HTTP requests are dropped; pending confirmations are
  cancelled (counts as deny).
- State becomes **HALTED**: wake listening stops, tray shows halted, and a
  chime confirms. Resume is **manual only** (tray click or the hotkey again)
  — a kill is never silently self-healed.
- Audit line with `outcome: "killed"` and which trigger fired.

---

## 6. Build order (after go-ahead — nothing below exists yet)

- **J0** — Wake spike: openWakeWord `hey_jarvis` via `ort` (and sherpa KWS if
  exposed); measure false-accepts/hour over a workday and detection latency.
- **J1** — Listener: streaming tap in whispr-core, ring buffer, VAD
  endpointing; wake → transcript printed to console.
- **J2** — Registry loader + stage-0 grammar + SAFE-AUTO executors; the
  YouTube reference case works end-to-end offline.
- **J3** — Audit log + kill switch (hotkey + abort keyword + Job objects).
- **J4** — Anthropic client + stage-1/stage-2 routing.
- **J5** — Confirmation flows: spoken yes/no for NEEDS-CONFIRM, non-voice
  dialog for DENY-BY-DEFAULT.
- **J6** — Tray app (Tauri v2, paperback theme, history = audit viewer).

## 7. Decisions (confirmed by Varun, 2026-07-09)

1. **Wake phrase:** "hey jarvis" — openWakeWord's pretrained model, no
   custom training pass.
2. **Response channel: both, tiered.** Toast for everything; SAPI TTS voice
   is added where attention matters: NEEDS-CONFIRM yes/no prompts,
   DENY-BY-DEFAULT dialog announcements, and kill/HALTED events. SAFE-AUTO
   actions stay toast-only (no chatter for routine commands).
3. **Kill hotkey:** Ctrl+Shift+Alt+K (F12 rejected — in use).
4. **CLI first**, tray app later (same M1→M2 pattern as whispr).
