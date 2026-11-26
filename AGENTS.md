# AGENTS.md

Instructions for coding agents working on Jarvis: a Windows desktop assistant that
sees the screen and operates the machine. Rust core, Tauri 2 shell, React island UI.

## Layout

```
crates/jarvis-core/   agent loop, LLM client, tools, policy, OS bridge — no Tauri dependency
src-tauri/            Tauri shell: window, commands, events, global shortcut
src/renderer/         island UI (React + CSS), with a scripted browser demo mode
```

`jarvis-core` must stay independent of Tauri so it can be tested on any platform.
Everything that touches the OS sits behind the `Platform` trait
(`crates/jarvis-core/src/platform/mod.rs`); a new OS is a new backend file, not a rewrite.

### Where things live

| Change | File |
|---|---|
| Assistant tone, rules, capabilities | `system_prompt()` in `crates/jarvis-core/src/agent/mod.rs` |
| Env/settings.json parsing, validation, key resolution | `crates/jarvis-core/src/config.rs` |
| Request shape, streaming, transcription | `crates/jarvis-core/src/llm.rs` |
| Tool schemas, execution, chip summaries | `crates/jarvis-core/src/tools/mod.rs` |
| Approval rules, blocked apps | `crates/jarvis-core/src/policy.rs` |
| OS behaviour (Windows) | `platform/{win,recorder,input,combo}.rs` |
| Starting the local fallback (WSL + Ollama) | `src-tauri/src/ollama.rs` |
| Speaking the answer (SAPI) | `src-tauri/src/speech.rs` |
| Window, commands, events, hotkeys | `src-tauri/src/lib.rs` |
| Island UI | `src/renderer/Island.tsx` |

## Configuration

Resolved by `Config::load()` from the environment and from `~/Jarvis/settings.json`.
The island's settings panel (gear icon in the open state) writes that file and rebuilds
the runtime, so an endpoint, model or key change applies without a restart. **Editing
`.env` still needs a restart.**

Precedence is **environment > `settings.json` > default**. `.env` therefore keeps an
existing setup working, and the panel shows those fields read-only with an `env` tag
(`config::env_locked_names`) rather than saving an edit the next start would override.
`resolve_with` takes the environment lookup as an argument so precedence stays testable
without mutating the real environment; keep that seam.

| Variable | Default | Notes |
|---|---|---|
| `JARVIS_BASE_URL` | `https://api.openai.com/v1` | any OpenAI-compatible endpoint |
| `JARVIS_API_KEY` | — | required for remote endpoints, optional on loopback |
| `JARVIS_MODEL` | `gpt-4o-mini` | must support tool calling **and** image input |
| `JARVIS_MAX_STEPS` | `8` | clamped to 1..=24 |
| `JARVIS_SITE_URL` | unset | OpenRouter attribution only, never sent elsewhere |
| `JARVIS_STT_PROVIDER` | `model` | `model`, `windows` (SAPI) or `whisper` (whisper.cpp in WSL) |
| `JARVIS_WHISPER_BIN` | `whisper-cli` | command or Linux path to whisper-cli inside WSL |
| `JARVIS_WHISPER_MODEL` | — | required with the `whisper` provider; Linux path to a GGUF model |
| `JARVIS_STT_MODEL` | `JARVIS_MODEL` | voice; must accept audio input |
| `JARVIS_STT_BASE_URL` | `JARVIS_BASE_URL` | separate voice endpoint |
| `JARVIS_STT_API_KEY` | — | required when `JARVIS_STT_BASE_URL` differs |
| `JARVIS_FALLBACK_BASE_URL` | unset | second endpoint, tried when the primary fails |
| `JARVIS_FALLBACK_MODEL` | unset | required with the URL above; either missing disables the fallback |
| `JARVIS_FALLBACK_API_KEY` | — | only for a remote fallback; loopback needs none |
| `JARVIS_SPEAK` | `true` | reads the answer aloud for turns that started by dictation |
| `JARVIS_VOICE` | unset | SAPI voice name; blank means the system default |
| `JARVIS_SPEECH_RATE` | `0` | clamped to -10..=10 |

