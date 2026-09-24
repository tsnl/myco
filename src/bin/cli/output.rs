//! Answer text goes to stdout; terminal activity and diagnostics go to stderr.

use std::io::Write;
use std::sync::Mutex;

use myco::generative_model::Content;
use myco::{AgentEvent, CancelToken, EventSink};

struct OutputState {
    pending: String,
    wrote: bool,
    newline: bool,
    error: Option<String>,
    cancel: CancelToken,
}

impl OutputState {
    fn new(cancel: CancelToken) -> Self {
        Self {
            pending: String::new(),
            wrote: false,
            newline: false,
            error: None,
            cancel,
        }
    }
}

pub(super) struct CliSink {
    state: Mutex<OutputState>,
    terminal: bool,
}

impl CliSink {
    pub fn new(terminal: bool) -> Self {
        Self {
            state: Mutex::new(OutputState::new(CancelToken::new())),
            terminal,
        }
    }

    pub fn begin(&self, cancel: CancelToken) {
        *self.state.lock().unwrap() = OutputState::new(cancel);
    }

    pub fn finish(&self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        let mut stdout = std::io::stdout().lock();
        let result = (|| {
            if state.wrote && !state.newline {
                stdout.write_all(b"\n")?;
                state.newline = true;
            }
            stdout.flush()
        })();
        if let Err(error) = result {
            state.error = Some(format!("write answer: {error}"));
            state.cancel.cancel();
        }
        state.error.clone().map_or(Ok(()), Err)
    }

    fn text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if state.error.is_some() {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        if let Err(error) = stdout
            .write_all(text.as_bytes())
            .and_then(|()| stdout.flush())
        {
            state.error = Some(format!("write answer: {error}"));
            state.cancel.cancel();
        } else {
            state.wrote = true;
            state.newline = text.ends_with('\n');
        }
    }
}

impl EventSink for CliSink {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::GenerationStarted { context } | AgentEvent::TurnFinished { context }
                if context.depth == 0 =>
            {
                self.state.lock().unwrap().pending.clear();
            }
            AgentEvent::GenerationFinished { context } if context.depth == 0 => {
                let text = std::mem::take(&mut self.state.lock().unwrap().pending);
                self.text(&text);
            }
            AgentEvent::TextDelta { text, context } if context.depth == 0 => {
                if self.terminal {
                    self.text(&text);
                } else {
                    self.state.lock().unwrap().pending.push_str(&text);
                }
            }
            AgentEvent::ToolStarted {
                tool_use, context, ..
            } if self.terminal && context.depth == 0 => {
                let _ = self.finish();
                eprintln!("\n── {} · running", tool_use.name);
                if let Some(fields) = tool_use.input.as_object() {
                    for (key, value) in fields {
                        let text = value
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| value.to_string());
                        eprintln!("{key}\n{}", preview(&text));
                    }
                }
            }
            AgentEvent::ToolFinished {
                tool_use,
                result,
                context,
                ..
            } if self.terminal && context.depth == 0 => {
                let status = result.status.as_deref().unwrap_or(if result.is_error {
                    "failed"
                } else {
                    "done"
                });
                eprintln!("── {} · {status}", tool_use.name);
                for part in result.content {
                    if let Content::Text { text } = part {
                        eprintln!("{}", preview(&text));
                    }
                }
                eprintln!();
            }
            AgentEvent::Failure {
                failure,
                retry_in,
                attempt,
                max_attempts,
                context,
                ..
            } if context.depth == 0 => {
                self.state.lock().unwrap().pending.clear();
                if let Some(delay) = retry_in {
                    let _ = self.finish();
                    eprintln!(
                        "myco: response interrupted; retrying {}/{} in {:.1}s: {}",
                        attempt + 1,
                        max_attempts,
                        delay.as_secs_f64(),
                        failure.cause
                    );
                }
            }
            _ => {}
        }
    }
}

fn preview(text: &str) -> String {
    let mut lines = text.lines();
    let prefix = lines.by_ref().take(12).collect::<Vec<_>>().join("\n");
    let mut chars = prefix.chars();
    let mut shown: String = chars.by_ref().take(2000).collect();
    if chars.next().is_some() || lines.next().is_some() {
        shown.push_str("\n… (full output in session history)");
    }
    shown
}
