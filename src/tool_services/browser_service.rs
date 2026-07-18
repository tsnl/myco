//! Host tool: `lynx_tui_browser` — text-mode web browsing / search via the `lynx` CLI.
//!
//! Uses `lynx -dump` so agents get readable plaintext with numbered link IDs
//! (and a trailing References list unless `list_links=false` → `-nolist`).
//! Requires `lynx` on the **host** PATH (`brew install lynx` / `apt install lynx`).

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use url::Url;

use crate::core::{Async, CancelToken};
use crate::external_command::LYNX;
use crate::generative_model::{self, ToolResult};

use super::{HostDispatchContext, ToolService, kill_process_group};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;
const DEFAULT_WIDTH: u32 = 100;
const MAX_WIDTH: u32 = 400;
const DEFAULT_MAX_BYTES: usize = 200_000;
const HARD_MAX_BYTES: usize = 1_000_000;
fn tool_description() -> String {
    format!(
        r#"
Text-mode browser (`lynx_tui_browser`) for simple web browsing and web search via the lynx TUI engine. Dumps a public
HTTP(S) URL to plaintext via `lynx -dump` (requires `lynx` on the host PATH:
`brew install lynx` / `apt install lynx`).

Use cases:
- Web search: open a search-results URL and read organic hits as text, e.g.
  `https://lite.duckduckgo.com/lite/?q=…`, `https://html.duckduckgo.com/html/?q=…`,
  or `https://www.bing.com/search?q=…`.
- Simple browsing: docs, READMEs, wiki/articles, issue pages — follow up by
  dumping a result URL from the numbered link list.

Link IDs: by default (list_links=true) lynx numbers links in the body as [1],
[2], … and appends a References list mapping each ID to a full URL. Use those
IDs to pick the next page to dump. Set list_links=false only if you want a
compact body without the References section (`-nolist`).

Not a JS engine — SPAs / heavy client-rendered pages may be incomplete. For raw
HTTP verbs / response metadata, use host `bash` with `curl`.

Parameters:
- url (required): http or https URL (page or search-results URL)
- list_links (optional, default true): emit lynx link IDs + References list;
  false → `-nolist` (no References appendix)
- width (optional, default {DEFAULT_WIDTH}, max {MAX_WIDTH}): dump column width (`-width=`)
- max_bytes (optional, default {DEFAULT_MAX_BYTES}, hard max {HARD_MAX_BYTES}): truncate stdout
- timeout_secs (optional, default {DEFAULT_TIMEOUT_SECS}, hard max {MAX_TIMEOUT_SECS})
- host (optional): routing host; default local
"#
    )
}

/// Runs `lynx -dump` on behalf of the agent. Host-placed (standard catalog).
#[derive(Default)]
pub struct BrowserService;

impl BrowserService {
    pub fn new() -> Self {
        Self
    }

    /// Tool schemas served by this service (static: no instance required).
    pub fn specs() -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "lynx_tui_browser".to_string(),
            description: tool_description(),
            input_schema: schemars::schema_for!(Input).to_value(),
        }]
    }
}

impl ToolService for BrowserService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        Self::specs()
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: generative_model::ToolUse,
        ctx: HostDispatchContext,
    ) -> Async<generative_model::ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool_use.input.clone()) {
                Ok(v) => v,
                Err(e) => {
                    return ToolResult::err(format!("invalid lynx_tui_browser input: {e}"));
                }
            };
            match self.dump(input, &ctx.cancel).await {
                Ok(text) => ToolResult::text(text),
                Err(e) => ToolResult::err(e),
            }
        })
    }
}

