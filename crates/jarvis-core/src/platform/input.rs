//! Keyboard and mouse injection via `enigo` (SendInput on Windows).
//!
//! `enigo` is cross-platform, which means these exact calls are compiled and
//! type-checked during development even on a machine that is not Windows.

use anyhow::{Context, Result};
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};

use super::combo::{self, Modifier, Named};

fn enigo() -> Result<Enigo> {
    Enigo::new(&Settings::default()).context("could not initialise input injection")
}

/// Type literal text into whatever has focus.
pub fn type_text(text: &str) -> Result<()> {
    let mut enigo = enigo()?;
    enigo.text(text).context("could not type text")?;
    Ok(())
}

/// Press a chord such as `ctrl+s`, holding modifiers for the whole sequence.
pub fn press_keys(combo_text: &str) -> Result<()> {
    let combo = combo::parse(combo_text)?;
    let mut enigo = enigo()?;

    let modifier_keys: Vec<Key> = combo
        .modifiers
        .iter()
        .map(|modifier| match modifier {
            Modifier::Ctrl => Key::Control,
            Modifier::Shift => Key::Shift,
            Modifier::Alt => Key::Alt,
            Modifier::Super => Key::Meta,
        })
        .collect();

    for key in &modifier_keys {
        enigo
            .key(*key, Direction::Press)
            .context("could not hold modifier")?;
    }

    let mut result = Ok(());
    for named in &combo.keys {
        if let Err(error) = enigo.key(to_key(*named), Direction::Click) {
            result = Err(anyhow::anyhow!("could not press key: {error}"));
            break;
        }
    }

    // Always release modifiers, even if a key failed.
    for key in modifier_keys.iter().rev() {
        let _ = enigo.key(*key, Direction::Release);
    }

    result
}

pub fn click(x: i32, y: i32) -> Result<()> {
    let mut enigo = enigo()?;
    enigo
        .move_mouse(x, y, Coordinate::Abs)
        .context("could not move the mouse")?;
    enigo
        .button(Button::Left, Direction::Click)
        .context("could not click")?;
    Ok(())
}

fn to_key(named: Named) -> Key {
    match named {
        Named::Char(c) => Key::Unicode(c),
        Named::Enter => Key::Return,
        Named::Tab => Key::Tab,
        Named::Escape => Key::Escape,
        Named::Space => Key::Space,
        Named::Backspace => Key::Backspace,
        Named::Delete => Key::Delete,
        Named::Up => Key::UpArrow,
        Named::Down => Key::DownArrow,
        Named::Left => Key::LeftArrow,
        Named::Right => Key::RightArrow,
        Named::Home => Key::Home,
        Named::End => Key::End,
        Named::PageUp => Key::PageUp,
        Named::PageDown => Key::PageDown,
        Named::Insert => Key::Insert,
        Named::PrintScreen => Key::PrintScr,
        Named::Function(n) => match n {
            1 => Key::F1,
            2 => Key::F2,
            3 => Key::F3,
            4 => Key::F4,
            5 => Key::F5,
            6 => Key::F6,
            7 => Key::F7,
            8 => Key::F8,
            9 => Key::F9,
            10 => Key::F10,
            11 => Key::F11,
            _ => Key::F12,
        },
    }
}