Every value except `JARVIS_SITE_URL` can also be set in the settings panel; the file it writes is
`~/Jarvis/settings.json` (pretty-printed JSON, gitignored by location, not encrypted).
`JARVIS_SITE_URL` is the one value the panel does not expose.

Resolution rules that are easy to break:

- **Loopback needs no key.** `localhost`, `127.0.0.1`, `::1` (Ollama and friends) pass
  validation with an empty key, and `LlmClient` then sends no `Authorization` header.
  Do not reintroduce a blanket "key required" check.
- **The chat key never crosses hosts.** A distinct `JARVIS_STT_BASE_URL` demands its own
  `JARVIS_STT_API_KEY`; `Config::transcription_key` returns empty rather than reuse the
  chat key, and validation explains why.
- **The fallback is one retry, not a loop.** `LlmClient::complete` tries `chat`, then the
  fallback once, then — if a `wake` hook is installed — wakes the local model and tries
  again once. It never retries after a delta has reached the island, because streamed
  text cannot be unsaid; that case reports the primary error instead. Do not turn this
  into a retry loop: a dead endpoint plus a dead fallback must fail in seconds.
  A loopback *primary* also gets one wake-and-retry (`with_primary_wake`), since there
  is no fallback to trigger the hook; a remote primary never does. Build the agent with
  that same configured client via `Agent::new`: a second `LlmClient` created inside the
  shell would silently lose the fallback and wake hook for every turn.
- **Only loopback fallbacks are started.** `ollama::ensure` parses the endpoint, refuses
  anything that is not `localhost` / `127.0.0.1` / `::1`, and validates the model name
  before interpolating it into the `wsl.exe` command — the same rule as `JARVIS_ARG`:
  nothing model-supplied reaches a shell.
- **Speech text never becomes script.** `speech::to_speech` decides *what* is read: code
  fences, inline code, URLs and markdown are dropped, short answers are read verbatim and
  long ones keep two sentences plus "The rest is on screen." The text and voice name travel
  in environment variables and the script arrives on `-Command -` stdin, so an answer cannot
  become PowerShell. Only unedited dictated prompts are spoken, editing the transcript makes
  it a typed turn, and a speech failure is silence, never `Phase::Error`.

- **Stopping the microphone sends an empty-composer transcript.**
  `finishDictation` hands it to the same `sendTurn` the composer form uses, so a dictated
  turn is one action and its answer is spoken. With a half-typed draft it only appends and
  waits for Send; Esc, closing the island and `keep === false` still drop the audio.
- **Chat and voice are separate `Target`s** in `LlmClient`, both routed through one
  `request()`. Voice sends the WAV as an `input_audio` chat part, not to
  `/audio/transcriptions` — so a Whisper-only service will not work by setting a URL.
  `JARVIS_STT_PROVIDER=windows` bypasses that target entirely: `platform/transcribe.rs`
  stages the WAV and runs `System.Speech` in a hidden PowerShell. `whisper` stages the
  same WAV, translates its path to `/mnt/c/...`, and calls `wsl.exe -e whisper-cli ...`
  with Linux paths — no shell, no model output interpolated into a command. Both paths
  return plain text, so the agent never sees audio and the text model always answers
  the turn.
- OpenRouter attribution headers are added only when the base URL host is
  `openrouter.ai`; no other endpoint may receive `HTTP-Referer`.
- **A stored key never travels back to the webview.** `SettingsView` carries
  `hasApiKey` / `hasSttApiKey` booleans, never values, so the panel's key field starts
  blank and a blank field on save means "keep the stored key". Do not add a getter that
  returns keys, and do not echo one back in an error message.

## Commands

| Command | What it proves |
|---|---|
| `npm install` | dependencies (needed before anything else) |
| `npm run app` | `tauri dev`: vite + the app window, using `.env` |
| `npm run dev` | vite only — browser demo mode, no desktop control |
| `npm run build` | `tsc --noEmit && vite build` |
| `cargo test -p jarvis-core` | the test suite (platform-independent) |
| `cargo clippy -p jarvis-core --all-targets -- -D warnings` | lints, warnings are errors |
| `cargo build --workspace` | **the only command that compiles the Windows-only code** |
| `npm run bundle` | release installers in `target/release/bundle/` |

