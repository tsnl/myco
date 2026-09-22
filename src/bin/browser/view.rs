//! Browser projection of recorded history and live events. Internal runtime
//! content stays hidden; tool inputs and results remain individually inspectable.

use chrono::{DateTime, SecondsFormat, Utc};
use myco::generative_model::{Content, Message, ToolResult, ToolUse};
use myco::session::Thread;
use serde::Serialize;

pub(super) fn timestamp(time: &DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Block {
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

    pub fn tool(tool: ToolUse) -> Self {
        Self::Tool {
            tool,
            text: String::new(),
            images: vec![],
            status: "running".into(),
            error: false,
            running: true,
        }
    }

    pub fn finish(&mut self, result: &ToolResult) {
        if let Self::Tool {
            text,
            images,
            status,
            error,
            running,
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
                || status
                    .strip_prefix("exit ")
                    .and_then(|n| n.parse::<i32>().ok())
                    .is_some_and(|n| n != 0);
            *running = false;
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
                for part in content {
                    match part {
                        Content::Text { text } if !text.is_empty() => blocks.push(Block::message(
                            "assistant",
                            std::slice::from_ref(part),
                            time.clone(),
                        )),
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
                pending.clear();
                for tool in tool_uses {
                    pending.push(blocks.len());
                    blocks.push(Block::tool(tool.clone()));
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
