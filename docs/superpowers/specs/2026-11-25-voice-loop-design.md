# Voice loop: speak answers back

**Date:** 2026-11-25
**Status:** approved for implementation
**Scope:** a turn that started by dictation is answered out loud, using the Windows voice
already installed. No new dependencies, no network, no tokens.

## Why

Alfred listens but never answers. Dictation is half a conversation: you speak, it replies on
screen, and you have to look at the island and read. Everything else about the app assumes you
are at the keyboard, which is the opposite of why voice input exists.

This closes the loop: speak a request, hear the answer.

## Decisions

| Decision | Choice | Why |
|---|---|---|
| When it speaks | Only when the turn started by dictation | Typed turns stay silent, so it never talks over a meeting. Predictable. |
| Engine | Windows `System.Speech` (SAPI) via hidden PowerShell | Ships with Windows, offline, free. Keeps the project rule of no audio crates and no new dependencies. |
| Where the spoken text comes from | A pure function in `alfred-core`, not a model call | Deterministic, reviewable, testable without Windows or audio, and costs no tokens. The weaker fallback model also behaves unpredictably at "write a spoken version". |
| When it starts | After the turn's final answer | Sentence-by-sentence streaming is lower latency but needs a queue, de-duplication and careful cancel. It can be layered on later without changing the summariser. |
| Length handling | Under 200 characters verbatim; longer, first two sentences + "The rest is on screen." | A voice answer cannot be skimmed. The screen keeps the detail; the ear gets the headline. |
| Stopping | Kill the PowerShell process | Blocking `Speak` has no in-band cancel; the process is the natural handle, and it matches how `ollama.rs` already manages a child. |

### What counts as "I spoke first"

Dictation deliberately drops the transcript into the composer so a misheard word can be
fixed. So a turn is voice-initiated when the prompt sent **is byte-identical to the transcript
that dictation produced, and the user did not edit it**. Editing it makes it a typed turn.

This is a comparison, not a timer: no "sent within 5 seconds" heuristic, nothing that behaves
differently on a slow machine.

## Architecture

Two units, each independently testable.

### `speech::to_speech` — `crates/alfred-core/src/speech.rs` (new)

```rust
/// What should be read aloud for this answer, if anything.
pub fn to_speech(answer: &str) -> Option<String>;
```

Pure, no I/O, no platform. Steps, in order:

1. Remove fenced code blocks entirely — their contents may contain anything, and reading code
   aloud is noise.
2. Remove inline code spans, URLs, and image/link syntax; keep link text.
3. Strip markdown emphasis and heading marks, collapse runs of whitespace.
4. If nothing remains, return `None`.
5. Under 200 characters — counted on the cleaned text, not the original — return it unchanged.
6. Otherwise: take the first two sentences (splitting on `.`, `!`, `?`, `…` followed by space
   or end), append " The rest is on screen." If no sentence boundary exists, cut at a word
   boundary near 200 characters and append the same tail.

### `src-tauri/src/speech.rs` (new) — the shell side

Mirrors `ollama.rs`: one owned child process, explicit start and stop.

```rust
pub fn speak(text: &str, voice: &str, rate: i32); // spawns, returns immediately
pub fn stop();                                   // kills the current child, if any
pub fn is_speaking() -> bool;
```

- Spawns hidden PowerShell (`CREATE_NO_WINDOW`), loading `System.Speech` and calling the
  blocking `Speak`.
- **Text and voice name travel in environment variables** (`ALFRED_SPEECH_TEXT`,
  `ALFRED_SPEECH_VOICE`), never interpolated into script source — the same rule as
  `ALFRED_ARG`, so model output cannot become code.
- Rate is a `-10..=10` integer validated before use.
- One child at a time: speaking again stops the previous one first, so answers cannot overlap.
- The child is killed on app exit.

### Where it is triggered — `src-tauri/src/lib.rs`

