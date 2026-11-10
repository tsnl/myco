//! Browser projection of recorded history and live events. Internal runtime
//! content stays hidden; tool inputs and results remain individually inspectable.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use myco::generative_model::{Content, Message, ToolResourceRef, ToolResult, ToolUse};
use myco::session::{Session, Thread};
use serde::Serialize;

#[path = "processes.rs"]
mod processes;
pub(super) use processes::{refresh_processes, retain_processes};

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
        #[serde(skip)]
        call_id: uuid::Uuid,
        #[serde(rename = "blocking")]
        waiting: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        resource: Option<ToolResourceRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        background_id: Option<uuid::Uuid>,
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
    // A hidden boundary keeps continuation headings and times distinct.
    Boundary {
        time: String,
    },
}

impl Block {
    pub fn input(content: &[Content], time: Option<String>) -> Self {
        let event = content.iter().find_map(|part| match part {
            Content::System { kind, .. } if kind == "timer" => Some("Timer fired"),
            Content::System { kind, .. } if kind == "inbox_event" => Some("Event received"),
            _ => None,
        });
        let mut block = Self::message(
            if event.is_some() { "system" } else { "user" },
            content,
            time,
        );
        if let Some(label) = event
            && let Self::Message { text, .. } = &mut block
        {
            *text = format!("{label}\n\n{text}");
        }
        block
    }

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
            call_id: uuid::Uuid::nil(),
            waiting: true,
            resource: None,
            background_id: None,
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
            background_id,
            resource,
            waiting,
            tool,
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
            *waiting = false;
            if tool.name == "bash"
                && matches!(
                    tool.input.get("action").and_then(serde_json::Value::as_str),
                    None | Some("exec" | "start")
                )
            {
                *resource = result.resource.clone();
            }
            *background_id = None;
            if let Some(timer) = timer {
                timer.finish();
            }
        }
    }

    pub fn continue_process(&mut self) {
        if let Self::Tool {
            resource: Some(_),
            running,
            status,
            error: false,
            timer,
            ..
        } = self
            && !status.starts_with("exit ")
            && !status.starts_with("signal ")
        {
            *running = true;
            *status = "running".into();
            if let Some(timer) = timer {
                timer.finished = None;
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
            Content::Text { text: value } if !legacy_prelude_notice(value) => {
                text.push(value.as_str())
            }
            Content::Image { source } => images.push(source.clone()),
            _ => {}
        }
    }
    (text.join("\n"), images)
}

fn legacy_prelude_notice(text: &str) -> bool {
    let Some(body) = text.strip_prefix("\n\n[myco: Prelude changes]\n") else {
        return false;
    };
    body.starts_with("The prelude has changed since the snapshot in your context. Files under the ")
        || body
            .starts_with("This context may omit earlier prelude updates. Use prelude action=list ")
}

/// Tools are assistant output even when no text preceded them. Keep the same
/// heading in live projection and replay without adding a saved model message.
pub(super) fn assistant_heading(blocks: &[Block]) -> Option<Block> {
    for block in blocks.iter().rev() {
        match block {
            Block::AssistantHeading { .. } => return None,
            Block::Boundary { time } => {
                return Some(Block::AssistantHeading {
                    time: Some(time.clone()),
                });
            }
            Block::Message { role, .. } if role == "assistant" => return None,
            Block::Message { role, time, .. } if role == "user" || role == "system" => {
                return Some(Block::AssistantHeading { time: time.clone() });
            }
            _ => {}
        }
    }
    None
}

pub(super) fn history(thread: &Thread) -> Vec<Block> {
    history_until(thread, thread.messages.len())
}

