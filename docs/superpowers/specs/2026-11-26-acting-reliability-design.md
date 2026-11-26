# Acting reliability: wait, scroll, clipboard, list files

**Date:** 2026-11-26
**Status:** approved for implementation
**Scope:** five tools that make multi-step desktop work possible with the model Jarvis already
has. No new crates, no new configuration, no change to the safety model beyond one new
confirmation.

## Why

Jarvis can open, click and type, but it cannot pace itself or look around, and that is where
real tasks break:

- Nothing lets it wait for a window to draw, so every second step is a guess about timing.
  "Open Discord and type this" fails because the click lands on a window that has not appeared.
- It can click an item in a list but never move through the list — `click` is absolute
  coordinates and there is no wheel input.
- The file tools read and write a path but cannot discover one. `~/Jarvis/workspace` is
  invisible to the model unless it already knows the filename.
- "Take what I copied and put it in Notepad" means retyping text the user already has.

Each of these is a small, deterministic addition. None of them needs a model to be smarter;
they remove the places where the only remaining strategy is guessing.

## Decisions

| Decision | Choice | Why |
|---|---|---|
| `wait` modes | Fixed milliseconds **and** wait-for-window | A blind sleep is only as good as the model's guess. Waiting for a title is a fact, not a guess, and it is what makes `open_app` followed by `click` reliable. |
| Where window waiting lives | `tools` polling `Platform::list_windows`, every 150 ms | `list_windows` already exists on every backend and already returns titles. A new trait method would duplicate it and need a stub plus a Windows implementation for no gain. |
| `scroll` | `SendInput` wheel events at the pointer, optionally moved to `x,y` first | Same mechanism as `click`, which is already trusted. `press_keys("pagedown")` scrolls the *focused* pane, which is not the pane being pointed at. |
| Clipboard transport | Two hidden-PowerShell calls, text through `JARVIS_ARG` | Matches the window-control pattern and keeps the existing rule: model-supplied text never becomes script source. |
| `clipboard_get` verdict | **Confirm** (Allow chip) | The clipboard is where password managers and copy-password buttons put secrets. A read that runs without a click is a quiet exfiltration path to a remote model. |
| `clipboard_set` verdict | Safe (no chip) | It changes what will be pasted, not the machine; the user asked for a copy and sees the chip text saying so. |
| `list_files` reach | Workspace only, one level | The file jail is absolute (`resolve_in_workspace`). Discovery must not become a way around it. |
| Verification | Not in this spec | Automatic screenshots after every mutation cost an image per step and need a vision model; `JARVIS_VISION=false` must stay a fully working configuration. |

## Architecture

Five tools, two new `Platform` methods, one policy change, one prompt change. Everything else
is reuse.

| Tool | Arguments | Verdict | Backend |
|---|---|---|---|
| `wait` | `{ms}` or `{window, timeout_ms?}` | Safe | `tokio::time::sleep`; `list_windows` polling |
| `scroll` | `{clicks, x?, y?}` | Confirm | `Platform::scroll` (Win32 `SendInput`) |
| `list_files` | `{path?}` | Safe | `std::fs`, jailed |
| `clipboard_get` | `{}` | **Confirm** | `Platform::get_clipboard` (PowerShell) |
| `clipboard_set` | `{text}` | Safe | `Platform::set_clipboard` (PowerShell) |

Schema count goes from 11 to 16.

### `wait`

```jsonc
{ "ms": 1500 }                          // sleep, 1..=10_000
{ "window": "Discord", "timeout_ms": 5000 }  // poll until a title contains it
```

- `ms` and `window` are mutually exclusive; neither present is an error, both present is an
  error. `timeout_ms` defaults to 5 000 and is clamped to 10 000.
- Window matching is case-insensitive `contains` against `list_windows` output, the same rule
  `focus_window` uses.
