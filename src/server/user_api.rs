//! The server's `MycoApi` half: the same interface `client::HttpClient`
//! speaks over the wire, implemented in-process. [`UserApi`] binds the
//! server to one caller, so nothing here runs without an identity to
//! attribute writes to.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::agent::AgentEvent;
use crate::config::Config;
use crate::models::Effort;
use crate::session::Session;
use myco_api::Author;

use myco_api as api;

use super::*;

use super::room::Accepted;

// ---------------------------------------------------------------------------
// MycoApi: the server object speaks the same interface as the HTTP client
// ---------------------------------------------------------------------------

impl From<SessionEvent> for api::StreamEvent {
    fn from(ev: SessionEvent) -> Self {
        match ev {
            SessionEvent::Agent(a) => match a {
                AgentEvent::TextDelta { text, .. } => api::StreamEvent::TextDelta { text },
                AgentEvent::ThinkingDelta { text, .. } => api::StreamEvent::ThinkingDelta { text },
                AgentEvent::ToolStarted { tool_use, .. } => api::StreamEvent::ToolStarted {
                    id: tool_use.id,
                    name: tool_use.name,
                    input: tool_use.input,
                },
                AgentEvent::ToolFinished { result, .. } => {
                    api::StreamEvent::ToolFinished { result }
                }
                // Not sent: `events()` filters this variant so the wire
                // carries exactly one TurnFinished per turn — the task-level
                // one, emitted after the turn's entries are persisted.
                AgentEvent::TurnFinished { .. } => api::StreamEvent::TurnFinished,
            },
            SessionEvent::Message { entry, wakes_agent } => {
                api::StreamEvent::Message { entry, wakes_agent }
            }
            SessionEvent::TurnStarted => api::StreamEvent::TurnStarted,
            SessionEvent::TurnFailed { message } => api::StreamEvent::TurnFailed { message },
            SessionEvent::TurnFinished => api::StreamEvent::TurnFinished,
            SessionEvent::Compacted { outcome } => api::StreamEvent::Compacted {
                predecessor: outcome.predecessor_id,
                successor: outcome.successor_id,
            },
        }
    }
}

/// Sessions shown in the browser list (most recent first).
const SESSION_LIST_LIMIT: usize = 200;

use myco_api::{ApiError, ErrorKind, MycoApi};

fn internal(e: String) -> ApiError {
    ApiError::new(ErrorKind::Internal, e)
}

fn summary_of(config: &Config, s: &Session, live: bool, busy: bool) -> api::SessionSummary {
    api::SessionSummary {
        archived: s.archived,
        id: s.id.clone(),
        title: s.title.clone(),
        model: s.model.clone(),
        created_at: s.created_at.to_rfc3339(),
        updated_at: s.updated_at.to_rfc3339(),
        message_count: s.entries.len(),
        snippet: String::new(),
        effort: s.effort.clone(),
        context_tokens: s.last_usage.map(|u| u.context_tokens()),
        context_window: config
            .models
            .get(&s.model)
            .ok()
            .map(|m| m.spec.context_window_tokens),
        live,
        busy,
    }
}

impl Server {
    /// The named session's current on-disk truth: live snapshot when
    /// resident, plain load otherwise.
    async fn load_or_snapshot(&self, id: &str) -> Result<Session, ApiError> {
        match self.get_live(id).await {
            Some(l) => Ok(l.session.snapshot()),
            None => {
                Session::load_by_id_or_prefix(id).map_err(|e| ApiError::new(ErrorKind::NotFound, e))
            }
        }
    }
}

