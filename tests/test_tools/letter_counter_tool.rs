#![allow(dead_code)]
use std::sync::Arc;

use myco::core::Async;
use myco::generative_model::{ToolResult, ToolSpec, ToolUse};
use myco::tool_services::{HostDispatchContext, ToolService};

/// Test tool: count occurrences of a letter in a word.
#[derive(Default)]
pub struct LetterCounterTool;

impl ToolService for LetterCounterTool {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "letter_counter".into(),
            description: "Count how many times a letter appears in a word.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "word": { "type": "string" },
                    "letter": { "type": "string" }
                },
                "required": ["word", "letter"]
            }),
            input_examples: vec![],
        }]
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: ToolUse,
        _ctx: HostDispatchContext,
    ) -> Async<ToolResult> {
        Box::pin(async move {
            let word = tool_use
                .input
                .get("word")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let letter = tool_use
                .input
                .get("letter")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ch = letter.chars().next();
            let count = match ch {
                Some(c) => word.chars().filter(|x| x.eq_ignore_ascii_case(&c)).count(),
                None => 0,
            };
            ToolResult::text(count.to_string())
        })
    }
}
