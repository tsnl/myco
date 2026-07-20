//! Root-only tool: ask the interactive user a question mid-turn.
//!
//! Installed on the in-process local worker only (never remotes — a remote host
//! has no terminal). Backed by a [`Prompt`]; the terminal implementation blocks
//! on stdin, so dispatch offloads the read to a blocking thread.

use std::sync::Arc;

use crate::core::Async;
use crate::generative_model::{self, ToolResult};
use crate::interaction::{Prompt, PromptError, Question};

use super::{HostDispatchContext, ToolService};

const TOOL_DESCRIPTION: &str = r#"
Ask the human operator a question and wait for their typed answer.

Use this only when you are genuinely blocked on a decision that is the user's to make and
cannot be resolved from the request, the code, or a sensible default — an ambiguous
requirement, a destructive-or-irreversible choice, or missing information only they have.
Do not use it to narrate progress, ask permission for routine work, or offload decisions you
can reasonably make yourself; prefer acting on a sensible default and saying what you chose.

Provide a clear `question`. Optionally list `options` as suggested answers — the user can
reply with an option's number or type something else. Returns the user's answer as text.

Only works in an interactive terminal session; in a piped/headless run it returns an error,
so treat a failure as "no answer available" and fall back to your best judgment.
"#;

/// Local tool that asks the interactive user a question via a [`Prompt`].
pub struct AskUserTool {
    prompt: Arc<dyn Prompt>,
}

impl AskUserTool {
    pub fn new(prompt: Arc<dyn Prompt>) -> Self {
        Self { prompt }
    }
}

impl ToolService for AskUserTool {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "ask_user".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            input_schema: schemars::schema_for!(Input).to_value(),
        }]
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: generative_model::ToolUse,
        ctx: HostDispatchContext,
    ) -> Async<generative_model::ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool_use.input.clone()) {
                Ok(v) => v,
                Err(e) => return ToolResult::err(format!("invalid ask_user input: {e}")),
            };
            if input.question.trim().is_empty() {
                return ToolResult::err("ask_user requires a non-empty question");
            }
            // Don't prompt for input the turn is already abandoning.
            if ctx.cancel.is_cancelled() {
                return ToolResult::err("turn cancelled before the user could be asked");
            }

            let question = Question {
                prompt: input.question,
                options: input.options,
                default: None,
            };
            let prompt = self.prompt.clone();
            // The terminal read blocks; keep it off the async runtime threads.
            let answered = tokio::task::spawn_blocking(move || prompt.ask(&question)).await;
            match answered {
                Ok(Ok(answer)) => ToolResult::text(format!("The user answered:\n{answer}")),
                Ok(Err(PromptError::NotInteractive)) => ToolResult::err(
                    "cannot ask the user: this myco session is not attached to an interactive \
                     terminal",
                ),
                Ok(Err(PromptError::Cancelled)) => {
                    ToolResult::err("the user dismissed the question without answering")
                }
                Ok(Err(PromptError::Io(e))) => {
                    ToolResult::err(format!("failed to read the user's answer: {e}"))
                }
                Err(join) => ToolResult::err(format!("ask_user task failed: {join}")),
            }
        })
    }
}

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
struct Input {
    /// The question to put to the user.
    question: String,
    /// Optional suggested answers, shown as a numbered menu.
    #[serde(default)]
    options: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;
    use crate::generative_model::{Content, ToolUse};
    use crate::interaction::ScriptedPrompt;
    use serde_json::json;

    fn tool_text(r: &generative_model::ToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    async fn ask(tool: Arc<AskUserTool>, input: serde_json::Value) -> generative_model::ToolResult {
        tool.dispatch_tool_use(
            ToolUse {
                id: "t1".into(),
                name: "ask_user".into(),
                input,
            },
            HostDispatchContext::bare(uuid::Uuid::nil(), CancelToken::new()),
        )
        .await
    }

    #[tokio::test]
    async fn returns_free_text_answer() {
        let prompt = Arc::new(ScriptedPrompt::new(["use postgres"]));
        let tool = Arc::new(AskUserTool::new(prompt));
        let res = ask(tool, json!({"question": "Which database?"})).await;
        assert!(!res.is_error, "{res:?}");
        assert!(tool_text(&res).contains("use postgres"), "{res:?}");
    }

    #[tokio::test]
    async fn numbered_option_is_selected() {
        let prompt = Arc::new(ScriptedPrompt::new(["2"]));
        let tool = Arc::new(AskUserTool::new(prompt));
        let res = ask(
            tool,
            json!({"question": "Pick", "options": ["sqlite", "postgres"]}),
        )
        .await;
        assert!(!res.is_error, "{res:?}");
        assert!(tool_text(&res).contains("postgres"), "{res:?}");
    }

    #[tokio::test]
    async fn empty_question_is_rejected() {
        let prompt = Arc::new(ScriptedPrompt::new(["ignored"]));
        let tool = Arc::new(AskUserTool::new(prompt));
        let res = ask(tool, json!({"question": "   "})).await;
        assert!(res.is_error, "{res:?}");
    }

    #[tokio::test]
    async fn user_dismissal_is_an_error_result() {
        // Empty script → the prompt reports Cancelled.
        let prompt = Arc::new(ScriptedPrompt::new(Vec::<String>::new()));
        let tool = Arc::new(AskUserTool::new(prompt));
        let res = ask(tool, json!({"question": "Anything?"})).await;
        assert!(res.is_error, "{res:?}");
        assert!(tool_text(&res).contains("dismissed"), "{res:?}");
    }

    #[tokio::test]
    async fn cancelled_turn_skips_the_prompt() {
        let prompt = Arc::new(ScriptedPrompt::new(["never read"]));
        let tool = Arc::new(AskUserTool::new(prompt.clone()));
        let cancel = CancelToken::new();
        cancel.cancel();
        let res = tool
            .dispatch_tool_use(
                ToolUse {
                    id: "t1".into(),
                    name: "ask_user".into(),
                    input: json!({"question": "Which database?"}),
                },
                HostDispatchContext::bare(uuid::Uuid::nil(), cancel),
            )
            .await;
        assert!(res.is_error, "{res:?}");
        assert!(
            prompt.asked().is_empty(),
            "prompt should not have been read"
        );
    }
}
