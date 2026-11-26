# Memory: survive a restart, learn facts, stop asking the same question

**Date:** 2026-11-26
**Status:** approved for implementation
**Scope:** three files under `~/Alfred/`, one new tool, one new chip button, two panel
buttons. No new crates, no new configuration, nothing leaves the machine.

## Why

Alfred forgets everything. Closing the island drops the visible conversation; rebuilding the
runtime (which a settings save does) throws away the `Agent` and with it the model's history,
mid-session. It also re-learns nothing: every "open Spotify" raises an Allow chip forever, and
anything you tell it about yourself has to be repeated next time.

Three small pieces fix that, and each is independently testable.

## Decisions

| Decision | Choice | Why |
|---|---|---|
| What a restart restores | The island's last 40 turns, and the last 12 **text** messages replayed to the model | A follow-up after a restart should make sense. Tool calls, results and screenshots are deliberately not stored: replaying them out of order breaks provider payloads, and a stale screenshot is worse than none. |
| Where the transcript lives | `~/Alfred/transcript.json` | Same place and trust level as `settings.json` and `audit.log`. Plain JSON so it can be read or deleted by hand. |
| Facts | `~/Alfred/memory.md`, written by a `remember` tool | The model is the thing that hears "I use VS Code"; a panel-only editor would mean retyping what it just learned. Markdown so the file stays human-readable. |
| Fact size | 500 chars per fact, 1 500 chars injected | A facts file that grows without bound would quietly eat the context window. The file keeps everything; only the injection is capped, newest first. |
| Approvals | `~/Alfred/approvals.json`, only for `open_app`, `close_app`, `focus_window` | These are the Confirm tools with a stable target. `click`, `type_text`, `press_keys` and `write_file` have no identity to remember, and `clipboard_get` has no target at all — it asks every time. |
| How it is granted | A third chip button, "Always allow" | Explicit. Silently remembering every Allow would broaden permissions without the user choosing to. |
| Revoking | Two panel buttons and the file itself | A permission you cannot see or drop is not a permission, it is a bug. |

## Architecture

### `crates/alfred-core/src/memory.rs` (new)

The only module that reads or writes these files. No Tauri, no platform, so it tests on Linux.

```rust
pub struct Memory { dir: PathBuf }

impl Memory {
    /// `~/Alfred` in the app; a tempdir in tests.
    pub fn new(dir: impl Into<PathBuf>) -> Self;

    /// Oldest first, at most TRANSCRIPT_TURNS, text only.
    pub fn transcript(&self) -> Vec<Turn>;
    pub fn append_turn(&self, role: Role, text: &str);
    pub fn forget_conversation(&self);

    pub fn remember(&self, fact: &str) -> Result<()>;
    /// Capped at FACTS_INJECTED_CHARS, newest first.
    pub fn facts(&self) -> String;

    pub fn is_allowed(&self, tool: &str, target: &str) -> bool;
    pub fn allow(&self, tool: &str, target: &str) -> Result<()>;
    pub fn approvals(&self) -> Vec<Approval>;
    pub fn forget_approvals(&self);
}

pub const TRANSCRIPT_TURNS: usize = 40;
pub const REPLAYED_MESSAGES: usize = 12;
pub const FACT_CHARS: usize = 500;
pub const FACTS_INJECTED_CHARS: usize = 1_500;
/// The only tools whose approval can be remembered.
pub const REMEMBERABLE: [&str; 3] = ["open_app", "close_app", "focus_window"];
```

- `transcript.json`: `{"turns": [{"role": "user", "text": "..."}]}`. Truncated to 40 turns on
  every write; each turn trimmed to 4 096 bytes on a character boundary.
- `memory.md`: one `- fact` line per fact. Exact duplicates are ignored. A fact is trimmed and
  clipped to 500 characters before it is written.
- `approvals.json`: `[{"tool": "open_app", "target": "spotify"}]`. `tool` must be in
  `REMEMBERABLE` or `allow` returns an error; `target` is trimmed and lowercased.
- A missing or corrupt file is an empty value, never an error: the next write replaces it.
  Writes are read-modify-write, best effort, and log at `warn` when they fail.

### Agent

- `Agent::new(config, platform, completer, audit, emit, memory)` — one more argument, loaded
  from `alfred_dir()` by the shell and from a tempdir by tests.
- On construction it replays `transcript()[-(REPLAYED_MESSAGES)..]` as `ChatMessage::user` /
  `ChatMessage::assistant` text. Nothing is emitted to the island from this; the island asks
  for the transcript itself.
- `ask()` appends the user turn before the loop starts; a successful `Final` appends the
  answer. Errors and cancellations append only the user turn, which is what the island shows
  too.
