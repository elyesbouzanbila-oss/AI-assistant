# Implementation plan: acting reliability

**Spec:** `docs/superpowers/specs/2026-11-26-acting-reliability-design.md`
**Branch:** `feat/acting-reliability`

Order matters only where a later step compiles against an earlier one. Each step ends with a
green `cargo test -p alfred-core`; the whole set ends with clippy and a workspace build.

## 1. `Platform` grows three methods

`crates/alfred-core/src/platform/mod.rs`

- `fn scroll(&self, clicks: i32, position: Option<(i32, i32)>) -> PlatformFuture<'_>;`
- `fn get_clipboard(&self) -> PlatformFuture<'_>;`
- `fn set_clipboard<'a>(&'a self, text: &'a str) -> PlatformFuture<'a>;`

`crates/alfred-core/src/platform/stub.rs`: three `unsupported()` bodies, using the existing
macro where the shape allows.

`crates/alfred-core/src/platform/win.rs`:

- `scroll`: `SetCursorPos` when `position` is `Some`, then one `SendInput` call carrying
  `MOUSEEVENTF_WHEEL` with `mouseData = clicks * 120`. Reuse the input helpers `click` already
  uses; do not duplicate the struct definitions.
- `get_clipboard`: hidden PowerShell running
  `try { Get-Clipboard -Raw } catch { $_.Exception.Message }`, `CREATE_NO_WINDOW`, stdout only.
- `set_clipboard`: same, with `Set-Clipboard -Value $env:ALFRED_ARG` and the text passed in the
  environment, never in the script.

Acceptance: `cargo build --workspace` on Windows compiles; stub still builds on Linux.

## 2. Policy

`crates/alfred-core/src/policy.rs`

- Rename `READ_ONLY` → `SAFE`, add `wait`, `list_files`, `clipboard_set`.
- `MUTATING` → `CONFIRM`, add `scroll`.
- Before either list: `clipboard_get` → `Confirm` with the reason from the spec.
- Add `list_files` to the workspace-jail check next to `read_file`.

Tests: one table test asserting the five verdicts, plus `clipboard_get_needs_confirmation`.

## 3. Tools

`crates/alfred-core/src/tools/mod.rs`

- Five schemas, inserted in a readable order (motion tools together, clipboard after files).
  The count assertion in `every_tool_has_a_schema_and_a_summary` becomes 16.
- `summarise` arms for all five, exact strings from the spec's chip table.
- `execute` arms:
  - `wait`: parse the two modes; `tokio::time::sleep`; window mode polls `ctx.platform
    .list_windows()` every 150 ms up to `timeout_ms`, comparing lowercase substrings; timeout
    error includes the window list.
  - `scroll`: validate `clicks` and optional `x`/`y`, call `platform.scroll`.
  - `list_files`: resolve in the workspace, `read_dir`, collect `(name, is_dir, len)`, sort
    directories first then name, cap 200 with an omitted count.
  - `clipboard_get` / `clipboard_set`: validate the cap, call the platform, word the result.

Tests (with a small fake `Platform` inside the module — the current tests use
`platform::current()`, which is the stub off Windows, so clipboard and scroll need a fake):

- `wait_rejects_both_or_neither_mode`, `wait_rejects_zero_ms`
- `wait_for_window_returns_when_the_title_appears`
- `wait_for_window_timeout_lists_open_windows`
- `list_files_is_jailed_to_the_workspace`
- `list_files_caps_the_listing`
- `clipboard_set_never_scripts` (asserts the fake received the text and that the tool text
  reports the character count)

## 4. Prompt

`crates/alfred-core/src/agent/mod.rs`

- Add the new abilities to "What you can do".
- Add the rule: after opening an app, wait for its window by title instead of guessing a delay.
- Leave the existing framing assertions alone; add one assertion that the prompt mentions
  scrolling and waiting.

## 5. Docs

- `README.md`: five rows in the "What it can do" table; one line in Safety about clipboard
  reads needing approval.
- `AGENTS.md`: clipboard text travels in `ALFRED_ARG`; policy split mention.

## 6. Verification

```
cargo test -p alfred-core
cargo clippy -p alfred-core --all-targets -- -D warnings
cargo build --workspace
npm run build
```

Manual, on this machine (not in CI): scroll a long page, wait for a window title, copy a line
and paste it, list `~/Alfred/workspace` through the app.

## Risks while implementing

- `MouseInput` struct reuse: `click` already builds one; extend rather than copy.
- `read_dir` ordering is filesystem-defined; sort explicitly or the cap test is flaky.
- The `wait` poll test must not depend on wall-clock precision: assert the window was found and
  that it took at least one poll, not an exact duration.
