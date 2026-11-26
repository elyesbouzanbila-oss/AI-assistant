//! The island window: sizing per phase, position at the top centre, and
//! click-through while idle.
//!
//! Tauri has no "forward mouse moves while ignoring clicks" mode, so hover is
//! detected from Rust: while the island is idle a small thread polls the cursor
//! and only passes clicks through when the cursor is outside the pill. Every
//! error path fails towards "accept clicks", so the island can never end up in
//! the classic stuck-click-through state.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::{LogicalSize, PhysicalPosition, WebviewWindow};

/// Set by `apply_phase`; the passthrough thread reads it.
static IDLE: AtomicBool = AtomicBool::new(true);
/// Last phase change, so the poller can leave the window alone while a resize
/// is settling. A stale sample mid-resize makes the island close under a
/// stationary cursor and reopen on the next poll - a rapid blink.
static LAST_PHASE_CHANGE: AtomicU64 = AtomicU64::new(0);

const TOP_MARGIN: f64 = 12.0;
const SETTLE: Duration = Duration::from_millis(250);

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Logical size of the island for a phase. Mirrors `PHASE_SIZE` in
/// `src/renderer/lib/types.ts`.
pub fn size_for(phase: &str) -> (f64, f64) {
    match phase {
        "idle" => (232.0, 48.0),
        "open" => (560.0, 248.0),
        "settings" => (560.0, 700.0),
        "working" => (560.0, 96.0),
        "confirm" => (560.0, 172.0),
        "done" => (560.0, 72.0),
        "error" => (560.0, 132.0),
        _ => (560.0, 96.0),
    }
}

/// Resize, re-centre and update the click-through state for a phase.
pub fn apply_phase(window: &WebviewWindow, phase: &str) {
    let (width, height) = size_for(phase);
    LAST_PHASE_CHANGE.store(now_millis(), Ordering::SeqCst);

    let _ = window.set_size(LogicalSize::new(width, height));
    centre_top(window, width);

    IDLE.store(phase == "idle", Ordering::SeqCst);

    // Let the poll decide while idle; be permissive until it does. Outside idle
    // every click should land anyway.
    let _ = window.set_ignore_cursor_events(false);
    if phase != "idle" {
        let _ = window.set_focus();
    }
}

fn centre_top(window: &WebviewWindow, width: f64) {
    let Ok(Some(monitor)) = window.current_monitor() else {
        return;
    };

    let scale = monitor.scale_factor();
    let monitor_size = monitor.size();
    let monitor_position = monitor.position();

    let width_px = width * scale;
    let x = monitor_position.x as f64 + (monitor_size.width as f64 - width_px) / 2.0;
    let y = monitor_position.y as f64 + TOP_MARGIN * scale;

    let _ = window.set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32));
}

/// Poll the cursor and toggle click-through while the island is idle.
pub fn spawn_passthrough_loop(window: WebviewWindow) {
    std::thread::spawn(move || {
        let mut ignoring = false;
        loop {
            // Only the idle pill needs hover tracking; while open or working
            // every click should land anyway, so do not wake 12 times a second.
            let delay = if IDLE.load(Ordering::SeqCst) { 80 } else { 400 };
            std::thread::sleep(Duration::from_millis(delay));

            if !IDLE.load(Ordering::SeqCst) {
                if ignoring && window.set_ignore_cursor_events(false).is_ok() {
                    ignoring = false;
                }
                continue;
            }

            // `apply_phase` just made the window interactive. Do not second-
            // guess it from geometry sampled while the OS is still resizing.
            let last_change = LAST_PHASE_CHANGE.load(Ordering::SeqCst);
            if now_millis().saturating_sub(last_change) < SETTLE.as_millis() as u64 {
                ignoring = false;
                continue;
            }

            let (Ok(cursor), Ok(position), Ok(size)) = (
                window.cursor_position(),
                window.outer_position(),
                window.outer_size(),
            ) else {
                // Unknown geometry: make sure clicks land on the island.
                if ignoring && window.set_ignore_cursor_events(false).is_ok() {
                    ignoring = false;
                }
                continue;
            };

            let inside = cursor.x >= position.x as f64
                && cursor.y >= position.y as f64
                && cursor.x <= (position.x + size.width as i32) as f64
                && cursor.y <= (position.y + size.height as i32) as f64;

            let want_ignore = !inside;
            if want_ignore != ignoring && window.set_ignore_cursor_events(want_ignore).is_ok() {
                ignoring = want_ignore;
            }
        }
    });
}

/// Put the island at its idle position on first show.
pub fn park(window: &WebviewWindow) {
    apply_phase(window, "idle");
}