- After `policy::evaluate` returns `Confirm` and before `request_permission`, `ask()` checks
  `memory.is_allowed(tool, target)`. A hit audits `allowed by memory` and runs without a chip.
  A `Blocked` verdict is never consulted against memory.
- `ConfirmRequest` gains `remember: bool`: true when the tool is in `REMEMBERABLE` and the
  call has a target.
- `Agent::confirm(id, allow, remember)`: when `allow && remember`, the approval is written
  before the turn continues.

### Policy

Unchanged. `evaluate` still returns the verdict; memory only short-circuits the UI prompt for
`Confirm`, so a remembered approval cannot turn a `Blocked` into a run. The target used for
memory is the same `target_name` the policy already extracts — exported as
`policy::target(tool, args)` so there is one definition, not two.

### Shell (`src-tauri/src/lib.rs`)

- `Runtime::build` passes `Memory::new(config::alfred_dir()?)` to `Agent::new`.
- New commands: `history() -> Vec<TurnView>`, `forget_history()`, `approvals() ->
  Vec<ApprovalView>`, `forget_approvals()`.
- `confirm(id, allow, remember: Option<bool>)`; `None` means false, so an older UI still works.

### Island

- On mount in live mode, `history()` seeds the chat lines (the same shape as a `ChatLine`), so
  the conversation is there after a restart and after a settings save.
- The confirm chip renders **Allow / Always allow / Deny**; the middle button only when
  `pending.remember`.
- Settings panel, new **Memory** section: `N facts · M remembered permissions`, then *Forget
  the conversation* and *Forget remembered permissions*. The facts file is not editable there.

## Data flow

```
application start ──▶ Runtime::build ──▶ Agent::new ──▶ Memory::transcript (last 12 text msgs)
                                    └──▶ history() command ──▶ island repaints 40 turns

ask ──▶ policy::evaluate ── Blocked ──▶ refuse (memory never consulted)
                        └─ Confirm ──▶ Memory::is_allowed?
                                          ├─ yes ──▶ run, audit "allowed by memory"
                                          └─ no  ──▶ chip ──▶ Allow always ──▶ Memory::allow

Final ──▶ Memory::append_turn(assistant) ──▶ transcript.json
remember({text}) ──▶ memory.md ──▶ next system_prompt
```

## Error handling

- None of these files can fail a turn. An unreadable transcript replays nothing; an unwritable
  approvals file means the chip keeps asking; an unwritable facts file makes `remember` return
  a tool error the model can report.
- `remember` rejects blank text and clips at 500 characters instead of failing.
- `forget_*` on a missing file is a no-op success.

## Testing

`cargo test -p alfred-core` only; no Tauri, no network, no real home directory (every test uses
`tempfile::tempdir()`).

| Test | Asserts |
|---|---|
| `transcript_round_trips_and_trims` | appended turns come back in order; the 41st drops the first |
| `transcript_is_plain_text` | no tool-call or image parts survive a round trip |
| `facts_dedupe_and_are_capped` | the same fact twice appears once; the injected text stops at 1 500 characters |
| `approvals_normalise_targets` | `Spotify` and `spotify` match; non-rememberable tools are refused |
| `approvals_survive_a_corrupt_file` | a garbage file reads as empty and the next write fixes it |
| `remembered_approval_skips_the_chip` | a scripted `open_app` runs with no `Confirm` event, once the approval exists |
| `a_fresh_agent_loads_the_last_turns` | new `Agent` on the same tempdir sees the earlier text messages |
| `facts_reach_the_system_prompt` | the prompt contains them, and omits the section when empty |
| `a_finished_turn_writes_both_messages` | transcript has the prompt and the answer |

The Tauri commands and the chip button are compiled and checked by `npm run build` / `cargo
build --workspace`; the button itself is a visual change that needs a manual look.

## Documentation

- `README.md`: a **Memory** section — the three files, what is stored, how to revoke.
- `AGENTS.md`: a row in "Where things live" for `memory.rs`, plus the rules that matter —
  transcript is text-only, approvals are whitelisted, facts are capped, and a remembered
  approval never lifts a policy `Blocked`.

## Out of scope

- Editing facts from the panel, or browsing the transcript there.
- More than one conversation, semantic recall, or pruning by relevance.
- Storing tool results, images or audio.
- Any sync, account or network storage.
- Wake word and scheduling (the next cluster).

## Risks

- **Plaintext on disk.** The transcript and facts are readable by anything that can read your
  home directory, exactly like `settings.json`. Deleting the files is always safe.
- **Stale replay.** Twelve text messages from yesterday can mislead a model about today's
  screen. The replay is short on purpose, and the prompt still says this is a live desktop.
- **Convenience drift.** "Always allow" on `close_app` could close something you did not mean
  later. That is why only three tools are rememberable, the chip says which app, the audit log
  records `allowed by memory`, and the panel can forget them all.
