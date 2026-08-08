//! The transcript → provider-message projection, the v3 reshape of v2's
//! `entries_to_messages`: one entry becomes one message, so nothing is
//! lost in either direction. The one addition is attribution — when more
//! than one principal has posted, each post is prefixed with a bracketed
//! speaker tag, because a model reading a shared transcript needs to know
//! who said what. Solo conversations are left exactly as typed.

use std::collections::BTreeSet;

use myco_instance::Principal;
use myco_models::{Content, Message};

use crate::{Body, Entry};

pub fn project(entries: &[Entry]) -> Vec<Message> {
    let posters: BTreeSet<&Principal> = entries
        .iter()
        .filter(|e| matches!(e.body, Body::Message { .. }))
        .map(|e| &e.author)
        .collect();
    let attribute = posters.len() > 1;

    entries
        .iter()
        .map(|e| match &e.body {
            Body::Message { text } => {
                let mut content = Vec::with_capacity(2);
                if attribute {
                    content.push(Content::Text {
                        text: format!("[{}]", e.author),
                    });
                }
                content.push(Content::Text { text: text.clone() });
                Message::UserMessage { content }
            }
            Body::Assistant {
                content,
                tool_uses,
                turn_end,
                ..
            } => Message::AssistantMessage {
                content: content.clone(),
                tool_uses: tool_uses.clone(),
                turn_end_reason: turn_end.clone(),
            },
            Body::ToolResults { results } => Message::ToolResults {
                tool_use_results: results.clone(),
            },
            // What a subscription saw reads to the model like the room
            // speaking: an unattributed user message, tagged with where it
            // came from.
            Body::Watched { instance, data } => Message::UserMessage {
                content: vec![Content::Text {
                    text: format!("[watched {instance}]\n{data}"),
                }],
            },
        })
        .collect()
}
