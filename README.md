# Nyxa

**[varundevarajrvu.github.io/nyxa](https://varundevarajrvu.github.io/nyxa/)**

A voice-controlled personal assistant for Windows that runs **entirely on your
machine** — wake word, speech recognition, intent parsing, and execution are
all local. No cloud, no API keys, no audio ever leaving your laptop.

Say *"hey jarvis, open youtube and search lo-fi study beats"* and it happens.
Show the app your hands and its orb UI pans and zooms with them.

<!-- TODO: demo GIF of the orb + a voice command + two-hand zoom -->

## Features

- **Wake word** — "hey jarvis" (openWakeWord ONNX, ~5% of one core always-on),
  or clap three times. Nothing is recorded, transcribed, or stored before the
  wake word fires — pre-wake audio only ever reaches the wake-word scorer.
- **Local speech-to-text** — Parakeet TDT 0.6B (int8) via sherpa-onnx, with
  Silero VAD endpointing. Typical transcript latency 150–350 ms.
- **Two-stage intent parsing** — a grammar over the action registry resolves
  most commands instantly with zero LLM involvement; misses fall back to a
  local LLM via [Ollama](https://ollama.com) (default `qwen2.5:3b`). No
  Ollama? Grammar-matched commands still work fully offline.
- **20 built-in actions** — open sites/apps, close apps or browser tabs,
  media keys, "play X" on YouTube or Spotify (no API keys), draft Gmail
  emails, move/rename/delete files, and more (`registry/actions.toml`).
- **Tiered permission model** (see below) with an append-only audit log and a
  hardware kill switch (`Ctrl+Shift+Alt+K`).
- **Hand-gesture control** — MediaPipe hand tracking (also fully local): one
  hand pans the orb, two hands zoom by spreading them apart.
- **Tray app** — Tauri v2, animated orb that reacts to listening/thinking/
  acting states.

## Security model

Every action in the registry carries a permission tier. **The tier comes from
the registry, never from a model** — no LLM output can name, change, or
downgrade one.

| Tier | Meaning | Gate |
|---|---|---|
| `SAFE-AUTO` | Read-only or trivially reversible | Executes immediately |
| `NEEDS-CONFIRM` | State-changing but recoverable | Spoken yes/no |
| `DENY-BY-DEFAULT` | Destructive or outward-facing | On-screen dialog — requires a mouse click, so ambient audio can't approve it |

Further invariants: transcripts are data, never spliced into shell strings
(typed params go to PowerShell as discrete arguments); LLM output is
schema-validated against the registry and dropped on any mismatch; file
params are canonicalized and jailed to the user profile; deletes go to the
Recycle Bin, never hard-delete; any error anywhere fails closed. Every
utterance — including refusals, errors, and kills — is logged to
`%APPDATA%\jarvis\audit.jsonl`.

## Requirements

- Windows 11 (uses SAPI TTS, SendInput, UI Automation, WebView2)
- [Rust](https://rustup.rs) (stable)
- A microphone; a webcam only if you want gesture control
- Optional: [Ollama](https://ollama.com) with `ollama pull qwen2.5:3b` for
  natural-language commands beyond the built-in grammar
- ~8 GB free RAM is comfortable; the ASR model uses ~1 GB

## Setup

```powershell
git clone https://github.com/varundevarajrvu/nyxa
cd nyxa

# One-time: download the speech-recognition model (~640 MB)
powershell -ExecutionPolicy Bypass -File scripts\fetch_models.ps1

# Build (first build takes a while — Tauri + ONNX runtimes)
cargo build --release -p jarvis-app

# Run FROM THE REPO ROOT (models and the action registry resolve from the cwd)
.\target\release\jarvis-app.exe
```

For a headless/testing version there is also a CLI:

```powershell
cargo run --release -p jarvis-cli              # full voice pipeline in a console
cargo run --release -p jarvis-cli -- --text "open youtube"   # type instead of speak
cargo run --release -p jarvis-cli -- --dry-run                # print, don't execute
```

## Usage

1. Launch the app — the orb window opens and the tray icon appears.
2. Say **"hey jarvis"** (or clap 3×), wait for the orb to brighten, then speak:
   - "open youtube and search for lo-fi study beats"
   - "play believer on spotify" / "pause music" / "volume up"
   - "close the youtube tab" → answers a spoken *yes or no?*
   - "delete report.pdf" → pops a dialog you must physically click
3. Click **Gesture** (top right) to enable hand control.
4. **Panic button:** `Ctrl+Shift+Alt+K` halts everything instantly, or say a
   halt phrase ("jarvis stop"). Resume is manual, never automatic.

### Configuration (env vars)

| Variable | Default | Purpose |
|---|---|---|
| `JARVIS_OLLAMA_MODEL` | `qwen2.5:3b` | Stage-1 intent model |
| `JARVIS_OLLAMA_ESCALATION_MODEL` | (same) | Optional stronger stage-2 model |
| `ANTHROPIC_API_KEY` | unset | Opt-in: route intent parsing to the Claude API instead of Ollama |
| `SKRYVER_MODELS` | `bench/models` | Override the ASR model directory |

## Architecture

```
mic ──► wake word (openWakeWord) ──► VAD (Silero) ──► ASR (Parakeet int8)
                                                          │ transcript
                    ┌─────────────────────────────────────┘
                    ▼
        stage-0 grammar over registry ── hit ──► tier gate ──► executor
                    │ miss                          ▲
                    ▼                               │
        local LLM (Ollama) ── schema-validated ─────┘
                                                    │
                              audit.jsonl ◄── every outcome, incl. refusals
```

Speech recognition and text post-processing come from
[skryver](https://github.com/varundevarajrvu/skryver), my offline dictation
app — Nyxa consumes its `skryver-core` crate as a git dependency.

## Model licenses

Bundled/downloaded models keep their upstream licenses:
[openWakeWord](https://github.com/dscripka/openWakeWord) (Apache-2.0),
[Silero VAD](https://github.com/snakers4/silero-vad) (MIT),
[MediaPipe](https://github.com/google-ai-edge/mediapipe) hand landmarker
(Apache-2.0), Parakeet TDT via
[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) (Apache-2.0 / CC-BY-4.0).

## License

MIT — see [LICENSE](LICENSE).
