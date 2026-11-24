use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    Char(char),
    Enter,
    Tab,
    Escape,
    Space,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    PrintScreen,
    Function(u8),
}

/// A parsed chord such as `ctrl+shift+s`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Combo {
    pub modifiers: Vec<Modifier>,
    pub keys: Vec<Named>,
}

/// Parse `ctrl+alt+delete` style input. Modifiers must come first; anything
/// after the first key is treated as a key sequence (so `ctrl+a b` types
/// ctrl+a then b).
pub fn parse(input: &str) -> Result<Combo> {
    let raw = input.trim();
    if raw.is_empty() {
        bail!("empty key combo");
    }

    let mut modifiers = Vec::new();
    let mut keys: Vec<Named> = Vec::new();

    for (index, token) in raw.split('+').map(str::trim).enumerate() {
        if token.is_empty() {
            bail!("malformed key combo: {input}");
        }

        // A literal plus is written as the final token.
        if token == "plus" || (token == "\"+\"") {
            keys.push(Named::Char('+'));
            continue;
        }

        match token.to_lowercase().as_str() {
            "ctrl" | "control" | "ctl" => modifiers.push(Modifier::Ctrl),
            "shift" => modifiers.push(Modifier::Shift),
            "alt" | "option" | "opt" => modifiers.push(Modifier::Alt),
            "win" | "super" | "meta" | "cmd" | "command" => modifiers.push(Modifier::Super),
            _ => {
                if index == 0 {
                    bail!("key combo must start with a modifier: {input}");
                }
                keys.push(parse_key(token)?);
            }
        }
    }

    if keys.is_empty() {
        bail!("key combo has no key: {input}");
    }

    Ok(Combo { modifiers, keys })
}

fn parse_key(token: &str) -> Result<Named> {
    let lower = token.to_lowercase();
    let named = match lower.as_str() {
        "enter" | "return" => Named::Enter,
        "tab" => Named::Tab,
        "escape" | "esc" => Named::Escape,
        "space" => Named::Space,
        "backspace" => Named::Backspace,
        "delete" | "del" => Named::Delete,
        "up" => Named::Up,
        "down" => Named::Down,
        "left" => Named::Left,
        "right" => Named::Right,
        "home" => Named::Home,
        "end" => Named::End,
        "pageup" | "pgup" => Named::PageUp,
        "pagedown" | "pgdn" => Named::PageDown,
        "insert" | "ins" => Named::Insert,
        "printscreen" | "prtsc" => Named::PrintScreen,
        other => {
            if let Some(number) = other.strip_prefix('f') {
                let number: u8 = number
                    .parse()
                    .map_err(|_| anyhow::anyhow!("unknown key: {token}"))?;
                if (1..=24).contains(&number) {
                    Named::Function(number)
                } else {
                    bail!("function key out of range: {token}");
                }
            } else {
                let mut chars = token.chars();
                match (chars.next(), chars.next()) {
                    (Some(single), None) => Named::Char(single),
                    _ => bail!("unknown key: {token}"),
                }
            }
        }
    };
    Ok(named)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_key() {
        let combo = parse("ctrl+shift+S").unwrap();
        assert_eq!(combo.modifiers, vec![Modifier::Ctrl, Modifier::Shift]);
        assert_eq!(combo.keys, vec![Named::Char('S')]);
    }

    #[test]
    fn parses_named_keys_and_function_keys() {
        assert_eq!(parse("ctrl+enter").unwrap().keys, vec![Named::Enter]);
        assert_eq!(parse("alt+f4").unwrap().keys, vec![Named::Function(4)]);
        assert_eq!(parse("ctrl+pageup").unwrap().keys, vec![Named::PageUp]);
    }

    #[test]
    fn supports_key_sequences() {
        let combo = parse("ctrl+a+b").unwrap();
        assert_eq!(combo.keys, vec![Named::Char('a'), Named::Char('b')]);
    }

    #[test]
    fn rejects_nonsense() {
        assert!(parse("").is_err());
        assert!(parse("a").is_err());
        assert!(parse("ctrl+").is_err());
        assert!(parse("ctrl+nosuchkey").is_err());
        assert!(parse("ctrl+f99").is_err());
    }
}
