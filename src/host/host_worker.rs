//! Host worker: serve NDJSON requests over arbitrary async Read/Write.
//!
//! Owns the tool service registry. The read loop never waits on tool work:
//! each request is handled on a background task that writes its response
//! through a shared writer lock.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::core::{CancelToken, ToolResource};
use crate::generative_model::{self, ToolUse};
use crate::host::protocol::{HOST_PROTOCOL_VERSION, Request, Response, read_line};
use crate::tool_services::{
    BashService, HostDispatchContext, SkillsService, TextEditorService, ToolService,
    ViewImageService,
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
    ///
    /// `max_image_base64_bytes` is the catalog ceiling, passed to remote workers
    /// with `--max-image-base64-bytes`. Calls lower it to their active model's
    /// cap without replacing the worker or its live resources.
    pub fn standard(name: impl Into<String>, max_image_base64_bytes: u64) -> Self {
        Self::new(name, Self::standard_services(max_image_base64_bytes))
    }

    /// Standard service list for building an extended local worker: the
    /// dispatchers behind [`Self::standard_tool_specs`].
    pub fn standard_services(max_image_base64_bytes: u64) -> Vec<Arc<dyn ToolService>> {
        vec![
            Arc::new(BashService::new()) as Arc<dyn ToolService>,
            Arc::new(TextEditorService::new()) as Arc<dyn ToolService>,
            Arc::new(ViewImageService::new(max_image_base64_bytes)) as Arc<dyn ToolService>,
            Arc::new(SkillsService::new()) as Arc<dyn ToolService>,
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
            SkillsService::specs(),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    pub fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        self.services
            .iter()
            .flat_map(|service| service.tool_specs())
            .collect()
    }

    /// Dispatch one tool use and attach successful-call notices. Both the
    /// in-process local host and the NDJSON serve loop run calls through here.
    pub async fn dispatch_tool_use(
        self: &Arc<Self>,
        tool_use: ToolUse,
        ctx: HostDispatchContext,
    ) -> generative_model::ToolResult {
        let Some(service) = self.tool_to_service.get(&tool_use.name).cloned() else {
            return generative_model::ToolResult::err(format!("unknown tool '{}'", tool_use.name));
        };
        let mut result = service
            .dispatch_tool_use(tool_use.clone(), ctx.clone())
            .await;
        if !result.is_error {
            for observer in &self.services {
                if let Some(notice) = observer
                    .clone()
                    .observe_successful_call(tool_use.clone(), ctx.clone())
                    .await
                {
                    result.content.push(generative_model::Content::System {
                        kind: "host_observation".into(),
                        text: format!("[myco: host {:?}] {notice}", self.name),
                        data: serde_json::json!({"host": self.name}),
                    });
                }
            }
        }
        result
    }

    pub fn notify_agent_finished(&self, agent_id: uuid::Uuid) {
        for service in &self.services {
            service.on_agent_finished(agent_id);
        }
    }

    pub fn resources(&self, agent_id: uuid::Uuid) -> Vec<ToolResource> {
        let mut resources: Vec<_> = self
            .services
            .iter()
            .flat_map(|service| service.resources(agent_id))
            .collect();
        resources.sort_by(|a, b| (&a.tool, &a.id).cmp(&(&b.tool, &b.id)));
        resources
    }

    /// Serve until `reader` ends: EOF or any read error.
    ///
    /// The read loop only waits on the next request line. Each request is
    /// handled on a spawned task; hello, tool-result, and error replies write
    /// through `writer` under a mutex. When input ends, in-flight calls are
    /// cancelled and joined so results are flushed before return.
    pub async fn serve<R, W>(self: &Arc<Self>, reader: R, writer: W)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: AsyncWriteExt + Unpin + Send + 'static,
    {
        let mut lines = BufReader::new(reader);
        let writer = Arc::new(Mutex::new(writer));
        let mut in_flight: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        let controls: Arc<Mutex<HashMap<String, HostDispatchContext>>> =
            Arc::new(Mutex::new(HashMap::new()));

        self.serve_requests(&mut lines, &writer, &controls, &mut in_flight)
            .await;

        // The controller is gone. Cancel host work before joining so a lost
        // SSH connection cannot leave a command running until its own
        // (possibly much longer) timeout.
        for context in controls.lock().await.values() {
            context.cancel.cancel();
        }
        while in_flight.join_next().await.is_some() {}
    }

    async fn serve_requests<R, W>(
        self: &Arc<Self>,
        lines: &mut BufReader<R>,
        writer: &Arc<Mutex<W>>,
        controls: &Arc<Mutex<HashMap<String, HostDispatchContext>>>,
        in_flight: &mut tokio::task::JoinSet<()>,
    ) where
        R: tokio::io::AsyncRead + Unpin,
        W: AsyncWriteExt + Unpin + Send + 'static,
    {
        loop {
            // Reap between reads: cancelling a partial read_line can lose protocol bytes.
            while in_flight.try_join_next().is_some() {}
            let line = match read_line(lines).await {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("host worker: input failed, shutting down: {e}");
                    break;
                }
            };

            let msg = match Request::decode(&line) {
                Ok(m) => m,
                Err(e) => {
                    let writer = Arc::clone(writer);
                    let error = Response::Error {
                        id: None,
                        message: format!("invalid request: {e}"),
                    };
                    in_flight.spawn(async move { reply(&writer, &error, "request error").await });
                    continue;
                }
            };

            // Everything but cancel and background runs on its own task: replies
            // can block on a full pipe, and service state (inventory, owner
            // cleanup) may be locked by a running call.
            let worker = Arc::clone(self);
            let writer = Arc::clone(writer);
            match msg {
                Request::ToolCall {
                    id,
                    agent_id,
                    max_image_base64_bytes,
                    thread_id,
                    tool_use,
                } => {
                    // Register before spawning so a following Cancel line can
                    // never race ahead of the tool task's token.
                    let mut context = HostDispatchContext::new(agent_id, CancelToken::new());
                    context.max_image_base64_bytes = max_image_base64_bytes;
                    context.thread_id = thread_id;
                    controls.lock().await.insert(id.clone(), context.clone());
                    let controls = Arc::clone(controls);
                    in_flight.spawn(async move {
                        let result = worker.dispatch_tool_use(tool_use, context).await;
                        let response = Response::ToolResult {
                            id: id.clone(),
                            result,
                        };
                        reply(&writer, &response, "tool result").await;
                        controls.lock().await.remove(&id);
                    });
                }
                Request::Cancel { id } => {
                    if let Some(context) = controls.lock().await.get(&id) {
                        context.cancel.cancel();
                    }
                }
                Request::Background { id } => {
                    if let Some(context) = controls.lock().await.get(&id) {
                        context.background.cancel();
                    }
                }
                Request::Hello => {
                    let hello = Response::HelloOk {
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        protocol: HOST_PROTOCOL_VERSION,
                    };
                    in_flight.spawn(async move { reply(&writer, &hello, "hello").await });
                }
                Request::Resources { id, agent_id } => {
                    in_flight.spawn(async move {
                        let response = Response::Resources {
                            id,
                            resources: worker.resources(agent_id),
                        };
                        reply(&writer, &response, "resource inventory").await;
                    });
                }
                // Fire-and-forget: no reply message exists for this request.
                Request::AgentFinished { agent_id } => {
                    in_flight.spawn(async move { worker.notify_agent_finished(agent_id) });
                }
            }
        }
    }

    /// Serve process stdin/stdout (`myco --mode host`).
    pub async fn serve_stdio(self) {
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

/// Lock the shared writer and send one response (concurrent tasks serialize
/// here). A failed write is only logged: the controller is gone or going.
async fn reply<W>(writer: &Arc<Mutex<W>>, response: &Response, what: &str)
where
    W: AsyncWriteExt + Unpin,
{
    let mut guard = writer.lock().await;
    if let Err(e) = response.write_to(&mut *guard).await {
        eprintln!("host worker: write {what} failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;

    #[tokio::test]
    async fn completed_requests_are_reaped_before_peer_disconnects() {
        let worker = Arc::new(HostWorker::new("test", Vec::new()));
        let (mut client, server) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(server);
        let mut reader = BufReader::new(reader);
        let writer = Arc::new(Mutex::new(writer));
        let controls = Arc::new(Mutex::new(HashMap::new()));
        let mut in_flight = tokio::task::JoinSet::new();

        let exchange = async {
            let mut replies = BufReader::new(&mut client);
            for index in 0..8 {
                let request = Request::ToolCall {
                    id: index.to_string(),
                    agent_id: uuid::Uuid::nil(),
                    max_image_base64_bytes: None,
                    thread_id: None,
                    tool_use: ToolUse {
                        name: "unknown".into(),
                        input: serde_json::json!({}),
                        thought_signature: None,
                    },
                };
                replies
                    .get_mut()
                    .write_all(&request.encode().unwrap())
                    .await
                    .unwrap();
                let mut reply = String::new();
                replies.read_line(&mut reply).await.unwrap();
                assert!(matches!(
                    Response::decode(&reply).unwrap(),
                    Response::ToolResult { id, .. } if id == index.to_string()
                ));
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                _ = worker.serve_requests(&mut reader, &writer, &controls, &mut in_flight) => {
                    panic!("worker stopped before the client disconnected");
                }
                _ = exchange => {}
            }
        })
        .await
        .expect("worker stopped responding");

        assert!(
            in_flight.len() <= 1,
            "completed requests retained: {}",
            in_flight.len()
        );
    }

    /// Waits for its call's cancel signal and records that it arrived.
    struct AwaitCancel(Arc<std::sync::atomic::AtomicBool>);

    impl ToolService for AwaitCancel {
        fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
            vec![generative_model::ToolSpec {
                name: "await_cancel".into(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
            }]
        }

        fn dispatch_tool_use(
            self: Arc<Self>,
            _tool_use: ToolUse,
            ctx: HostDispatchContext,
        ) -> crate::core::Async<generative_model::ToolResult> {
            Box::pin(async move {
                ctx.cancel.cancelled().await;
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                generative_model::ToolResult::err("cancelled")
            })
        }
    }

    /// A broken input stream ends the controller just as EOF does, so it must
    /// also cancel and join in-flight work rather than abandoning it.
    #[tokio::test]
    async fn read_error_cancels_and_joins_in_flight_calls() {
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = Arc::new(HostWorker::new(
            "test",
            vec![Arc::new(AwaitCancel(cancelled.clone())) as Arc<dyn ToolService>],
        ));
        let call = Request::ToolCall {
            id: "1".into(),
            agent_id: uuid::Uuid::nil(),
            max_image_base64_bytes: None,
            thread_id: None,
            tool_use: ToolUse {
                name: "await_cancel".into(),
                input: serde_json::json!({}),
                thought_signature: None,
            },
        };
        let mut input = call.encode().unwrap();
        input.extend_from_slice(b"\xff\n");

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            worker.serve(std::io::Cursor::new(input), tokio::io::sink()),
        )
        .await
        .expect("worker hung after a read error");

        assert!(
            cancelled.load(std::sync::atomic::Ordering::SeqCst),
            "in-flight call was abandoned instead of cancelled"
        );
    }

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
