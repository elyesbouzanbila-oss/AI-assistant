//! What Jarvis keeps between turns and between runs: the conversation
//! transcript, facts about the user, and approvals already granted.
//!
//! Three plaintext files in one directory — `~/Jarvis` in the app, a temporary
//! directory in tests. Nothing here talks to the network, the model or the OS,
//! and every read of a missing or damaged file is an empty value rather than an
//! error: memory should never be the reason a turn fails.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Turns the island may repaint after a restart.
pub const TRANSCRIPT_TURNS: usize = 40;
/// Messages handed back to the model on start. Short on purpose: a stale
/// conversation should hint, not overrule what is on the screen now.
pub const REPLAYED_MESSAGES: usize = 12;
/// One turn is clipped here before it is stored, so a pasted document cannot
/// grow the file without bound.
pub const TURN_CHARS: usize = 4_096;
/// One fact is clipped here.
pub const FACT_CHARS: usize = 500;
/// Only this much of `memory.md` reaches the prompt; the file keeps everything.
pub const FACTS_INJECTED_CHARS: usize = 1_500;
/// The only tools whose approval can be remembered: they name a stable target.
/// `click`, `type_text`, `press_keys` and `write_file` have no identity to
/// remember, and `clipboard_get` has no target at all.
pub const REMEMBERABLE: [&str; 3] = ["open_app", "close_app", "focus_window"];

/// Whether an approval for this tool could be remembered at all.
pub fn is_rememberable(tool: &str) -> bool {
    REMEMBERABLE.contains(&tool)
}

const TRANSCRIPT: &str = "transcript.json";
const FACTS: &str = "memory.md";
const APPROVALS: &str = "approvals.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub tool: String,
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forget {
    Conversation,
    Approvals,
}

/// Files backed by one directory.
#[derive(Debug, Clone)]
pub struct Memory {
    dir: PathBuf,
}

impl Memory {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    // ---------------------------------------------------------- transcript

    /// Oldest first, text only.
    pub fn transcript(&self) -> Vec<Turn> {
        self.stored_transcript().turns
    }

    /// Append one side of a turn. Best effort: a disk error must not fail a turn.
    pub fn append_turn(&self, role: Role, text: &str) {
        let text = clip(text.trim(), TURN_CHARS);
        if text.is_empty() {
            return;
        }

        let mut transcript = self.stored_transcript();
        transcript.turns.push(Turn { role, text });
        let excess = transcript.turns.len().saturating_sub(TRANSCRIPT_TURNS);
        if excess > 0 {
            transcript.turns.drain(..excess);
        }
        self.write(TRANSCRIPT, &transcript);
    }

