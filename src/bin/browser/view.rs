//! Browser projection of recorded history and live events. Internal runtime
//! content stays hidden; tool inputs and results remain individually inspectable.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use myco::generative_model::{Content, Message, ToolResult, ToolUse};
use myco::session::Thread;
use serde::Serialize;

pub(super) fn timestamp(time: &DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Observed by this server process; historical calls have no measured duration.
#[derive(Clone)]
pub(super) struct ToolTimer {
    started: Instant,
    finished: Option<Duration>,
}

impl ToolTimer {
    fn finish(&mut self) {
        self.finished.get_or_insert_with(|| self.started.elapsed());
    }
}

impl Serialize for ToolTimer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let elapsed = self.finished.unwrap_or_else(|| self.started.elapsed());
        serializer.serialize_u64(elapsed.as_millis().try_into().unwrap_or(u64::MAX))
    }
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Block {
    AssistantHeading {
        time: Option<String>,
    },
    Message {
        role: String,
        text: String,
        images: Vec<String>,
        time: Option<String>,
    },
    Tool {
        tool: ToolUse,
        text: String,
        images: Vec<String>,
        status: String,
        error: bool,
        running: bool,
        #[serde(rename = "elapsed_ms", skip_serializing_if = "Option::is_none")]
        timer: Option<ToolTimer>,
    },
    Notice {
        text: String,
    },
}

impl Block {
    pub fn message(role: &str, content: &[Content], time: Option<String>) -> Self {
        let (text, images) = visible(content);
        Self::Message {
            role: role.into(),
            text,
            images,
            time,
        }
    }

    pub fn tool(tool: ToolUse, started: Option<Instant>) -> Self {
        Self::Tool {
            tool,
            text: String::new(),
            images: vec![],
            status: "running".into(),
            error: false,
            running: true,
            timer: started.map(|started| ToolTimer {
                started,
                finished: None,
            }),
        }
    }

    pub fn finish(&mut self, result: &ToolResult) {
        if let Self::Tool {
            text,
            images,
            status,
            error,
            running,
            timer,
            ..
        } = self
        {
            (*text, *images) = visible(&result.content);
            *status = result
                .status
                .clone()
                .unwrap_or_else(|| if result.is_error { "failed" } else { "done" }.into());
            *error = result.is_error
                || status.starts_with("signal ")
                || status.starts_with("timed out")
                || status.starts_with("cancel requested")
                || status
                    .strip_prefix("exit ")
                    .and_then(|n| n.parse::<i32>().ok())
                    .is_some_and(|n| n != 0);
            *running = false;
            if let Some(timer) = timer {
                timer.finish();
            }
        }
    }
}

/// Keep observations when rebuilding the same thread, matching repeated calls
/// by occurrence. Calls loaded from disk must not acquire invented durations.
pub(super) fn retain_tool_timers(blocks: &mut [Block], previous: &[Block]) {
    let mut timers: HashMap<_, VecDeque<_>> = HashMap::new();
    for block in previous {
        if let Block::Tool { tool, timer, .. } = block {
            timers
                .entry((&tool.name, tool.input.to_string()))
                .or_default()
                .push_back(timer.clone().filter(|timer| timer.finished.is_some()));
        }
    }
    for block in blocks {
        if let Block::Tool { tool, timer, .. } = block {
            *timer = timers
                .get_mut(&(&tool.name, tool.input.to_string()))
                .and_then(VecDeque::pop_front)
                .flatten();
        }
    }
}

fn visible(content: &[Content]) -> (String, Vec<String>) {
    let mut text = Vec::new();
    let mut images = Vec::new();
    for part in content {
        match part {
            Content::Text { text: value } => text.push(value.as_str()),
            Content::Image { source } => images.push(source.clone()),
            _ => {}
        }
    }
    (text.join("\n"), images)
}

/// Tools are assistant output even when no text preceded them. Keep the same
/// heading in live projection and replay without adding a saved model message.
pub(super) fn assistant_heading(blocks: &[Block]) -> Option<Block> {
    for block in blocks.iter().rev() {
        match block {
            Block::AssistantHeading { .. } => return None,
            Block::Message { role, .. } if role == "assistant" => return None,
            Block::Message { role, time, .. } if role == "user" => {
                return Some(Block::AssistantHeading { time: time.clone() });
            }
            _ => {}
        }
    }
    None
}

pub(super) fn history(thread: &Thread) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut time = None;
    let mut pending = Vec::new();
    for (index, message) in thread.messages.iter().enumerate() {
        match message {
            Message::UserMessage { content } if message.is_user_turn() => {
                time = thread.user_turn_timestamps.get(&index).map(timestamp);
                blocks.push(Block::message("user", content, time.clone()));
            }
            Message::AssistantMessage {
                content, tool_uses, ..
            } => {
                // Providers can split Markdown syntax across adjacent text parts.
                // Replay must concatenate them exactly as the live stream does.
                let mut text_block: Option<usize> = None;
                for part in content {
                    if !matches!(part, Content::Text { .. }) {
                        text_block = None;
                    }
                    match part {
                        Content::Text { text } if !text.is_empty() => {
                            if let Some(Block::Message { text: previous, .. }) =
                                text_block.and_then(|index| blocks.get_mut(index))
                            {
                                previous.push_str(text);
                            } else {
                                text_block = Some(blocks.len());
                                blocks.push(Block::message(
                                    "assistant",
                                    std::slice::from_ref(part),
                                    time.clone(),
                                ));
                            }
                        }
                        Content::Thinking { text, .. } if !text.is_empty() => {
                            blocks.push(Block::Message {
                                role: "thinking".into(),
                                text: text.clone(),
                                images: vec![],
                                time: time.clone(),
                            })
                        }
                        Content::Image { .. } => blocks.push(Block::message(
                            "assistant",
                            std::slice::from_ref(part),
                            time.clone(),
                        )),
                        _ => {}
                    }
                }
                if !tool_uses.is_empty()
                    && let Some(heading) = assistant_heading(&blocks)
                {
                    blocks.push(heading);
                }
                pending.clear();
                for tool in tool_uses {
                    pending.push(blocks.len());
                    blocks.push(Block::tool(tool.clone(), None));
                }
            }
            Message::ToolResults { tool_use_results } => {
                for (index, result) in pending.drain(..).zip(tool_use_results) {
                    blocks[index].finish(result);
                }
            }
            _ => {}
        }
    }
    for block in &mut blocks {
        if let Block::Tool {
            status, running, ..
        } = block
            && *running
        {
            *status = "outcome not recorded".into();
            *running = false;
        }
    }
    blocks
}