- Success: `"waited 1.2 s for a window matching “Discord”"`. Timeout: an error that also
  reports the windows that *do* exist, so the model can correct the name instead of repeating
  the same guess:

  ```
  no window matching "Discord" appeared within 5000 ms; open windows: Discord.exe | Spotify | Settings
  ```

- Milliseconds returns `"waited 1.5 s"`.

### `scroll`

```jsonc
{ "clicks": -3 }              // three notches up, at the current pointer
{ "clicks": 5, "x": 720, "y": 400 }   // move there first, then five notches down
```

- `clicks` is required and must be non-zero, in `-10..=10`; positive scrolls down.
- One click is one wheel notch (`WHEEL_DELTA`, 120 units) — the smallest step a wheel-driven
  list actually responds to.
- With `x,y`, the cursor moves first (`SetCursorPos`), then the wheel events go to whatever is
  under it. Without them, the wheel goes wherever the pointer already is, which is the same
  place `click` last left it.
- Result: `"scrolled down 3"`.

### `list_files`

```jsonc
{}                                  // the workspace root
{ "path": "notes" }                 // a subdirectory
```

- Resolved through `resolve_in_workspace`; anything that escapes it is refused with the same
  wording `read_file` uses.
- One level, no recursion: `name`, directory marker, byte size. Sorted by name, directories
  first. At most 200 entries; beyond that the result says how many were omitted.
- A missing path is an error naming it. An empty directory returns `"(empty)"`.
- Result example:

  ```
  12 entries in notes:
  drafts/ (dir)
  today.txt (1.2 KB)
  ...
  ```

### Clipboard

- **Read** (`clipboard_get`): `Get-Clipboard -Raw` in a hidden PowerShell, wrapped in
  `try/catch` that writes `$_.Exception.Message` to stdout, per the existing PowerShell rule.
  Capped at 64 KB (`MAX_READ_BYTES`, the same limit as `read_file`). Empty or whitespace-only
  returns `"(the clipboard is empty)"`.
- **Write** (`clipboard_set`): `Set-Clipboard -Value $env:JARVIS_ARG`. Returns
  `"copied 84 characters"`.
- Neither path ever puts text into the script source.

### `Platform` additions

```rust
/// Wheel notches at the pointer, positive = down. Optionally move the cursor first.
fn scroll(&self, clicks: i32, position: Option<(i32, i32)>) -> PlatformFuture<'_>;
fn get_clipboard(&self) -> PlatformFuture<'_>;
fn set_clipboard<'a>(&'a self, text: &'a str) -> PlatformFuture<'a>;
```

`stub.rs` reports the usual "desktop control is only implemented for Windows" error.

### Policy (`policy.rs`)

