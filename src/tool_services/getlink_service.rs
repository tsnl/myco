//! Server-only tool for the same workspace capability used by static file HTTP
//! routes and Markdown rewriting. No copies, uploads, or network requests.

use std::path::Path;
use std::sync::Arc;

use crate::core::{Async, WorkspaceFiles};
use crate::generative_model::{ToolResult, ToolSpec, ToolUse};

use super::{HostDispatchContext, ToolService};

pub struct GetLinkTool {
    files: WorkspaceFiles,
}

impl GetLinkTool {
    pub fn new(files: WorkspaceFiles) -> Self {
        Self { files }
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Input {
    /// Literal absolute or workspace-relative file path, without URL encoding.
    path: String,
}

impl ToolService for GetLinkTool {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "getlink".into(),
            description: "Get a browser URL for an existing file in this profile's server workspace. \
                Accepts an absolute or workspace-relative filesystem path, including spaces. \
                Returns a profile-scoped, browser-relative URL that works through SSH tunnels. \
                Use it verbatim in Markdown: [label](URL) for files or ![alt text](URL) for images. \
                This uses the same URL mapping as Markdown. Files outside the workspace and \
                files on other hosts are not served; copy them into the workspace first. \
                Links reflect the file's current contents while this Myco server is running.".into(),
            input_schema: super::tool_input_schema::<Input>(),
        }]
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool: ToolUse,
        _ctx: HostDispatchContext,
    ) -> Async<ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool.input) {
                Ok(input) => input,
                Err(error) => return ToolResult::err(format!("Invalid getlink input: {error}")),
            };
            let result =
                tokio::task::spawn_blocking(move || self.files.link(Path::new(&input.path))).await;
            match result {
                Ok(Ok(url)) => ToolResult::text(url),
                Ok(Err(error)) => ToolResult::err(error),
                Err(error) => ToolResult::err(format!("Cannot resolve workspace file: {error}")),
            }
        })
    }
}
