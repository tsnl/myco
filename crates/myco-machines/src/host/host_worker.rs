//! Host worker: serve NDJSON requests over arbitrary async Read/Write.
//!
//! Owns the tool service registry. The read loop never waits on tool work:
//! each request is handled on a background task that writes its response
//! through a shared writer lock.

use myco_models as generative_model;
use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::host::protocol::{Request, Response};
use crate::tool_services::{
    BashService, HostDispatchContext, TextEditorService, ToolService, ViewImageService,
};
use myco_api::ToolUse;
use myco_core::CancelToken;
use myco_models;

/// Worker process: tool registry + NDJSON serve loop.
#[derive(Clone)]
pub struct HostWorker {
    name: String,
    services: Vec<Arc<dyn ToolService>>,
    tool_to_service: HashMap<String, Arc<dyn ToolService>>,
    /// The bash service behind the `Shell*` observer requests. `None` on a
    /// bespoke worker built without bash — those answer observer requests
    /// with an error.
    bash: Option<Arc<BashService>>,
}

impl HostWorker {
    /// Build a worker with an explicit service list.
    pub fn new(name: impl Into<String>, services: Vec<Arc<dyn ToolService>>) -> Self {
        let tool_to_service = build_tool_to_service_map(&services);
        Self {
            name: name.into(),
            services,
            tool_to_service,
            bash: None,
        }
    }

    /// Register `bash` as this worker's shell-observer backend (the same
    /// instance that serves the `bash` tool, or the observer would watch a
    /// different set of sessions than the agent drives).
    pub fn with_bash(mut self, bash: Arc<BashService>) -> Self {
        self.bash = Some(bash);
        self
    }

    /// The shell-observer backend, when this worker has one.
    pub fn bash(&self) -> Option<&Arc<BashService>> {
        self.bash.as_ref()
    }

    /// Standard host catalog (same on every host / remote binary).
    ///
    /// `max_image_base64_bytes` is the driving model's image cap: it bounds what
    /// `view_image` will read and is quoted in the tool's description, so a
    /// remote worker must be spawned with the same value the agent side
    /// resolved (`--max-image-base64-bytes`).
    pub fn standard(name: impl Into<String>, max_image_base64_bytes: u64) -> Self {
        let bash = Arc::new(BashService::new());
        Self::new(
            name,
            Self::standard_services_with(bash.clone(), max_image_base64_bytes),
        )
        .with_bash(bash)
    }

    /// Standard service list for building an extended local worker: the
    /// dispatchers behind [`Self::standard_tool_specs`].
    pub fn standard_services(max_image_base64_bytes: u64) -> Vec<Arc<dyn ToolService>> {
        Self::standard_services_with(Arc::new(BashService::new()), max_image_base64_bytes)
    }

    /// [`Self::standard_services`] around a caller-held [`BashService`] — the
    /// harness keeps the local instance so the shell observer surface
    /// (scrollback, keyboard lock) is reachable outside tool dispatch.
    pub fn standard_services_with(
        bash: Arc<BashService>,
        max_image_base64_bytes: u64,
    ) -> Vec<Arc<dyn ToolService>> {
        vec![
            bash as Arc<dyn ToolService>,
            Arc::new(TextEditorService::new()) as Arc<dyn ToolService>,
            Arc::new(ViewImageService::new(max_image_base64_bytes)) as Arc<dyn ToolService>,
        ]
    }

