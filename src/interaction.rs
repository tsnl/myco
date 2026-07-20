//! Asking the human operator a question on the controlling terminal.
//!
//! One primitive, two callers: the `ask_user` tool
//! ([`crate::tool_services::AskUserTool`]) lets a running agent ask the user a
//! question mid-turn, and the first-run guided setup wizard
//! ([`crate::config::run_setup`]) walks a new user through building their model
//! catalog. Both go through [`Prompt::ask`].
//!
//! [`TerminalPrompt`] is the real implementation: it prints the question to
//! stdout and reads one line from stdin. It is TTY-gated (a piped / headless
//! session gets [`PromptError::NotInteractive`], never a silent hang) and its
//! reads are serialized so concurrent tool calls cannot interleave on the
//! terminal. Reads are blocking; callers on the async runtime offload with
//! `spawn_blocking`.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Question / answer model
// ---------------------------------------------------------------------------

/// A single question posed to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// The prompt text shown to the user.
    pub prompt: String,
    /// A numbered menu of suggested answers. Empty means free-form: the user
    /// types whatever they want. When non-empty the user may reply with a
    /// number to pick a listed value, or type something else entirely.
    pub options: Vec<String>,
    /// Answer substituted when the user submits an empty line. `None` returns
    /// the empty string for a blank reply.
    pub default: Option<String>,
}

impl Question {
    /// A free-form question with no suggested options.
    pub fn free(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            options: Vec::new(),
            default: None,
        }
    }

    /// A question with a numbered menu of suggested answers.
    pub fn choose(prompt: impl Into<String>, options: Vec<String>) -> Self {
        Self {
            prompt: prompt.into(),
            options,
            default: None,
        }
    }

    /// Set the answer used when the user just hits Enter.
    pub fn with_default(mut self, default: impl Into<String>) -> Self {
        self.default = Some(default.into());
        self
    }
}

/// Why a prompt did not produce an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptError {
    /// No controlling terminal (piped / headless session).
    NotInteractive,
    /// The user closed input (EOF / Ctrl-D) without answering.
    Cancelled,
    /// Reading or writing the terminal failed.
    Io(String),
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptError::NotInteractive => write!(f, "not attached to an interactive terminal"),
            PromptError::Cancelled => write!(f, "input closed without an answer"),
            PromptError::Io(e) => write!(f, "terminal I/O error: {e}"),
        }
    }
}

impl std::error::Error for PromptError {}

/// Something that can ask the user a [`Question`] and return their answer.
///
/// `Send + Sync + 'static` so a `dyn Prompt` can be shared across the harness
/// and moved onto a blocking thread for the (blocking) read.
pub trait Prompt: Send + Sync + 'static {
    fn ask(&self, question: &Question) -> Result<String, PromptError>;
}

// ---------------------------------------------------------------------------
// Rendering / answer resolution (pure — shared by every implementation)
// ---------------------------------------------------------------------------

/// The exact text shown before reading input: the prompt, a numbered menu of
/// any options, then the input caret (with the default in brackets).
pub fn render_question(question: &Question) -> String {
    let mut out = String::new();
    out.push_str(question.prompt.trim_end());
    out.push('\n');
    for (i, option) in question.options.iter().enumerate() {
        out.push_str(&format!("  {}) {}\n", i + 1, option));
    }
    match &question.default {
        Some(default) => out.push_str(&format!("[{default}] > ")),
        None => out.push_str("> "),
    }
    out
}

/// Map a raw input line to an answer: a blank line yields the default (or the
/// empty string); a number in range picks that option; anything else is
/// returned verbatim (trimmed) as a free-form answer.
pub fn resolve_answer(question: &Question, raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return question.default.clone().unwrap_or_default();
    }
    if !question.options.is_empty()
        && let Ok(n) = trimmed.parse::<usize>()
        && (1..=question.options.len()).contains(&n)
    {
        return question.options[n - 1].clone();
    }
    trimmed.to_string()
}

// ---------------------------------------------------------------------------
// Terminal implementation
// ---------------------------------------------------------------------------