impl BrowserService {
    async fn dump(&self, input: Input, cancel: &CancelToken) -> Result<String, String> {
        let url = parse_http_url(&input.url)?;
        let list_links = input.list_links.unwrap_or(true);
        let width = input.width.unwrap_or(DEFAULT_WIDTH).clamp(20, MAX_WIDTH);
        let max_bytes = input
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .clamp(1, HARD_MAX_BYTES);
        let timeout_secs = input
            .timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        let lynx = LYNX.resolve().ok_or_else(|| {
            "lynx not found on PATH. Install: `brew install lynx` (macOS) or \
             `apt install lynx` / `dnf install lynx` (Linux)."
                .to_string()
        })?;

        let mut cmd = Command::new(&lynx);
        cmd.arg("-dump")
            .arg(format!("-width={width}"))
            .arg("-force_html")
            .arg("-accept_all_cookies");
        if !list_links {
            cmd.arg("-nolist");
        }
        cmd.arg(url.as_str());
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "lynx not found on PATH. Install: `brew install lynx` or `apt install lynx`."
                    .to_string()
            } else {
                format!("failed to spawn lynx ({}): {e}", lynx.display())
            }
        })?;

        let child_pid = child.id();
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");
        let stdout_buf = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let stdout_task = {
            let stdout_buf = Arc::clone(&stdout_buf);
            tokio::spawn(async move {
                let mut local = Vec::new();
                let _ = stdout.read_to_end(&mut local).await;
                *stdout_buf.lock().await = local;
            })
        };
        let stderr_task = {
            let stderr_buf = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                let mut local = Vec::new();
                let _ = stderr.read_to_end(&mut local).await;
                *stderr_buf.lock().await = local;
            })
        };

        let deadline = Duration::from_secs(timeout_secs);
        enum Outcome {
            Cancelled,
            TimedOut,
            Status(std::io::Result<std::process::ExitStatus>),
        }
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => Outcome::Cancelled,
            _ = tokio::time::sleep(deadline) => Outcome::TimedOut,
            status = child.wait() => Outcome::Status(status),
        };

        match outcome {
            Outcome::Cancelled | Outcome::TimedOut => {
                kill_process_group(child_pid);
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                let partial = String::from_utf8_lossy(&stdout_buf.lock().await).into_owned();
                let why = if matches!(outcome, Outcome::Cancelled) {
                    "cancelled"
                } else {
                    "timed out"
                };
                if partial.trim().is_empty() {
                    return Err(format!(
                        "lynx_tui_browser {why} after {timeout_secs}s (no output)"
                    ));
                }
                let mut out = format!("status: {why}\nurl: {url}\npartial: true\n\n");
                out.push_str(&truncate_bytes(&partial, max_bytes));
                Ok(out)
            }
            Outcome::Status(Err(e)) => {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                Err(format!("lynx wait failed: {e}"))
            }
            Outcome::Status(Ok(status)) => {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                let stdout = String::from_utf8_lossy(&stdout_buf.lock().await).into_owned();
                let stderr = String::from_utf8_lossy(&stderr_buf.lock().await).into_owned();
                if !status.success() && stdout.trim().is_empty() {
                    let err = stderr.trim();
                    return Err(format!(
                        "lynx exited {}{}",
                        status
                            .code()
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "signal".into()),
                        if err.is_empty() {
                            String::new()
                        } else {
                            format!(": {err}")
                        }
                    ));
                }
                let mut out = format!("url: {url}\nlynx: {}\n", lynx.display());
                if !status.success() {
                    out.push_str(&format!(
                        "exit: {}\n",
                        status
                            .code()
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "signal".into())
                    ));
                }
                if !stderr.trim().is_empty() {
                    out.push_str(&format!("stderr: {}\n", stderr.trim().replace('\n', " | ")));
                }
                out.push('\n');
                out.push_str(&truncate_bytes(&stdout, max_bytes));
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                Ok(out)
            }
        }
    }
}

fn parse_http_url(raw: &str) -> Result<Url, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("url must be non-empty".into());
    }
    let url = Url::parse(trimmed).map_err(|e| format!("invalid url: {e}"))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err(format!(
            "only http/https URLs are allowed (got scheme {other:?})"
        )),
    }
}

fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    let b = text.as_bytes();
    if b.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_string();
    out.push_str(&format!(
        "\n\n[truncated: kept {end} of {} bytes]\n",
        b.len()
    ));
    out
}

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
struct Input {
    /// http or https URL to dump (page or search-results URL).
    url: String,
    /// Emit lynx link IDs (`[n]` in the body) and the trailing References list
    /// that maps each ID to a full URL (default true). Set false for `-nolist`
    /// (compact body without the References appendix).
    #[serde(default)]
    list_links: Option<bool>,
    /// Dump column width (default 100, max 400).
    #[serde(default)]
    width: Option<u32>,
    /// Max stdout bytes to return (default 200000).
    #[serde(default)]
    max_bytes: Option<usize>,
    /// Process timeout in seconds (default 30, hard max 120).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generative_model::{Content, ToolUse};
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

    fn ctx() -> HostDispatchContext {
        HostDispatchContext {
            agent_id: uuid::Uuid::nil(),
            cancel: CancelToken::new(),
            agent_root: None,
        }
    }

    #[test]
    fn rejects_non_http() {
        assert!(parse_http_url("file:///etc/passwd").is_err());
        assert!(parse_http_url("https://example.com").is_ok());
    }

    #[test]
    fn description_covers_search_browse_and_link_ids() {
        let specs = BrowserService::new().tool_specs();
        assert_eq!(specs.len(), 1);
        let d = &specs[0].description;
        assert!(d.contains("web search") || d.contains("Web search"), "{d}");
        assert!(
            d.contains("browsing") || d.contains("Simple browsing"),
            "{d}"
        );
        assert!(d.contains("list_links"), "{d}");
        assert!(d.contains("References"), "{d}");
        assert!(
            d.contains("lite.duckduckgo.com") || d.contains("bing.com"),
            "{d}"
        );
        // Default keeps link IDs (no -nolist unless list_links=false).
        assert!(
            d.contains("default true") || d.contains("list_links=true"),
            "{d}"
        );
        // Stated defaults/limits must be the ones actually enforced.
        for needle in [
            DEFAULT_WIDTH.to_string(),
            MAX_WIDTH.to_string(),
            DEFAULT_MAX_BYTES.to_string(),
            HARD_MAX_BYTES.to_string(),
            DEFAULT_TIMEOUT_SECS.to_string(),
            MAX_TIMEOUT_SECS.to_string(),
        ] {
            assert!(d.contains(&needle), "description missing {needle}: {d}");
        }
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "é".repeat(10);
        let t = truncate_bytes(&s, 3);
        assert!(t.contains("truncated"));
    }

    #[tokio::test]
    async fn rejects_file_scheme_before_spawn() {
        let svc = Arc::new(BrowserService::new());
        let res = svc
            .dispatch_tool_use(
                ToolUse {
                    id: "t".into(),
                    name: "lynx_tui_browser".into(),
                    input: json!({"url": "file:///tmp/x"}),
                },
                ctx(),
            )
            .await;
        assert!(res.is_error);
        assert!(
            tool_text(&res).contains("http/https"),
            "{}",
            tool_text(&res)
        );
    }

    #[tokio::test]
    #[ignore = "network + lynx"]
    async fn live_dump_example_com() {
        if LYNX.resolve().is_none() {
            eprintln!("skip: lynx not installed");
            return;
        }
        let svc = Arc::new(BrowserService::new());
        let res = svc
            .dispatch_tool_use(
                ToolUse {
                    id: "live".into(),
                    name: "lynx_tui_browser".into(),
                    input: json!({
                        "url": "https://example.com/",
                        "list_links": false,
                        "width": 80,
                    }),
                },
                ctx(),
            )
            .await;
        assert!(!res.is_error, "{}", tool_text(&res));
        let text = tool_text(&res);
        assert!(
            text.to_ascii_lowercase().contains("example domain"),
            "{text}"
        );
    }
}
