//! Host worker: serve NDJSON requests over arbitrary async Read/Write.
//!
//! Owns the tool service registry. The read loop never waits on tool work:
//! each request is handled on a background task that writes its response
//! through a shared writer lock.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::core::CancelToken;
use crate::generative_model::{self, ToolUse};
use crate::host::protocol::{Request, Response};
use crate::tool_services::{
    BashService, HostDispatchContext, ManualService, TextEditorService, ToolService,
};

/// Worker process: tool registry + NDJSON serve loop.
#[derive(Clone)]
pub struct HostWorker {
    name: String,
    services: Vec<Arc<dyn ToolService>>,
    tool_to_service: HashMap<String, Arc<dyn ToolService>>,
}

impl HostWorker {
    /// Build a worker with an explicit service list.
    pub fn new(name: impl Into<String>, services: Vec<Arc<dyn ToolService>>) -> Self {
        let tool_to_service = build_tool_to_service_map(&services);
        Self {
            name: name.into(),
            services,
            tool_to_service,
        }
    }

    /// Standard host catalog (same on every host / remote binary).
    pub fn standard(name: impl Into<String>) -> Self {
        Self::new(name, Self::standard_services())
    }

    /// Standard service list for building an extended local worker: the
    /// dispatchers behind [`Self::standard_tool_specs`].
    pub fn standard_services() -> Vec<Arc<dyn ToolService>> {
        vec![
            Arc::new(BashService::new()) as Arc<dyn ToolService>,
            Arc::new(TextEditorService::new()) as Arc<dyn ToolService>,
            Arc::new(ManualService::new()) as Arc<dyn ToolService>,
        ]
    }

    /// Tool catalog advertised by [`Self::standard`] — pure static data, no
    /// services constructed.
    ///
    /// Used by the harness for routing and by lazy
    /// [`crate::host::HostController`]s to advertise tools before any
    /// connection exists. Concatenates the same per-service `specs()` the
    /// live services serve; a test pins this against a real worker so the
    /// two can never drift.
    pub fn standard_tool_specs() -> Vec<generative_model::ToolSpec> {
        [
            BashService::specs(),
            TextEditorService::specs(),
            ManualService::specs(),
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
    ) -> generative_model::ToolResult {
        let id = tool_use.id.clone();
        let Some(service) = self.tool_to_service.get(&tool_use.name).cloned() else {
            return generative_model::ToolResult::err(format!("unknown tool '{}'", tool_use.name))
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
                    name: self.name.clone(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    tools: self.tool_specs(),
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
            Request::AgentFinished { id, agent_id } => {
                self.notify_agent_finished(agent_id);
                let response = Response::AgentFinishedOk { id, agent_id };
                if let Err(e) = write_locked(&writer, &response).await {
                    eprintln!("host worker: write agent_finished failed: {e}");
                }
            }
        }
    }

    /// Serve until `reader` hits EOF.
    ///
    /// The read loop only waits on the next request line. Hello, tool calls,
    /// agent_finished, and error replies are all spawned and write through
    /// `writer` under a mutex. On EOF, in-flight tasks are joined so results
    /// are flushed before return.
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
    #[test]
    fn standard_catalog_matches_standard_worker() {
        let catalog =
            serde_json::to_value(HostWorker::standard_tool_specs()).expect("catalog json");
        let advertised =
            serde_json::to_value(HostWorker::standard("x").tool_specs()).expect("advertised json");
        assert_eq!(catalog, advertised);
    }
}
