//! What to read aloud.
//!
//! A voice answer cannot be skimmed, so the decision of *what* is spoken lives
//! here as a pure function: no audio, no Windows, no window handle. CI runs on
//! Linux, where the speech path can never be exercised, so this is the only part
//! of the voice loop that can be tested at all - and it is the part with the
//! interesting rules.

/// Characters of cleaned text read verbatim before the answer is cut down to a
/// headline. A sentence or two: long enough to be useful, short enough that a
/// spoken reply never turns into a transcript.
const SPOKEN_LIMIT: usize = 200;

/// Sentences kept when the answer is longer than that.
const SPOKEN_SENTENCES: usize = 2;

/// Said instead of the rest of a long answer, which is already on screen.
const TAIL: &str = " The rest is on screen.";

/// Everything worth hearing, or `None` when there is nothing to say.
pub fn to_speech(answer: &str) -> Option<String> {
    let clean = strip(answer);
    if clean.is_empty() {
        return None;
    }
    if clean.chars().count() <= SPOKEN_LIMIT {
        return Some(clean);
    }
    Some(format!("{}{TAIL}", headline(&clean)))
}

/// Drop what cannot be read meaningfully aloud: fenced code (its contents may be
/// anything), inline code, URLs and markdown punctuation. Link text is kept,
/// because it is usually the part a person would say.
fn strip(answer: &str) -> String {
    let mut kept = String::new();
    let mut fenced = false;

    for line in answer.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        strip_line(line, &mut kept);
        kept.push(' ');
    }

    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_line(line: &str, out: &mut String) {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            // Inline code: spoken as nothing, since it is usually an identifier.
            '`' => {
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    i += 1;
                }
                i += 1;
            }
            '[' => match (find(&chars, i + 1, ']'), find(&chars, i + 1, ')')) {
                (Some(close), Some(paren)) if chars.get(close + 1) == Some(&'(') && paren > close => {
                    let text: String = chars[i + 1..close].iter().collect();
                    out.push_str(&text);
                    i = paren + 1;
                }
                _ => {
                    out.push(chars[i]);
                    i += 1;
                }
            },
            // A bare URL is punctuation soup when spoken.
            'h' if starts_with(&chars, i, "http://")
                || starts_with(&chars, i, "https://")
                || starts_with(&chars, i, "www.") =>
            {
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], ')' | ']' | '>')
                {
                    i += 1;
                }
            }
            // Markdown decoration. `_` is deliberately kept: it is far more often
            // part of an identifier like `read_file` than an italic marker.
            '*' | '#' | '>' | '|' | '~' => i += 1,
            '-' if out.is_empty() || out.ends_with(' ') => i += 1,
            _ => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
}

fn find(chars: &[char], from: usize, needle: char) -> Option<usize> {
    chars
        .iter()
        .skip(from)
        .position(|c| *c == needle)
        .map(|offset| from + offset)
}

fn starts_with(chars: &[char], at: usize, needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(offset, expected)| chars.get(at + offset) == Some(&expected))
}

/// The first `SPOKEN_SENTENCES` sentences, or a word-boundary cut when the text
/// has no sentence punctuation at all.
fn headline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut sentences = 0;

    for (i, c) in chars.iter().enumerate() {
        out.push(*c);
        // A dot inside "3.5" is not the end of a sentence.
        let ends = matches!(c, '.' | '!' | '?' | '…')
            && chars.get(i + 1).is_none_or(|next| next.is_whitespace());
        if ends {
            sentences += 1;
            if sentences == SPOKEN_SENTENCES {
                break;
            }
        }
    }

    if sentences == 0 {
        return cut_at_word(text, SPOKEN_LIMIT);
    }
    out.trim_end().to_string()
}

fn cut_at_word(text: &str, limit: usize) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if out.chars().count() + word.chars().count() + 1 > limit {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.is_empty() {
        text.chars().take(limit).collect()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_answer_is_read_verbatim() {
        assert_eq!(
            to_speech("Opened Notepad.").as_deref(),
            Some("Opened Notepad.")
        );
    }

    #[test]
    fn long_answer_keeps_two_sentences_and_points_at_the_screen() {
        let answer = "The build failed. The linker could not find libfoo. \
            Install the development package from your distribution, clear the target \
            directory, then rebuild the workspace and run the whole test suite again \
            before pushing anything at all to main.";
        assert!(answer.chars().count() > SPOKEN_LIMIT);

        let spoken = to_speech(answer).expect("something to say");

        assert!(spoken.ends_with(" The rest is on screen."), "{spoken}");
        assert!(spoken.starts_with("The build failed. The linker"), "{spoken}");
        assert!(!spoken.contains("Install"), "{spoken}");
    }

    #[test]
    fn code_blocks_are_never_read() {
        let answer = "Run this first:\n\n```bash\nrm -rf / --no-preserve-root\n```\n\nThen tell me what it printed.";
        let spoken = to_speech(answer).expect("something to say");

        assert!(!spoken.contains("rm -rf"), "{spoken}");
        assert!(spoken.contains("Run this first:"), "{spoken}");
        assert!(spoken.contains("Then tell me what it printed."), "{spoken}");
    }

    #[test]
    fn urls_and_markdown_are_not_spoken() {
        let spoken = to_speech("See [the docs](https://example.com/docs) for the **exact** flags.")
            .expect("something to say");

        assert_eq!(spoken, "See the docs for the exact flags.");
    }

    #[test]
    fn whitespace_is_collapsed() {
        assert_eq!(
            to_speech("Line one.\n\n   Line two.  ").as_deref(),
            Some("Line one. Line two.")
        );
    }

    #[test]
    fn nothing_to_say_is_none() {
        assert_eq!(to_speech(""), None);
        assert_eq!(to_speech("   \n\t "), None);
        assert_eq!(to_speech("```\nrm -rf /\n```"), None);
    }
}