- `SAFE` (today's `READ_ONLY`, renamed because it now also holds a write that needs no chip):
  `list_windows`, `screenshot`, `read_file`, `system_info`, `wait`, `list_files`,
  `clipboard_set`.
- `CONFIRM` (today's `MUTATING`, plus `scroll`): `open_app`, `close_app`, `focus_window`,
  `type_text`, `press_keys`, `click`, `write_file`, `scroll`.
- `clipboard_get` is checked before both lists and always returns Confirm with the reason
  `"the clipboard can hold passwords; approve only if you meant to share it"`.
- Unknown tools stay Blocked.
- The jail checks for `read_file`, `write_file` and now `list_files` stay where they are.

### Prompt (`system_prompt`)

- "What you can do" lists the new abilities: scroll, wait for a window, read and write the
  clipboard, list files.
- One new rule: *after opening an app, wait for its window by title instead of guessing at a
  delay*.
- The existing framing assertions in `agent::tests` are unchanged.

### Audit

No new paths. Every call reaches `audit.log` through the existing `Agent::ask` loop, with
chip text from `tools::summarise`:

| Tool | Chip |
|---|---|
| `wait` | `Wait 1.5 s` / `Wait for “Discord”` |
| `scroll` | `Scroll down 3` |
| `list_files` | `List files in notes` |
| `clipboard_get` | `Read the clipboard` |
| `clipboard_set` | `Copy 84 characters` |

## Data flow

```
model → tools::execute → policy::evaluate ──Confirm──▶ island Allow chip → platform
                                   │                          │
                                   └──────── Safe ────────────┘
                                         │
                     wait: core sleep or list_windows poll
                     scroll / clipboard: win.rs (SendInput / hidden PowerShell)
                     list_files: fs, jailed by resolve_in_workspace
```

## Error handling

- Every tool returns model-visible text or an `Err` that becomes `ERROR: …` in the tool
  message. Nothing panics on bad input: `clicks: 0`, `ms: 0`, both `wait` modes at once, and a
  path outside the workspace are all errors with the reason.
- A `wait` timeout is an error, not a success with a warning, so the model does not proceed as
  if the window is there.
- Clipboard failures (no `Get-Clipboard` on this Windows build, a locked clipboard) surface the
  PowerShell message; they never end the turn.
- A denied `scroll` or `clipboard_get` behaves like any other denial: the model is told not to
  retry and to ask what the user prefers.

## Testing

`cargo test -p jarvis-core`, no Windows and no network needed.

| Test | Asserts |
|---|---|
| `every_tool_has_a_schema_and_a_summary` | count is 16 and every name has a chip |
| `scroll_and_clipboard_have_chips` | the five new summaries read as written above |
| `wait_rejects_both_or_neither_mode` | `{}`, `{ms, window}` are errors; `{ms: 0}` too |
| `wait_for_window_returns_when_the_title_appears` | fake platform reports the window on its second poll; returns success in ~150 ms |
| `wait_for_window_timeout_lists_open_windows` | the error names the windows that exist |
| `list_files_is_jailed_to_the_workspace` | `../` and absolute paths are refused; a `tempdir` listing works |
| `list_files_caps_the_listing` | 250 files yield 200 entries plus an "omitted" note |
| `clipboard_get_needs_confirmation` | policy verdict is Confirm, not Safe |
| `clipboard_set_is_safe_and_never_scripts` | verdict Safe; the command argument is `JARVIS_ARG`, and the text appears nowhere in it |
| `scroll_is_a_confirmation` | verdict Confirm |

The Windows backends (`SendInput` wheel, `Get-Clipboard`, `Set-Clipboard`) are verified by hand
on this machine: scroll a long page, copy a line, paste it, read it back. CI's Windows job
builds them but does not drive the desktop.

## Documentation

- `README.md`: five rows in the "What it can do" table, and the safety section gains one line
  about clipboard reads needing approval.
- `AGENTS.md`: the "OS actions go through PowerShell" area gains the clipboard rule (text in
  `JARVIS_ARG`, never in the script), and the policy paragraph mentions the new verdict split.
- `.env.example`: unchanged — nothing here is configurable.

## Out of scope

- Automatic verification screenshots after a mutation, and retry logic.
- Drag and drop, hover, right-click, double-click.
- A UI-automation/accessibility tree (element names instead of coordinates).
- Recursive listing, file move/delete, or any path outside the workspace.
- Wake-word, scheduling, or memory work — separate specs, in that order after this one.

## Risks

- **Wheel events go to the pointer, not the app.** If the cursor is over the island when
  `scroll` runs, nothing scrolls. Mitigated by the optional `x,y` and by the prompt rule to
  point first; not eliminated.
- **Polling `list_windows`** costs a Win32 enumeration every 150 ms for up to 10 s. Cheap, but
  it is real work; the cap exists for that reason.
- **Clipboard text in the model's context.** Even with an approval, a password copied a minute
  ago can be approved by reflex. The chip text says "Read the clipboard" so the decision is at
  least explicit.
- **`wait` invites long turns.** With `max_steps` at 8, a turn that waits several times can look
  hung; the island already shows the step summary while it works.