fn history_until(thread: &Thread, end: usize) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut time = None;
    let mut pending = Vec::new();
    let mut boundary = compaction_boundary(thread);
    for (index, message) in thread.messages[..end].iter().enumerate() {
        if boundary == Some(index) {
            time = Some(timestamp(&thread.created_at));
            blocks.push(Block::Boundary {
                time: timestamp(&thread.created_at),
            });
            boundary = None;
        }
        match message {
            Message::UserMessage { content } if message.is_user_turn() => {
                time = thread.user_turn_timestamps.get(&index).map(timestamp);
                let block = Block::input(content, time.clone());
                if let Block::Message { text, images, .. } = &block
                    && (!text.is_empty() || !images.is_empty() || content.is_empty())
                {
                    blocks.push(block);
                }
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
    if boundary == Some(end) {
        blocks.push(Block::Boundary {
            time: timestamp(&thread.created_at),
        });
    }
    for block in &mut blocks {
        if let Block::Tool {
            status,
            running,
            waiting,
            ..
        } = block
            && *waiting
        {
            *status = "outcome not recorded".into();
            *running = false;
            *waiting = false;
        }
    }
    blocks
}

fn compaction(thread: &Thread) -> Option<&serde_json::Value> {
    let Message::UserMessage { content } = thread.messages.first()? else {
        return None;
    };
    content.iter().find_map(|part| match part {
        Content::System { kind, data, .. } if kind == "compaction" => Some(data),
        _ => None,
    })
}

// The successor begins with a summary and copied recent context. Its boundary
// belongs after that context, not before the user's retained messages.
fn compaction_boundary(thread: &Thread) -> Option<usize> {
    compaction(thread)?["tail_messages"]
        .as_u64()
        .and_then(|count| usize::try_from(count).ok()?.checked_add(1))
        .filter(|&index| index <= thread.messages.len())
}

//
// Threads replaced by a successor
//

/// A replaced thread's history that its successor does not already show.
#[derive(Serialize)]
pub(super) struct Earlier {
    /// When the successor took over.
    replaced_at: String,
    blocks: Vec<Block>,
}

/// Replaced threads with history of their own, oldest first.
pub(super) fn earlier_threads(session: &Session) -> Vec<String> {
    session
        .threads()
        .windows(2)
        .filter(|pair| !restarts(&pair[1], &pair[0]))
        .map(|pair| pair[0].id.clone())
        .collect()
}

pub(super) fn earlier(session: &Session, id: &str) -> Option<Earlier> {
    let threads = session.threads();
    let index = threads.iter().position(|thread| thread.id == id)?;
    let (thread, successor) = (&threads[index], threads.get(index + 1)?);
    Some(Earlier {
        replaced_at: timestamp(&successor.created_at),
        blocks: history_until(thread, carried_from(thread, successor)),
    })
}

// Removing a rejected submission restarts from a copy of everything before
// it, under a new thread stamp. That successor repeats this thread in order.
fn restarts(successor: &Thread, thread: &Thread) -> bool {
    fn unstamped(thread: &Thread) -> Option<Vec<&Content>> {
        let Message::UserMessage { content } = thread.messages.first()? else {
            return None;
        };
        Some(
            content
                .iter()
                .filter(|part| !matches!(part, Content::System { kind, .. } if kind == "session"))
                .collect(),
        )
    }
    let first = unstamped(successor);
    first.is_some() && first == unstamped(thread)
}

// Compaction copies whole recent turns, so the successor's retained context
// begins at one of this thread's user turns. When the copy cannot be located,
// keep the whole thread: a repeated turn is better than a missing one.
fn carried_from(thread: &Thread, successor: &Thread) -> usize {
    if restarts(successor, thread) {
        return 0;
    }
    let all = thread.messages.len();
    let summarizes =
        compaction(successor).is_some_and(|data| data["predecessor_id"] == thread.id.as_str());
    let Some(end) = compaction_boundary(successor).filter(|_| summarizes) else {
        return all;
    };
    let carried = successor.messages[1..end]
        .iter()
        .filter(|message| message.is_user_turn())
        .count();
    let Some(before) = carried.checked_sub(1) else {
        return all;
    };
    thread
        .messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| message.is_user_turn())
        .nth(before)
        .map_or(all, |(index, _)| index)
}