/// The API surface, minus identity. [`UserApi`] binds a caller to these and
/// is what implements [`MycoApi`] — so no route can reach the runtime without
/// naming who is asking.
impl Server {
    pub(crate) async fn list_sessions(
        &self,
        include_archived: bool,
    ) -> Result<Vec<api::SessionSummary>, ApiError> {
        let entries =
            crate::session::list_sessions_with(SESSION_LIST_LIMIT, false, include_archived)
                .map_err(internal)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let (live, busy) = self.live_flags(&e.id).await;
            out.push(api::SessionSummary {
                id: e.id,
                title: e.title,
                model: e.model,
                created_at: e.created_at.to_rfc3339(),
                updated_at: e.updated_at.to_rfc3339(),
                message_count: e.message_count,
                snippet: e.snippet,
                archived: e.archived,
                effort: e.effort,
                // The browser list shows no context meter; the conversation
                // view gets these from its own detail fetch.
                context_tokens: None,
                context_window: None,
                live,
                busy,
            });
        }
        Ok(out)
    }

    pub(crate) async fn create_session(
        &self,
        req: api::CreateSession,
    ) -> Result<api::SessionSummary, ApiError> {
        let session = match req.parent_session.as_deref().map(str::trim) {
            None | Some("") => {
                let model_key = req
                    .model
                    .clone()
                    .unwrap_or_else(|| self.config().model.clone());
                self.config().models.get(&model_key).map_err(|e| {
                    ApiError::new(ErrorKind::BadRequest, format!("model {model_key:?}: {e}"))
                })?;
                Session::new(model_key)
            }
            Some(parent) => self
                .new_child(parent, req.model.clone(), req.fork)
                .map_err(|e| ApiError::new(ErrorKind::BadRequest, e))?,
        };
        let id = session.id.clone();
        let live = self
            .ensure_live(&id, Some(session))
            .await
            .map_err(internal)?;
        Ok(summary_of(
            &self.config,
            &live.session.snapshot(),
            true,
            false,
        ))
    }

    pub(crate) async fn session_detail(&self, id: &str) -> Result<api::SessionDetail, ApiError> {
        let session = self.load_or_snapshot(id).await?;
        let (live, busy) = self.live_flags(&session.id).await;
        Ok(api::SessionDetail {
            entries: session.entries.clone(),
            summary: summary_of(&self.config, &session, live, busy),
        })
    }

    pub(crate) async fn update_session(
        &self,
        id: &str,
        req: api::UpdateSession,
    ) -> Result<api::SessionSummary, ApiError> {
        // Validate up front so a bad key or effort is a 400 with the session
        // untouched — the agent task's rebuild should only ever see values
        // that resolved here.
        if let Some(model) = req.model.as_deref() {
            self.config.models.get(model).map_err(|e| {
                ApiError::new(ErrorKind::BadRequest, format!("model {model:?}: {e}"))
            })?;
        }
        // `""` clears the override; anything else must parse. Stored
        // canonically (`"MED"` → `"medium"`) so the document and the summary
        // read the same as the wire.
        let effort = match req.effort.as_deref() {
            None => None,
            Some("") => Some(None),
            Some(s) => Some(Some(
                s.parse::<Effort>()
                    .map_err(|e| ApiError::new(ErrorKind::BadRequest, e))?
                    .as_str()
                    .to_string(),
            )),
        };
        let apply = |s: &mut Session| {
            if let Some(title) = req.title.clone() {
                s.title = Some(title);
            }
            if let Some(archived) = req.archived {
                s.archived = archived;
            }
            if let Some(model) = req.model.clone() {
                s.model = model;
            }
            if let Some(effort) = effort.clone() {
                s.effort = effort;
            }
            s.touch();
            s.save()
        };
        let reconfigures = req.model.is_some() || effort.is_some();

        // Edit the live copy when there is one, so a viewer sees it at once;
        // otherwise edit on disk.
        let session = match self.get_live(id).await {
            Some(l) => {
                l.session.with_mut(apply).map_err(internal)?;
                if reconfigures {
                    // The document changed; tell the agent task to catch up.
                    // Queued behind whatever is running, so the change lands
                    // at the next turn boundary.
                    let _ = l.tx.send(Cmd::Reconfigure);
                }
                l.session.snapshot()
            }
            None => {
                let mut s = Session::load_by_id_or_prefix(id)
                    .map_err(|e| ApiError::new(ErrorKind::NotFound, e))?;
                apply(&mut s).map_err(internal)?;
                s
            }
        };
        let (live, busy) = self.live_flags(&session.id).await;
        Ok(summary_of(&self.config, &session, live, busy))
    }

    pub(crate) async fn post_message(
        &self,
        author: &Author,
        id: &str,
        req: api::PostMessage,
    ) -> Result<api::Poll, ApiError> {
        if req.text.trim().is_empty() {
            return Err(ApiError::new(ErrorKind::BadRequest, "empty message"));
        }
        let live = self
            .ensure_live(id, None)
            .await
            .map_err(|e| ApiError::new(ErrorKind::Conflict, e))?;
        let snapshot = live.session.snapshot();
        let entry = self.compose(author.clone(), &req.text, &snapshot.model);

        // Acceptance is atomic under the room lock: the wake decision reads
        // the room as it is *now* — participants include everyone whose
        // message is still queued behind a running turn, not just what has
        // reached disk — and the feed broadcast and inbox push happen in the
        // same order for every message. The broadcast goes out ahead of any
        // folding, so a message shows up for everyone the instant it is
        // accepted, even mid-turn.
        let wakes_agent = {
            let mut room = live.room.lock().unwrap_or_else(|e| e.into_inner());
            let wakes_agent = room.wakes_agent(&req.text, &snapshot.model, author);
            if let Author::User { id, .. } = author {
                room.participants.insert(id.clone());
            }
            let _ = live.events.send(SessionEvent::Message {
                entry: entry.clone(),
                wakes_agent,
            });
            room.inbox.push_back(Accepted { entry, wakes_agent });
            live.tx
                .send(Cmd::Poke)
                .map_err(|_| internal("agent task gone".into()))?;
            wakes_agent
        };
        Ok(api::Poll {
            // Whether a reply is coming *or already underway* — a room note
            // posted mid-turn must not tell the client the agent went idle.
            busy: wakes_agent || live.is_busy(),
            context_tokens: snapshot.last_usage.map(|u| u.context_tokens()),
            total: snapshot.entries.len(),
            entries: Vec::new(),
            last_error: None,
        })
    }

    pub(crate) async fn poll(&self, id: &str, since: usize) -> Result<api::Poll, ApiError> {
        let session = self.load_or_snapshot(id).await?;
        let (_, busy) = self.live_flags(&session.id).await;
        let last_error = match self.get_live(&session.id).await {
            Some(l) => l.error(),
            None => None,
        };
        let all = session.entries.clone();
        let since = since.min(all.len());
        Ok(api::Poll {
            busy,
            context_tokens: session.last_usage.map(|u| u.context_tokens()),
            total: all.len(),
            entries: all[since..].to_vec(),
            last_error,
        })
    }

    pub(crate) async fn events(&self, id: &str) -> Result<api::EventStream, ApiError> {
        let live = self
            .ensure_live(id, None)
            .await
            .map_err(|e| ApiError::new(ErrorKind::Conflict, e))?;
        let rx = live.subscribe();
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    // The agent emits its own TurnFinished *before* the task
                    // persists the turn's entries; a client that refetched on
                    // it would briefly read a transcript missing the answer.
                    // The wire carries only the task-level one, sent after
                    // persistence.
                    Ok(SessionEvent::Agent(AgentEvent::TurnFinished { .. })) => continue,
                    Ok(ev) => return Some((api::StreamEvent::from(ev), rx)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        Ok(Box::pin(stream))
    }

    pub(crate) async fn cancel(&self, id: &str) -> Result<api::Poll, ApiError> {
        let live = self
            .get_live(id)
            .await
            .ok_or_else(|| ApiError::new(ErrorKind::NotFound, "session not live"))?;
        live.cancel_turn().await;
        let snapshot = live.session.snapshot();
        Ok(api::Poll {
            busy: live.is_busy(),
            context_tokens: snapshot.last_usage.map(|u| u.context_tokens()),
            total: snapshot.entries.len(),
            entries: Vec::new(),
            last_error: live.error(),
        })
    }

    pub(crate) async fn compact(&self, id: &str) -> Result<api::Poll, ApiError> {
        let live = self
            .ensure_live(id, None)
            .await
            .map_err(|e| ApiError::new(ErrorKind::Conflict, e))?;
        live.tx
            .send(Cmd::Compact { automatic: false })
            .map_err(|_| internal("agent task gone".into()))?;
        Ok(api::Poll {
            busy: true,
            context_tokens: None,
            total: 0,
            entries: Vec::new(),
            last_error: None,
        })
    }

    pub(crate) async fn retire(&self, id: &str) -> Result<api::Poll, ApiError> {
        match self.retire_live(id).await {
            Some(_) => Ok(api::Poll {
                busy: false,
                context_tokens: None,
                total: 0,
                entries: Vec::new(),
                last_error: None,
            }),
            None => Err(ApiError::new(ErrorKind::NotFound, "session not live")),
        }
    }

    pub(crate) async fn models(&self) -> Result<api::Models, ApiError> {
        Ok(api::Models {
            models: self
                .config()
                .models
                .keys()
                .into_iter()
                .map(String::from)
                .collect(),
            default_model: self.config().model.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// UserApi
// ---------------------------------------------------------------------------

/// [`Server`] bound to one caller: the [`MycoApi`] implementation frontends
/// actually hold.
///
/// Identity lives on the handle rather than on each call, which is what lets
/// the trait stay identical across the in-process server and `HttpClient`.
/// The web adapter mints one per authenticated request; the CLI mints one for
/// the roster's local user at startup.
#[derive(Clone)]
pub struct UserApi {
    server: Arc<Server>,
    author: Author,
}

impl UserApi {
    pub fn author(&self) -> &Author {
        &self.author
    }

    pub fn server(&self) -> &Arc<Server> {
        &self.server
    }
}

impl Server {
    /// A handle acting as `author`.
    pub fn as_user(self: &Arc<Self>, author: Author) -> UserApi {
        UserApi {
            server: self.clone(),
            author,
        }
    }

    /// A handle acting as the roster's local user — the CLI, and any
    /// in-process caller that is simply this machine's operator.
    pub fn as_local(self: &Arc<Self>) -> UserApi {
        let author = local_author(&self.config);
        self.as_user(author)
    }
}

#[async_trait::async_trait]
impl MycoApi for UserApi {
    async fn list_sessions(
        &self,
        include_archived: bool,
    ) -> Result<Vec<api::SessionSummary>, ApiError> {
        self.server.list_sessions(include_archived).await
    }

    async fn create_session(
        &self,
        req: api::CreateSession,
    ) -> Result<api::SessionSummary, ApiError> {
        self.server.create_session(req).await
    }

    async fn session_detail(&self, id: &str) -> Result<api::SessionDetail, ApiError> {
        self.server.session_detail(id).await
    }

    async fn update_session(
        &self,
        id: &str,
        req: api::UpdateSession,
    ) -> Result<api::SessionSummary, ApiError> {
        self.server.update_session(id, req).await
    }

    async fn post_message(&self, id: &str, req: api::PostMessage) -> Result<api::Poll, ApiError> {
        self.server.post_message(&self.author, id, req).await
    }

    async fn poll(&self, id: &str, since: usize) -> Result<api::Poll, ApiError> {
        self.server.poll(id, since).await
    }

    async fn events(&self, id: &str) -> Result<api::EventStream, ApiError> {
        self.server.events(id).await
    }

    async fn cancel(&self, id: &str) -> Result<api::Poll, ApiError> {
        self.server.cancel(id).await
    }

    async fn compact(&self, id: &str) -> Result<api::Poll, ApiError> {
        self.server.compact(id).await
    }

    async fn retire(&self, id: &str) -> Result<api::Poll, ApiError> {
        self.server.retire(id).await
    }

    async fn models(&self) -> Result<api::Models, ApiError> {
        self.server.models().await
    }

    async fn whoami(&self) -> Result<api::Identity, ApiError> {
        match &self.author {
            Author::User { id, name } => Ok(api::Identity {
                id: id.clone(),
                name: name.clone(),
            }),
            other => Err(internal(format!("handle is not a user: {other:?}"))),
        }
    }

    async fn shells(&self, id: &str) -> Result<api::Shells, ApiError> {
        self.server.shells(id).await
    }

    async fn shell_tail(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        from: u64,
    ) -> Result<api::ShellTailChunk, ApiError> {
        self.server.shell_tail(id, host, shell, from).await
    }

    async fn shell_input(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        data: String,
    ) -> Result<api::Shell, ApiError> {
        self.server
            .shell_input(&self.author, id, host, shell, data)
            .await
    }

    async fn shell_lock(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        lock: api::ShellLockMode,
    ) -> Result<api::Shell, ApiError> {
        self.server
            .shell_lock(&self.author, id, host, shell, lock)
            .await
    }

    async fn shell_screen(
        &self,
        id: &str,
        host: &str,
        shell: &str,
    ) -> Result<api::ShellScreen, ApiError> {
        self.server.shell_screen(id, host, shell).await
    }

    async fn shell_resize(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        cols: u16,
        rows: u16,
    ) -> Result<api::Shell, ApiError> {
        self.server.shell_resize(id, host, shell, cols, rows).await
    }

    async fn shell_start(
        &self,
        id: &str,
        host: &str,
        req: api::CreateShell,
    ) -> Result<api::Shell, ApiError> {
        self.server.shell_start(&self.author, id, host, req).await
    }

    async fn shell_rename(
        &self,
        id: &str,
        host: &str,
        shell: &str,
        title: Option<String>,
    ) -> Result<api::Shell, ApiError> {
        self.server
            .shell_rename(&self.author, id, host, shell, title)
            .await
    }

    async fn subagents(&self, id: &str) -> Result<api::Subagents, ApiError> {
        self.server.subagents(id).await
    }

    async fn subagent_lock(
        &self,
        id: &str,
        child: &str,
        lock: api::ShellLockMode,
    ) -> Result<api::Subagent, ApiError> {
        self.server
            .subagent_lock(&self.author, id, child, lock)
            .await
    }

    async fn subagent_input(
        &self,
        id: &str,
        child: &str,
        text: String,
    ) -> Result<api::Subagent, ApiError> {
        self.server
            .subagent_input(&self.author, id, child, text)
            .await
    }
}
