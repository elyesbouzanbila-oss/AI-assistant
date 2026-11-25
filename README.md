# Jarvis

A floating island AI assistant for Windows that actually operates your PC: it opens and closes
apps, focuses windows, types, clicks, reads your screen and works with files.

Rust (Tauri 2 + `jarvis-core`) behind a React island UI. ~15 MB installer, low idle memory,
no Node process at runtime.

## Requirements

| | |
|---|---|
| Rust | `rustup` with the **MSVC** toolchain, plus **Visual Studio Build Tools** (Desktop development with C++). One-time, ~2–3 GB. |
| Node | 20+ (build-time only) |
| Windows | 10/11. WebView2 is present by default; the installer bundles a bootstrapper in case it is not. |

## Setup

```bash
npm install
npm run app                 # dev build: vite + cargo tauri dev
```

Then open the island (hover it or press **Ctrl+Space**), click the gear icon and fill in
the endpoint, model and API key. That is the whole setup — no file to edit.

`npm run bundle` produces an `.msi` / `.exe` installer in `target/release/bundle/`.

## Settings

Everything is in the island's gear menu and saved to `~/Jarvis/settings.json`: endpoint,
model, API key, max steps, and the optional voice model / endpoint / key. **Saving applies
immediately** — no restart.

`.env` still works and takes precedence, so an existing setup keeps running and the fields
it controls show an `env` tag. `.env.example` documents every variable, including the few
things the panel deliberately leaves out (OpenRouter's `JARVIS_SITE_URL`).

### Choosing a model

Any OpenAI-compatible endpoint works. The model must support **tool calling** and **image input**
— screenshots are sent to it.

**OpenRouter** is the default in `.env.example`: one key, `author/slug` model ids, and it is
plainly OpenAI-compatible, so nothing in the client is OpenRouter-specific.

```
JARVIS_BASE_URL=https://openrouter.ai/api/v1
JARVIS_API_KEY=sk-or-v1-...
JARVIS_MODEL=google/gemini-2.5-flash
JARVIS_SITE_URL=https://example.com   # optional; only sent to openrouter.ai
```

Filter for models that support both requirements:
`https://openrouter.ai/models?supported_parameters=tools,vision`. Z.ai GLM and OpenAI configurations
are in `.env.example` too. OpenRouter attribution headers (`X-OpenRouter-Title` and `HTTP-Referer`)
are only built when the base URL host is `openrouter.ai`, so other endpoints never receive them.

### Running fully locally (Ollama)

Ollama serves the same OpenAI-compatible protocol, so it needs no separate code path: point the base
URL at it and nothing leaves the machine.

```bash
ollama pull qwen2.5vl          # the model must do tool calling AND image input
```

```
JARVIS_BASE_URL=http://localhost:11434/v1
JARVIS_MODEL=qwen2.5vl
```

`JARVIS_API_KEY` can stay empty for a loopback endpoint (`localhost`, `127.0.0.1`, `::1`): the app
only requires a key for remote endpoints and sends no `Authorization` header when it has none.

Dictation is the exception: Ollama's API takes no audio input. Point `JARVIS_STT_BASE_URL` and
`JARVIS_STT_API_KEY` at a cloud audio model and voice keeps working while chat stays local — the
transcript is typed into the input box and then answered by the local model.

## Using it

- The island sits at the top centre of the screen. Hover it (or press **Ctrl+Space**) to open.
- **Ctrl+Shift+Space** starts and stops dictation. **Esc** cancels whatever is running and drops
  an in-progress recording. **Ctrl+Shift+Q** quits.
- Actions that change your machine show an Allow / Deny chip first, and auto-deny after 10 seconds.

## Voice

Click the mic in the composer (or press **Ctrl+Shift+Space**), speak, and stop. The transcript
lands in the input box so you can fix a word before sending — nothing runs on its own. Recordings
stop by themselves after 30 seconds.

By default the endpoint that answers questions also transcribes the audio (the OpenAI `input_audio`
content part), so your existing key covers it. Gemini 2.5 Flash and the GPT audio models accept
audio; for a text-only main model, point `JARVIS_STT_MODEL` at an audio-capable model.

Transcription can also live somewhere else entirely: set `JARVIS_STT_BASE_URL` and
`JARVIS_STT_API_KEY` and the recording is sent there instead. That is what makes a fully local chat
model usable with cloud dictation. A separate endpoint never inherits `JARVIS_API_KEY`, so the chat
key cannot leak to another provider.

Capture is Windows MCI (`waveaudio`) through the same hidden PowerShell used for window control,
so there is no audio dependency. The WAV is written to the temp folder, sent once, and deleted
whether transcription succeeds or not.

## What it can do

| Tool | Notes |
|---|---|
| `open_app` | Start-menu app name, URL or `ms-settings:` URI |
| `close_app` | `CloseMainWindow` only — never kills a process, so unsaved work prompts normally |
| `focus_window` | Restores and foregrounds a window by title |
| `list_windows` | Enumerates visible windows |
| `type_text` | Types into the focused window via `SendInput` |
| `press_keys` | `ctrl+s`, `alt+f4`, `ctrl+shift+t`, … |
| `click` | Absolute screen coordinates (physical pixels) |
| `screenshot` | Whole virtual desktop, downscaled to 1400 px JPEG before it goes to the model |
| `read_file` / `write_file` | Jailed to `~/Jarvis/workspace` |
| `system_info` | OS, backend, workspace |

## Safety model

- **Read-only tools** (screenshot, list windows, read file) run freely.
- **Changing tools** need an explicit Allow in the island.
- **Blocked outright**: password managers, wallets, banking (by name), anything outside the
  workspace, and any unknown tool. An agent that reads your screen can be steered by what it
  reads, so it never gets near credentials.
- Model-supplied text is passed to PowerShell as an environment variable, never interpolated into
  the script, so a prompt injection cannot become a command injection.
- Every action is appended to `~/Jarvis/audit.log` (JSONL) with a timestamp and outcome.
- The API key lives in `~/Jarvis/settings.json` or `.env`, read only by the Rust process. The
  webview learns only whether a key is stored, never its value, and the key is never written to
  the audit log.

## Layout

```
crates/jarvis-core/     agent loop, LLM client, tools, policy, OS bridge (no UI dependency)
src-tauri/              Tauri shell: window, commands, events, global shortcut
src/renderer/           island UI (React + CSS), with a scripted browser demo mode
```

`jarvis-core` has no Tauri dependency, so `cargo test -p jarvis-core` runs on any platform.
Open `npm run dev` in a browser to see the island UI in demo mode (no desktop control there).

## Not in this version

Wake-word, spoken replies, tray icon and a control-centre window, macOS/Linux backends, and
multi-monitor-aware coordinates for `click` (the screenshot covers the whole virtual desktop, but
clicks use primary-screen physical pixels).