    /// Tool catalog advertised by [`Self::standard`] — pure data, no services
    /// constructed.
    ///
    /// Used by the harness for routing and by lazy
    /// [`crate::host::HostController`]s to advertise tools before any
    /// connection exists. Concatenates the same per-service `specs()` the
    /// live services serve; a test pins this against a real worker so the
    /// two can never drift.
    pub fn standard_tool_specs(max_image_base64_bytes: u64) -> Vec<generative_model::ToolSpec> {
        [
            BashService::specs(),
            TextEditorService::specs(),
            ViewImageService::specs(max_image_base64_bytes),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        self.services
            .iter()
            .flat_map(|service| service.tool_specs())
            .collect()
    }

    /// In-process tool dispatch (unit tests; no NDJSON).
    pub async fn dispatch_tool_use(
        self: &Arc<Self>,
        tool_use: ToolUse,
        ctx: HostDispatchContext,
    ) -> myco_api::ToolResult {
        let id = tool_use.id.clone();
        let Some(service) = self.tool_to_service.get(&tool_use.name).cloned() else {
            return myco_api::ToolResult::err(format!("unknown tool '{}'", tool_use.name))
                .with_id(id);
        };
        service.dispatch_tool_use(tool_use, ctx).await.with_id(id)
    }

    pub fn notify_agent_finished(&self, agent_id: uuid::Uuid) {
        for service in &self.services {
            service.on_agent_finished(agent_id);
        }
    }

    /// One-line summaries of tool work still running for `agent_id` across
    /// all services (e.g. live bash sessions).
    pub fn running_tool_summaries(&self, agent_id: uuid::Uuid) -> Vec<String> {
        self.services
            .iter()
            .flat_map(|service| service.running_tool_summaries(agent_id))
            .collect()
    }

    /// Handle one decoded request and write the reply through `writer`.
    async fn handle_request<W>(self: Arc<Self>, writer: Arc<Mutex<W>>, msg: Request)
    where
        W: AsyncWriteExt + Unpin,
    {
        match msg {
            Request::Hello => {
                let response = Response::HelloOk {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                };
                if let Err(e) = write_locked(&writer, &response).await {
                    eprintln!("host worker: write hello failed: {e}");
                }
            }
            Request::ToolCall {
                id,
                agent_id,
                tool_use,
            } => {
                let result = self
                    .dispatch_tool_use(
                        tool_use,
                        HostDispatchContext {
                            agent_id,
                            cancel: CancelToken::new(),
                        },
                    )
                    .await;
                let response = Response::ToolResult { id, result };
                if let Err(e) = write_locked(&writer, &response).await {
                    eprintln!("host worker: write tool result failed: {e}");
                }
            }
            Request::AgentFinished { agent_id } => {
                // Fire-and-forget: no reply message exists for this request.
                self.notify_agent_finished(agent_id);
            }
            Request::ShellList { id } => {
                let reply = match self.bash() {
                    Some(b) => Response::Shells {
                        id,
                        shells: b.shell_overviews(),
                    },
                    None => no_bash(id),
                };
                let _ = write_locked(&writer, &reply).await;
            }
            Request::ShellTail {
                id,
                shell,
                from,
                max_bytes,
            } => {
                let reply = match self.bash() {
                    Some(b) => match b.shell_tail(&shell, from, max_bytes as usize) {
                        Ok(t) => Response::ShellTail {
                            id,
                            from: t.from,
                            end: t.end,
                            data: String::from_utf8_lossy(&t.data).into_owned(),
                            running: t.running,
                            lock: t.lock,
                        },
                        Err(e) => shell_err(id, e),
                    },
                    None => no_bash(id),
                };
                let _ = write_locked(&writer, &reply).await;
            }
            Request::ShellInput { id, shell, data } => {
                let reply = match self.bash() {
                    Some(b) => match b.shell_user_write(&shell, &data).await {
                        Ok(()) => match b.shell_overview(&shell) {
                            Ok(overview) => Response::Shell {
                                id,
                                shell: overview,
                                previous_lock: None,
                            },
                            Err(e) => shell_err(id, e),
                        },
                        Err(e) => shell_err(id, e),
                    },
                    None => no_bash(id),
                };
                let _ = write_locked(&writer, &reply).await;
            }
            Request::ShellLock { id, shell, lock } => {
                let reply = match self.bash() {
                    Some(b) => match b.shell_set_lock(&shell, lock) {
                        Ok(previous) => match b.shell_overview(&shell) {
                            Ok(overview) => Response::Shell {
                                id,
                                shell: overview,
                                previous_lock: Some(previous),
                            },
                            Err(e) => shell_err(id, e),
                        },
                        Err(e) => shell_err(id, e),
                    },
                    None => no_bash(id),
                };
                let _ = write_locked(&writer, &reply).await;
            }
            Request::ShellScreenshot { id, shell } => {
                let reply = match self.bash() {
                    Some(b) => match b.shell_screen(&shell) {
                        Ok(screen) => Response::ShellScreen { id, screen },
                        Err(e) => shell_err(id, e),
                    },
                    None => no_bash(id),
                };
                let _ = write_locked(&writer, &reply).await;
            }
        }
    }

    /// Serve until `reader` hits EOF.
    ///
    /// The read loop only waits on the next request line. Each request is
    /// handled on a spawned task; hello, tool-result, and error replies write
    /// through `writer` under a mutex. On EOF, in-flight tasks are joined so
    /// results are flushed before return.
    pub async fn serve<R, W>(self: &Arc<Self>, reader: R, writer: W) -> Result<(), String>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: AsyncWriteExt + Unpin + Send + 'static,
    {
        let mut lines = BufReader::new(reader);
        let writer = Arc::new(Mutex::new(writer));
        let mut in_flight: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

        loop {
            let line = match read_line(&mut lines).await {
                Ok(l) => l,
                Err(e) if e == "peer closed" => break,
                Err(e) => return Err(e),
            };

            let msg = match Request::decode(&line) {
                Ok(m) => m,
                Err(e) => {
                    let writer = Arc::clone(&writer);
                    in_flight.spawn(async move {
                        let err = Response::Error {
                            id: None,
                            message: format!("invalid request: {e}"),
                        };
                        let _ = write_locked(&writer, &err).await;
                    });
                    continue;
                }
            };

            let worker = Arc::clone(self);
            let writer = Arc::clone(&writer);
            in_flight.spawn(async move {
                worker.handle_request(writer, msg).await;
            });
        }

        while in_flight.join_next().await.is_some() {}
        Ok(())
    }

    /// Serve process stdin/stdout (`myco --mode host`).
    pub async fn serve_stdio(self) -> Result<(), String> {
        Arc::new(self)
            .serve(tokio::io::stdin(), tokio::io::stdout())
            .await
    }
}

/// Error reply for a shell-observer request that failed on this worker.
fn shell_err(id: String, message: String) -> Response {
    Response::Error {
        id: Some(id),
        message,
    }
}

/// Error reply for a worker built without a bash service.
fn no_bash(id: String) -> Response {
    shell_err(id, "this worker has no bash service (no shells)".into())
}

fn build_tool_to_service_map(
    services: &[Arc<dyn ToolService>],
) -> HashMap<String, Arc<dyn ToolService>> {
    let mut map = HashMap::new();
    for service in services {
        for tool_spec in service.tool_specs() {
            let old = map.insert(tool_spec.name.clone(), service.clone());
            if old.is_some() {
                panic!("Duplicate tool name: {}", tool_spec.name);
            }
        }
    }
    map
}

async fn read_line(r: &mut (impl AsyncBufReadExt + Unpin)) -> Result<String, String> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = r
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("peer closed".into());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return Ok(trimmed.to_string());
    }
}

/// Lock the shared writer and send one response (concurrent tasks serialize here).
async fn write_locked<W>(writer: &Arc<Mutex<W>>, msg: &Response) -> Result<(), String>
where
    W: AsyncWriteExt + Unpin,
{
    let mut guard = writer.lock().await;
    msg.write_to(&mut *guard).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The static catalog and a live standard worker must advertise the same
    /// tools: [`HostWorker::standard_tool_specs`] is what routing trusts
    /// before any worker exists, so it must never drift from what workers
    /// actually serve.
    ///
    /// Checked at a non-default cap, which also pins that the configured
    /// limit reaches both paths: routing advertises what the worker serves,
    /// including the size `view_image` tells the model about.
    #[test]
    fn standard_catalog_matches_standard_worker() {
        let cap = 9 * 1024 * 1024;
        let catalog = serde_json::to_value(HostWorker::standard_tool_specs(cap)).expect("catalog");
        let advertised =
            serde_json::to_value(HostWorker::standard("x", cap).tool_specs()).expect("advertised");
        assert_eq!(catalog, advertised);
        assert!(
            serde_json::to_string(&catalog).unwrap().contains("9.0 MiB"),
            "configured cap should reach the advertised view_image description"
        );
    }
}