### Where Windows code is compiled

The `Lint & Test` job runs on `ubuntu-latest`, so it builds the stub backend: no
`#[cfg(windows)]` module — `platform/win.rs`, `platform/recorder.rs` — compiles there.
The separate `windows-build` job runs `cargo build --workspace` on `windows-latest` and
is the only job that compiles them. Do not move that build to Ubuntu: Tauri needs
GTK/WebKit headers there, and the Windows modules are cfg'd out anyway.

`cargo check` only type-checks and emits no binaries; use `cargo build` when an
actual executable is the deliverable.

## Conventions

- **The assistant's voice lives only in `system_prompt()`.** Tone, brevity budget and
  the "desktop assistant" framing are one string; the UI holds no personality.
  `agent::tests::system_prompt_mentions_the_platform_and_workspace` asserts that
  framing and the one-sentence budget, so change both together.
- **OS actions go through PowerShell**, and model-supplied text is passed in the
  `JARVIS_ARG` environment variable — never interpolated into script source. A
  prompt injection must not be able to become a command injection.
- Hidden PowerShell windows use `CREATE_NO_WINDOW` (`0x0800_0000`).
- **PowerShell failures must be printed on stdout.** `stderr` is piped but never
  read, so a bare `throw` surfaces to the user as the useless "(no response)".
  Wrap scripts in `try/catch` and emit `$_.Exception.Message`.
- `PlatformFuture` carries model-visible result text; an **empty string means
  "nothing to report"** and the tool layer words the confirmation ("typed",
  "pressed", "clicked").
- File tools are jailed by `config::resolve_in_workspace`. Never read or write
  outside `Config::workspace`, and do not bypass the jail "temporarily".
- Policy (`policy.rs`): read-only tools run freely, changing tools need an
  explicit Allow, blocked app categories stay blocked, unknown tools are refused.
- Do not add crates for audio, input injection or Win32 bindings. The Windows
  backend deliberately uses MCI and PowerShell so every action is reproducible by
  hand in a terminal.

## Windows gotchas already paid for

- `canonicalize()` returns verbatim paths (`\\?\C:\...`) while `join()` does not.
  Build candidates **from the resolved root**, not from the raw workspace, or
  every not-yet-written file looks like it is outside the workspace.
- MCI `waveaudio` devices disagree about recording formats, and the `set`
  commands return 0 before `record` fails with error 328. Negotiate:
  16000 → 22050 → 32000 → device default.
- Do not signal the recorder over stdin: with no console attached,
  `[Console]::In.ReadLine()` hits end-of-stream immediately and cuts recordings
  to a fraction of a second. It polls for a `<wav>.stop` file instead.
- `mciSendString` return codes are the only diagnostics worth having — decode
  them with `mciGetErrorString` before reporting failure.
- A recording stops itself at `MAX_SECONDS` (30 s) in the island, and the script
  gives up after 35 s so an abandoned process cannot record forever.

## Style

- Rust: rustfmt defaults. Comments explain *why* (the constraint, the failure it
  prevents), not what the line does.
- TypeScript: `strict`, `noUnusedLocals`, `noUnusedParameters` — no dead code.
- Keep diffs small and focused. No new dependencies, files or abstractions unless
  the task needs them.

## Tests and lint

- `cargo test -p jarvis-core` must stay green. A behaviour fix deserves a test
  that **fails without the fix** — check that it does before keeping it.
- Never silence a check to make it pass: no `#[allow(dead_code)]` on live code,
  no weakened assertions, no skipped tests. If clippy complains about an argument
  count, group the arguments; do not add `#[allow(clippy::too_many_arguments)]`.

## Secrets

`~/Jarvis/settings.json` (written by the settings panel) and `.env` hold
`JARVIS_API_KEY` and `JARVIS_STT_API_KEY`. Both files are read by the Rust process only:
never log the keys, never send them to the webview, never write them to the audit log,
and never print them in chat or commit them.
`.env.example` is the template, and `.env` must never be copied into one.