/// Reads answers from the process's controlling terminal (stdin/stdout).
///
/// The internal mutex serializes prompts so two concurrent tool calls cannot
/// interleave their questions on one terminal.
#[derive(Default)]
pub struct TerminalPrompt {
    serialize: Mutex<()>,
}

impl TerminalPrompt {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Prompt for TerminalPrompt {
    fn ask(&self, question: &Question) -> Result<String, PromptError> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Err(PromptError::NotInteractive);
        }
        let _guard = self.serialize.lock().unwrap_or_else(|e| e.into_inner());

        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(render_question(question).as_bytes())
            .and_then(|_| stdout.flush())
            .map_err(|e| PromptError::Io(e.to_string()))?;

        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => Err(PromptError::Cancelled), // EOF / Ctrl-D
            Ok(_) => Ok(resolve_answer(question, &line)),
            Err(e) => Err(PromptError::Io(e.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Test double
// ---------------------------------------------------------------------------

/// Scripted [`Prompt`] for tests: answers a fixed queue of raw input lines
/// (resolved exactly as the terminal would), then reports `Cancelled`.
#[cfg(test)]
pub(crate) struct ScriptedPrompt {
    answers: Mutex<std::collections::VecDeque<String>>,
    asked: Mutex<Vec<String>>,
}

#[cfg(test)]
impl ScriptedPrompt {
    pub(crate) fn new<I, S>(answers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            answers: Mutex::new(answers.into_iter().map(Into::into).collect()),
            asked: Mutex::new(Vec::new()),
        }
    }

    /// Prompt texts asked so far, in order.
    pub(crate) fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl Prompt for ScriptedPrompt {
    fn ask(&self, question: &Question) -> Result<String, PromptError> {
        self.asked.lock().unwrap().push(question.prompt.clone());
        match self.answers.lock().unwrap().pop_front() {
            Some(raw) => Ok(resolve_answer(question, &raw)),
            None => Err(PromptError::Cancelled),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_numbers_options_and_shows_default() {
        let q =
            Question::choose("Pick one", vec!["alpha".into(), "beta".into()]).with_default("alpha");
        let text = render_question(&q);
        assert_eq!(text, "Pick one\n  1) alpha\n  2) beta\n[alpha] > ");
    }

    #[test]
    fn render_free_form_has_no_menu() {
        assert_eq!(
            render_question(&Question::free("Your name?")),
            "Your name?\n> "
        );
    }

    #[test]
    fn number_selects_option() {
        let q = Question::choose("Pick", vec!["alpha".into(), "beta".into()]);
        assert_eq!(resolve_answer(&q, "2\n"), "beta");
    }

    #[test]
    fn out_of_range_number_is_treated_as_free_text() {
        let q = Question::choose("Pick", vec!["alpha".into(), "beta".into()]);
        assert_eq!(resolve_answer(&q, "9"), "9");
    }

    #[test]
    fn blank_uses_default_or_empty() {
        let with = Question::free("x").with_default("fallback");
        assert_eq!(resolve_answer(&with, "\n"), "fallback");
        assert_eq!(resolve_answer(&Question::free("x"), "  \n"), "");
    }

    #[test]
    fn free_text_is_trimmed_and_verbatim() {
        let q = Question::free("Path?");
        assert_eq!(resolve_answer(&q, "  ~/.secrets/key \n"), "~/.secrets/key");
    }

    #[test]
    fn scripted_prompt_answers_in_order_then_cancels() {
        let prompt = ScriptedPrompt::new(["1", "hello"]);
        let menu = Question::choose("Pick", vec!["alpha".into(), "beta".into()]);
        assert_eq!(prompt.ask(&menu).unwrap(), "alpha");
        assert_eq!(prompt.ask(&Question::free("Say")).unwrap(), "hello");
        assert_eq!(
            prompt.ask(&Question::free("Again")),
            Err(PromptError::Cancelled)
        );
        assert_eq!(prompt.asked(), vec!["Pick", "Say", "Again"]);
    }
}