- The `ask` command gains `voice: bool` (default `false`).
- After the turn emits `Final`, if `voice` and `speech.enabled` and `to_speech` returned
  `Some(text)`, spawn speech **after** `Phase::Done` is emitted, on a background task, so the
  island is never waiting on audio.
- `Ctrl+Shift+Space` (dictation) while speaking stops speech first, then records.
- `Esc` stops speech, same as it cancels a turn.

### The renderer — `src/renderer/Island.tsx`

Tracks whether the composer text came from dictation and is unedited:

- `dictated: string | null` set when a transcript arrives.
- Send passes `voice: dictated !== null && form.text === dictated`.
- Any edit to the composer clears `dictated`.
- A small muted line under the composer while speaking is **not** in scope; the orb already
  shows activity.

### Settings — `crates/alfred-core/src/config.rs`

Three values, same precedence as everything else (environment > `settings.json` > default),
plus panel fields and the `env` tag for locked ones:

| Variable | Default | Notes |
|---|---|---|
| `ALFRED_SPEAK` | `true` | master switch; `false` never speaks |
| `ALFRED_VOICE` | unset | voice name as SAPI reports it; blank = system default |
| `ALFRED_SPEECH_RATE` | `0` | clamped to −10..=10 |

`.env.example`, the settings panel, `README.md` and the `AGENTS.md` variable table all gain
these three. `AGENTS.md` also gains the "speech text goes through environment variables" rule
next to the `ALFRED_ARG` one.

Panel placement: a **Voice** row gains two fields (voice, rate) and a **Speak replies**
toggle, next to the existing voice model/endpoint/key.

## Data flow

```
mic → dictation → transcript in composer ──(send, unedited)──▶ agent.ask(voice = true)
                                                                       │
                                            AgentEvent::Final(answer)  ▼
                                              to_speech(answer) ─────────▶ speech::speak
                                              (core, pure)                 (SAPI child)
```

Typed send never reaches the speech branch.

## Error handling

- No matching voice, SAPI missing, or a busy audio device: the child fails, the error is
  logged at `warn` and shown as a muted note in the island. **Never** `Phase::Error` and never
  a failed turn — a silent assistant is acceptable, a turn that dies because audio was busy is
  not.
- Nothing worth speaking (`None`) is silent, not an error.
- Speech is not retried. If SAPI failed once for this answer, it will fail again.
- `stop()` is idempotent: no child, no work.

## Testing

`to_speech` is a pure function, so the whole decision surface is unit-testable in
`cargo test -p alfred-core`:

| Test | Asserts |
|---|---|
| `short_answer_is_read_verbatim` | under the limit, unchanged |
| `long_answer_keeps_two_sentences_and_points_at_the_screen` | two sentences + tail |
| `code_blocks_are_never_read` | fenced block removed entirely |
| `urls_and_markdown_are_not_spoken` | no `https://`, no `**`, no inline backticks |
| `whitespace_is_collapsed` | newlines and runs of spaces become one space |
| `nothing_to_say_is_none` | empty, code-only, and whitespace-only all return `None` |

The PowerShell child is verified by hand (one utterance, one stop) — asking SAPI to talk in CI
would be flaky, and the CI Windows job only builds, it does not run audio.

## Out of scope

- Streaming sentence-by-sentence speech (the summariser is unchanged if this is added).
- A model-generated spoken summary.
- Voice output for typed turns, wake words or always-listening.
- Any TTS provider other than SAPI, and any new crate.

## Risks

- **Voice quality.** Desktop SAPI voices are dated. Windows 11's natural voices can be
  installed in Settings → Speech and are then selectable by name in the panel; the spec does
  not install anything.
- **Spawn cost.** Each utterance starts a PowerShell process (~200–400 ms) before the first
  syllable. Acceptable for answers; noticeable if this were used for per-word feedback.
- **Half-duplex.** Recording is blocked while speaking, otherwise the mic hears Alfred. This is
  a deliberate limitation, not an oversight.