    pub fn forget(&self, what: Forget) {
        let name = match what {
            Forget::Conversation => TRANSCRIPT,
            Forget::Approvals => APPROVALS,
        };
        match fs::remove_file(self.path(name)) {
            Ok(()) => tracing::info!("forgot {name}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!("could not forget {name}: {error}"),
        }
    }

    fn stored_transcript(&self) -> Transcript {
        fs::read_to_string(self.path(TRANSCRIPT))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    // -------------------------------------------------------------- facts

    /// Facts worth putting in the prompt, newest first, capped in total.
    pub fn facts(&self) -> String {
        let stored = fs::read_to_string(self.path(FACTS)).unwrap_or_default();
        let mut kept = String::new();
        for fact in stored
            .lines()
            .filter_map(|line| line.trim().strip_prefix("- "))
            .rev()
        {
            let fact = fact.trim();
            if fact.is_empty() {
                continue;
            }
            let separator = usize::from(!kept.is_empty());
            // The bullet and its newline are part of the budget, so the
            // injected block can never overshoot by a line.
            let budget = FACTS_INJECTED_CHARS
                .saturating_sub(kept.chars().count() + separator + 2);
            if budget == 0 {
                break;
            }
            if separator == 1 {
                kept.push('\n');
            }
            kept.push_str("- ");
            kept.push_str(&clip(fact, budget));
        }
        kept
    }

    /// How many facts are stored, for the panel.
    pub fn facts_count(&self) -> usize {
        fs::read_to_string(self.path(FACTS))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.trim().starts_with("- "))
            .count()
    }

    /// Append a fact, ignoring an exact repeat. Blank is an error the model can
    /// report, not a silent success.
    pub fn remember(&self, fact: &str) -> Result<()> {
        let fact = clip(fact.trim(), FACT_CHARS);
        if fact.is_empty() {
            bail!("nothing to remember: the fact was empty");
        }

        let path = self.path(FACTS);
        let stored = fs::read_to_string(&path).unwrap_or_default();
        if stored
            .lines()
            .filter_map(|line| line.trim().strip_prefix("- "))
            .any(|known| known.trim() == fact)
        {
            return Ok(());
        }

        let mut next = stored;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str("- ");
        next.push_str(&fact);
        next.push('\n');
        fs::write(&path, next).with_context(|| format!("could not write {}", path.display()))
    }

    // ---------------------------------------------------------- approvals

    pub fn approvals(&self) -> Vec<Approval> {
        self.stored_approvals()
    }

    /// Whether this exact action was allowed before.
    pub fn is_allowed(&self, tool: &str, target: &str) -> bool {
        let target = normalise(target);
        REMEMBERABLE.contains(&tool)
            && !target.is_empty()
            && self
                .stored_approvals()
                .iter()
                .any(|known| known.tool == tool && normalise(&known.target) == target)
    }

    /// Remember an approval. Refuses tools whose identity would be misleading.
    pub fn allow(&self, tool: &str, target: &str) -> Result<()> {
        if !REMEMBERABLE.contains(&tool) {
            bail!("{tool} cannot be remembered: it has no stable target");
        }
        let target = normalise(target);
        if target.is_empty() {
            bail!("{tool} named nothing to remember");
        }
        if self.is_allowed(tool, &target) {
            return Ok(());
        }

        let mut approvals = self.stored_approvals();
        approvals.push(Approval {
            tool: tool.to_string(),
            target,
        });
        self.write(APPROVALS, &approvals);
        Ok(())
    }

    fn stored_approvals(&self) -> Vec<Approval> {
        fs::read_to_string(self.path(APPROVALS))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------ helpers

    fn write<T: Serialize>(&self, name: &str, value: &T) {
        let path = self.path(name);
        let result = serde_json::to_string(value)
            .map_err(anyhow::Error::from)
            .and_then(|text| fs::write(&path, text).context("could not write the file"));
        if let Err(error) = result {
            tracing::warn!("could not save {}: {error:#}", path.display());
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Transcript {
    #[serde(default)]
    turns: Vec<Turn>,
}

fn normalise(target: &str) -> String {
    target.trim().to_lowercase()
}

/// Clip to a character count on a boundary, never mid-code-point.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> (tempfile::TempDir, Memory) {
        let dir = tempfile::tempdir().expect("tempdir");
        let memory = Memory::new(dir.path());
        (dir, memory)
    }

    #[test]
    fn transcript_round_trips_and_trims() {
        let (_dir, memory) = memory();
        assert!(memory.transcript().is_empty());

        for index in 0..TRANSCRIPT_TURNS + 1 {
            memory.append_turn(Role::User, &format!("question {index}"));
            memory.append_turn(Role::Assistant, &format!("answer {index}"));
        }

        let turns = memory.transcript();
        assert_eq!(turns.len(), TRANSCRIPT_TURNS);
        assert_eq!(turns.last().unwrap().text, format!("answer {TRANSCRIPT_TURNS}"));
        // The oldest turns are the ones that went.
        // 82 turns were written; the oldest 42 are the ones that went.
        assert_eq!(turns.first().unwrap().text, "question 21");
    }

    #[test]
    fn a_turn_is_clipped_not_dropped() {
        let (_dir, memory) = memory();
        memory.append_turn(Role::User, &"é".repeat(TURN_CHARS * 2));
        let turns = memory.transcript();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].text.chars().count(), TURN_CHARS);
    }

    #[test]
    fn forgetting_the_conversation_leaves_the_files_alone() {
        let (_dir, memory) = memory();
        memory.append_turn(Role::User, "hello");
        memory.remember("uses VS Code").unwrap();
        memory.forget(Forget::Conversation);
        assert!(memory.transcript().is_empty());
        assert!(memory.facts().contains("uses VS Code"));
    }

    #[test]
    fn facts_dedupe_and_are_capped() {
        let (_dir, memory) = memory();
        memory.remember("likes dark mode").unwrap();
        memory.remember("likes dark mode").unwrap();
        assert_eq!(memory.facts().matches("dark mode").count(), 1);

        for index in 0..20 {
            memory.remember(&format!("fact {index} {}", "x".repeat(200)))
                .unwrap();
        }
        let injected = memory.facts();
        assert!(injected.chars().count() <= FACTS_INJECTED_CHARS);
        // Newest first: the last fact written is the first one injected.
        assert!(injected.starts_with("- fact 19"));
    }

    #[test]
    fn a_blank_fact_is_an_error() {
        let (_dir, memory) = memory();
        assert!(memory.remember("   ").is_err());
        assert!(memory.facts().is_empty());
    }

    #[test]
    fn approvals_normalise_targets() {
        let (_dir, memory) = memory();
        memory.allow("open_app", "Spotify").unwrap();
        assert!(memory.is_allowed("open_app", "spotify"));
        assert!(!memory.is_allowed("open_app", "notepad"));
        // A different action on the same app is its own permission.
        assert!(!memory.is_allowed("close_app", "Spotify"));
        assert_eq!(memory.approvals().len(), 1);
    }

    #[test]
    fn tools_without_a_target_cannot_be_remembered() {
        let (_dir, memory) = memory();
        assert!(memory.allow("type_text", "spotify").is_err());
        assert!(memory.allow("clipboard_get", "").is_err());
        assert!(memory.allow("open_app", "  ").is_err());
        assert!(!memory.is_allowed("type_text", "spotify"));
        assert!(memory.approvals().is_empty());
    }

    #[test]
    fn approvals_survive_a_corrupt_file() {
        let (_dir, memory) = memory();
        fs::write(memory.path(APPROVALS), "not json at all").unwrap();
        assert!(memory.approvals().is_empty());
        assert!(!memory.is_allowed("open_app", "spotify"));

        memory.allow("open_app", "spotify").unwrap();
        assert!(memory.is_allowed("open_app", "spotify"));
    }

    #[test]
    fn forgetting_approvals_empties_the_list() {
        let (_dir, memory) = memory();
        memory.allow("open_app", "spotify").unwrap();
        memory.forget(Forget::Approvals);
        assert!(memory.approvals().is_empty());
        assert!(!memory.is_allowed("open_app", "spotify"));
        // A second forget is a no-op, not an error.
        memory.forget(Forget::Approvals);
    }

    #[test]
    fn a_missing_directory_is_an_empty_memory() {
        let memory = Memory::new(std::env::temp_dir().join("jarvis-memory-that-does-not-exist"));
        assert!(memory.transcript().is_empty());
        assert!(memory.facts().is_empty());
        assert!(memory.approvals().is_empty());
    }
}
